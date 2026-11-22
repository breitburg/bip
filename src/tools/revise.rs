use crate::context::{Context, Message, Role};
use crate::tools::Outcome;
use async_openai::types::chat::ChatCompletionMessageToolCalls;
use schemars::JsonSchema;
use serde::Deserialize;
use std::error::Error;

/// Revise the conversation: remove or rewrite earlier messages with `edits`, then continue
/// the conversation from right after `resume_from`. Everything after that
/// message is dropped, including the request that led to this call and this
/// call itself, so the conversation reads as if it had always been this way.
///
/// If `resume_from` is a user message, you reply to it afresh. If it's one of
/// your own messages, the turn ends there. When the person asks you to change
/// the conversation, apply the change and resume from the message just before
/// their request.
///
/// Every message except tool results starts with a `<metadata>` tag holding
/// its 6-character ID and when it arrived; refer to messages by that ID.
/// Removing a message that called tools also removes the tools' results. The
/// current turn can't be edited. Edits apply in order, and if any fails,
/// nothing changes.
#[derive(Deserialize, JsonSchema)]
pub struct Revise {
    /// Changes to make, in order. May be empty, to only rewind.
    edits: Vec<Operation>,
    /// The ID of an existing message to continue from, after the edits. It
    /// can't be the person's latest message unless `edits` rewrites it.
    resume_from: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Operation {
    /// Delete a message.
    Remove { id: String },
    /// Change a message's text with one or more search/replace blocks:
    ///
    /// <<<<<<< SEARCH
    /// exact text to find
    /// =======
    /// text to put in its place
    /// >>>>>>> REPLACE
    ///
    /// Each SEARCH text must appear exactly once in the message, as it is
    /// after the blocks before it. An empty REPLACE deletes the text.
    Rewrite { id: String, blocks: String },
}

pub fn revise(args: Revise, context: &mut Context) -> Result<Outcome, Box<dyn Error>> {
    let messages = context.messages();
    let current = context.current.as_deref();

    // Check the resume point before touching anything.
    let resume_index = find(messages, current, &args.resume_from)?;
    let resume_from = messages[resume_index].id.clone();
    // The person's latest message is what led to this call. Resuming from it
    // unchanged would only replay the same request, so it's allowed only when
    // this call rewrites it.
    let request = messages[..current
        .and_then(|id| messages.iter().position(|m| m.id == id))
        .unwrap_or(messages.len())]
        .iter()
        .rposition(|m| matches!(m.role, Role::User));
    let rewrites_request = args.edits.iter().any(|operation| {
        matches!(operation, Operation::Rewrite { id, .. }
            if find(messages, current, id).ok() == Some(resume_index))
    });
    if request == Some(resume_index) && !rewrites_request {
        return Err(format!(
            "{resume_from} is the request that led to this call; resuming from it unchanged would repeat it. Resume from the message before it, or rewrite it in `edits`."
        )
        .into());
    }

    // Work on a copy, so a failing edit leaves the real context untouched and
    // a successful one is published as a single change.
    let mut draft = messages.to_vec();
    let mut summary = Vec::new();

    for operation in args.edits {
        summary.push(match operation {
            Operation::Remove { id } => {
                let index = find(&draft, current, &id)?;
                let removed = draft.remove(index);
                // A tool result can't outlive the call it answers, so removing
                // an assistant message also removes the results of its calls.
                let call_ids: Vec<&str> = removed
                    .tool_calls
                    .iter()
                    .filter_map(|call| match call {
                        ChatCompletionMessageToolCalls::Function(call) => Some(call.id.as_str()),
                        _ => None,
                    })
                    .collect();
                let mut ids = vec![removed.id.clone()];
                while let Some(index) = draft.iter().position(|m| {
                    matches!(&m.role, Role::Tool(call_id) if call_ids.contains(&call_id.as_str()))
                }) {
                    ids.push(draft.remove(index).id);
                }
                format!("removed {}", ids.join(", "))
            }
            Operation::Rewrite { id, blocks } => {
                let index = find(&draft, current, &id)?;
                let message = &mut draft[index];
                for block in blocks.split(">>>>>>> REPLACE").filter(|b| !b.trim().is_empty()) {
                    let Some((search, replace)) = block
                        .split_once("<<<<<<< SEARCH\n")
                        .and_then(|(_, rest)| rest.split_once("\n=======\n"))
                    else {
                        return Err("each block needs `<<<<<<< SEARCH`, `=======` and `>>>>>>> REPLACE` on their own lines".into());
                    };
                    // The REPLACE marker sits on its own line; drop that line break.
                    let replace = replace.strip_suffix('\n').unwrap_or(replace);
                    match message.content.matches(search).count() {
                        1 => message.content = message.content.replacen(search, replace, 1),
                        0 => return Err(format!("{}: SEARCH text not found: {search:?}", message.id).into()),
                        n => return Err(format!("{}: SEARCH text appears {n} times, add context to make it unique: {search:?}", message.id).into()),
                    }
                }
                format!("rewrote {}", message.id)
            }
        });
    }

    if !draft.iter().any(|m| m.id == resume_from) {
        return Err(format!("these edits remove {resume_from}, the message to resume from").into());
    }

    context.update(|messages| *messages = draft);
    summary.push(format!("resuming from {resume_from}"));
    Ok(Outcome {
        text: format!("Done: {}.", summary.join(", ")),
        resume_from: Some(resume_from),
    })
}

/// Find the message the model means by `reference` in `messages`, if it may be
/// edited: messages from `current` on are the current turn. Models are sloppy,
/// so this accepts `<AB12CD>`, `[ab12cd]`, `id="AB12CD"`, and so on: the first
/// run of exactly 6 letters or digits, ignoring case.
fn find(messages: &[Message], current: Option<&str>, reference: &str) -> Result<usize, String> {
    let wanted = reference
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|part| part.len() == 6)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let index = messages
        .iter()
        .position(|m| m.id == wanted)
        .ok_or_else(|| format!("no message with ID {reference:?}"))?;
    // Tool results have no visible ID, so the model can only have guessed it.
    if matches!(messages[index].role, Role::Tool(_)) {
        return Err(format!("no message with ID {reference:?}"));
    }
    let current = current.and_then(|id| messages.iter().position(|m| m.id == id));
    if current.is_some_and(|current| index >= current) {
        return Err(format!(
            "{wanted} is part of the current turn and can't be edited"
        ));
    }
    Ok(index)
}
