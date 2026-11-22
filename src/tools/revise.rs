use crate::context::{Context, Message, Role, frozen_from};
use crate::agent::Agent;
use crate::tools::{Error, Tool};
use schemars::JsonSchema;
use serde::Deserialize;

/// Revise the conversation: remove or rewrite earlier messages with `edits`, then continue
/// the conversation from right after `resume_from`. Everything after that
/// message is dropped, including the request that led to this call and this
/// call itself, so the conversation reads as if it had always been this way.
///
/// If `resume_from` is a message to you, from the person or another agent,
/// you reply to it afresh. If it's one of
/// your own messages, the turn ends there, unless that message called tools:
/// then its results stay and you go on after them. When the person asks you to change
/// the conversation, apply the change and resume from the message just before
/// their request.
///
/// Every message except tool results starts with a `<metadata>` tag holding
/// its 6-character ID and when it arrived; refer to messages by that ID.
/// Removing a message that called tools also removes the tools' results. The
/// current turn can't be edited. Edits apply in order, and if any fails,
/// nothing changes.
#[derive(Deserialize, JsonSchema)]
pub struct Args {
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

/// Lets an agent edit and rewind its own conversation.
pub struct Revise;

impl Tool for Revise {
    const NAME: &'static str = "revise";
    type Args = Args;

    async fn call(&self, args: Args, agent: &mut Agent) -> Result<String, Error> {
        revise(args, &mut agent.context)
    }
}

fn revise(args: Args, context: &mut Context) -> Result<String, Error> {
    let messages = context.messages();

    // Check the resume point before touching anything.
    let resume_index = find(messages, &args.resume_from)?;
    let resume_from = messages[resume_index].id.clone();
    // The latest message to this agent, from the person or another agent, is
    // what led to this call. Resuming from it unchanged would only replay the
    // same request, so it's allowed only when this call rewrites it.
    let request = messages[..frozen_from(messages).unwrap_or(messages.len())]
        .iter()
        .rposition(|m| m.role != Role::Assistant);
    let rewrites_request = args.edits.iter().any(|operation| {
        matches!(operation, Operation::Rewrite { id, .. }
            if find(messages, id).ok() == Some(resume_index))
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
                let index = find(&draft, &id)?;
                format!("removed {}", draft.remove(index).id)
            }
            Operation::Rewrite { id, blocks } => {
                let index = find(&draft, &id)?;
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

    // Rewind: drop everything after the resume point. That includes the reply
    // making this call, so the result below has no call left to belong to.
    let Some(index) = draft.iter().position(|m| m.id == resume_from) else {
        return Err(format!("these edits remove {resume_from}, the message to resume from").into());
    };
    draft.truncate(index + 1);
    context.replace(draft);
    summary.push(format!("resumed from {resume_from}"));
    Ok(format!("Done: {}.", summary.join(", ")))
}

/// Find the message the model means by `reference` in `messages`, if it may be
/// edited. Models are sloppy, so this accepts `<AB12CD>`, `[ab12cd]`,
/// `id="AB12CD"`, and so on: the first run of exactly 6 letters or digits,
/// ignoring case.
fn find(messages: &[Message], reference: &str) -> Result<usize, String> {
    let wanted = reference
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|part| part.len() == 6)
        .unwrap_or_default()
        .to_ascii_uppercase();
    let index = messages
        .iter()
        .position(|m| m.id == wanted)
        .ok_or_else(|| format!("no message with ID {reference:?}"))?;
    if frozen_from(messages).is_some_and(|frozen| index >= frozen) {
        return Err(format!(
            "{wanted} is part of the current turn and can't be edited"
        ));
    }
    Ok(index)
}
