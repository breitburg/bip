use crate::context::{self, Message, Role, TIME_FORMAT, frozen_from};
use crate::interface::{Error, Functions, Interface};
use mlua::{Lua, LuaSerdeExt, Table, Value};
use serde::Deserialize;

const DOCS: &str = "\
`context` is your own conversation, as it is right now, including this
call's message. Every message except tool results starts with a `<metadata>`
tag holding its 6-character ID and when it arrived; refer to messages by
that ID.

- `context.count()`: how many messages there are.
- `context.message(i)`: the message at 1-based index `i`, as a table with
  `id`, `role` (`\"user\"`, `\"assistant\"` or `\"agent\"`), `from` (the
  agent's name, for `\"agent\"`), `content`, `time`, and `calls`, a list of
  tools it called with their `name`, `arguments` and `result`.
- `context.messages()`: all of them, as a list.
- `context.find(id)`: the index of the message with that ID, or nil.
- `context.revise{edits = {...}, resume_from = id}`: remove or rewrite
  earlier messages, then continue the conversation from right after the
  message `resume_from`. Everything after that message is dropped, including
  the request that led to this call and this call itself, so the
  conversation reads as if it had always been this way. Nothing the script
  does after `revise` is kept, so call it last.

  `edits` is a list, applied in order, and may be empty, to only rewind. Each
  edit is `{action = \"remove\", id = id}`, which deletes a message, or
  `{action = \"rewrite\", id = id, blocks = text}`, which changes a
  message's text with one or more search/replace blocks:

  <<<<<<< SEARCH
  exact text to find
  =======
  text to put in its place
  >>>>>>> REPLACE

  Each SEARCH text must appear exactly once in the message, as it is after
  the blocks before it. An empty REPLACE deletes the text. If any edit
  fails, nothing changes.

  If `resume_from` is a message to you, from the person or another agent,
  you reply to it afresh; it can't be the latest such message unless `edits`
  rewrites it. If it's one of your own messages, the turn ends there, unless
  that message called tools: then its results stay and you go on after them.
  When the person asks you to change the conversation, apply the change and
  resume from the message just before their request. Removing a message that
  called tools also removes the tools' results. The current turn can't be
  edited.";

/// Lets an agent read its own conversation, and edit and rewind it.
pub struct Context;

impl Interface for Context {
    fn name(&self) -> &'static str {
        "context"
    }

    fn docs(&self) -> &'static str {
        DOCS
    }

    fn functions(&self, functions: &mut Functions) {
        functions
            .add("count", |_, agent, ()| Ok(agent.context.messages().len()))
            .add("message", |lua, agent, index: usize| {
                let message = index
                    .checked_sub(1)
                    .and_then(|i| agent.context.messages().get(i));
                Ok(message.map(|m| table(lua, m)).transpose()?)
            })
            .add("messages", |lua, agent, ()| {
                let messages = agent.context.messages().iter().map(|m| table(lua, m));
                Ok(lua.create_sequence_from(messages.collect::<mlua::Result<Vec<_>>>()?)?)
            })
            .add("find", |_, agent, id: String| {
                let mut messages = agent.context.messages().iter();
                Ok(messages
                    .position(|m| m.id.eq_ignore_ascii_case(&id))
                    .map(|i| i + 1))
            })
            .add("revise", |lua, agent, args: Value| {
                revise(lua.from_value(args)?, &mut agent.context)
            });
    }
}

/// A message as a script sees it.
fn table(lua: &Lua, message: &Message) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("id", message.id.as_str())?;
    let (role, from) = match &message.role {
        Role::User => ("user", None),
        Role::Assistant => ("assistant", None),
        Role::Agent(name) => ("agent", Some(name.as_str())),
    };
    table.set("role", role)?;
    table.set("from", from)?;
    table.set("content", message.content.as_str())?;
    table.set("time", message.time.format(TIME_FORMAT).to_string())?;
    let calls = message.tool_calls.iter().map(|call| {
        let entry = lua.create_table()?;
        entry.set("name", call.name.as_str())?;
        entry.set("arguments", call.arguments.as_str())?;
        entry.set("result", call.result.as_deref())?;
        Ok(entry)
    });
    table.set(
        "calls",
        lua.create_sequence_from(calls.collect::<mlua::Result<Vec<_>>>()?)?,
    )?;
    Ok(table)
}

/// What `context.revise` takes.
#[derive(Deserialize)]
pub struct Args {
    /// Changes to make, in order. May be empty, to only rewind.
    #[serde(default)]
    edits: Vec<Operation>,
    /// The ID of an existing message to continue from, after the edits. It
    /// can't be the person's latest message unless `edits` rewrites it.
    resume_from: String,
}

#[derive(Deserialize)]
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

fn revise(args: Args, context: &mut context::Context) -> Result<String, Error> {
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
