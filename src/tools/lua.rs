use crate::agent::Agent;
use crate::context::{Context, Message, Role, TIME_FORMAT};
use crate::tools::{Error, Tool};
use mlua::{
    LuaOptions, MultiValue, StdLib, UserData, UserDataFields, UserDataMethods, Value, VmState,
};
use schemars::JsonSchema;
use serde::Deserialize;
use std::time::{Duration, Instant};

/// How long one run may take before it's stopped.
const TIME_LIMIT: Duration = Duration::from_secs(10);
/// How much memory a session's scripts may hold, all runs together.
const MEMORY_LIMIT: usize = 64 << 20;
/// How much of a run's output goes back, so a runaway `print` can't flood
/// the conversation.
const OUTPUT_LIMIT: usize = 16 << 10;

/// Run Luau code. Globals persist between calls, so you can build things up
/// over several; locals don't. Code that is a single expression returns its
/// value, like a REPL: `1 + 1` gives `2`. `print` output comes back too.
///
/// The runtime is sandboxed: it has the `string`, `table`, `math`, `utf8`,
/// `bit32` and `buffer` libraries, and nothing that reaches files, processes
/// or the network. Each call may run for 10 seconds.
///
/// The global `context` is your own conversation, read-only, as it is right
/// now, including this call's message:
///
/// - `#context`: how many messages there are.
/// - `context:message(i)`: the message at 1-based index `i`, as a table with
///   `id`, `role` (`"user"`, `"assistant"` or `"agent"`), `from` (the agent's
///   name, for `"agent"`), `content`, `time`, and `calls`, a list of tools it
///   called with their `name`, `arguments` and `result`.
/// - `context:messages()`: all of them, as a list.
/// - `context:find(id)`: the index of the message with that ID, or nil.
#[derive(Deserialize, JsonSchema)]
pub struct Args {
    /// The Luau code to run.
    code: String,
}

/// Lets an agent run Luau in a sandbox of its own that lasts until a clear.
pub struct Lua;

impl Tool for Lua {
    const NAME: &'static str = "lua";
    type Args = Args;

    async fn call(&self, args: Args, agent: &mut Agent) -> Result<String, Error> {
        let lua = match &agent.lua {
            Some(lua) => lua,
            None => agent.lua.insert(start()?),
        };
        run(lua, &args.code, &agent.context)
    }
}

/// When the current run must stop.
struct Deadline(Instant);

/// What the current run printed.
#[derive(Default)]
struct Printed(String);

/// A new sandboxed runtime for one agent.
fn start() -> Result<mlua::Lua, Error> {
    let libs =
        StdLib::STRING | StdLib::TABLE | StdLib::MATH | StdLib::UTF8 | StdLib::BIT | StdLib::BUFFER;
    let lua = mlua::Lua::new_with(libs, LuaOptions::default())?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    lua.set_interrupt(|lua| match lua.app_data_ref::<Deadline>() {
        Some(deadline) if Instant::now() > deadline.0 => Err(mlua::Error::runtime(format!(
            "stopped after {} seconds",
            TIME_LIMIT.as_secs()
        ))),
        _ => Ok(VmState::Continue),
    });
    let print = lua.create_function(|lua, values: MultiValue| {
        let line: Vec<String> = values.iter().map(show).collect();
        if let Some(mut printed) = lua.app_data_mut::<Printed>() {
            printed.0 += &line.join("\t");
            printed.0.push('\n');
        }
        Ok(())
    })?;
    lua.globals().set("print", print)?;
    // From here on the built-in globals are read-only, and what scripts
    // set goes to a table of their own that persists between runs.
    lua.sandbox(true)?;
    Ok(lua)
}

/// Run `code` with `context` lent to it, and return what it printed and
/// returned.
fn run(lua: &mlua::Lua, code: &str, context: &Context) -> Result<String, Error> {
    lua.set_app_data(Deadline(Instant::now() + TIME_LIMIT));
    lua.set_app_data(Printed::default());
    // The context is lent for this run only; a script that keeps it in a
    // global finds it gone in the next.
    let result = lua.scope(|scope| {
        lua.globals()
            .set("context", scope.create_userdata_ref(context)?)?;
        lua.load(code).set_name("=code").eval::<MultiValue>()
    });
    lua.remove_app_data::<Deadline>();
    let printed = lua.remove_app_data::<Printed>().unwrap_or_default().0;
    let values = result?;

    let mut output = printed;
    if !values.is_empty() {
        let values: Vec<String> = values.iter().map(show).collect();
        output += &values.join("\t");
    }
    let output = output.trim_end();
    Ok(match output {
        "" => "(no output)".into(),
        _ => truncate(output),
    })
}

impl UserData for Context {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "Context");
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__len", |_, context, ()| Ok(context.messages().len()));
        methods.add_method("message", |lua, context, index: usize| {
            match index.checked_sub(1).and_then(|i| context.messages().get(i)) {
                Some(message) => Ok(Value::Table(table(lua, message)?)),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("messages", |lua, context, ()| {
            let messages = context.messages().iter().map(|m| table(lua, m));
            lua.create_sequence_from(messages.collect::<mlua::Result<Vec<_>>>()?)
        });
        methods.add_method("find", |_, context, id: String| {
            let index = context
                .messages()
                .iter()
                .position(|m| m.id.eq_ignore_ascii_case(&id));
            Ok(index.map(|i| i + 1))
        });
    }
}

/// A message as a script sees it.
fn table(lua: &mlua::Lua, message: &Message) -> mlua::Result<mlua::Table> {
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

/// A value as `print` and results show it: strings as they are, tables
/// spelled out, anything else as `tostring` would.
fn show(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_string_lossy(),
        Value::Table(_) => format!("{value:#?}"),
        _ => value.to_string().unwrap_or_else(|_| format!("{value:?}")),
    }
}

fn truncate(output: &str) -> String {
    if output.len() <= OUTPUT_LIMIT {
        return output.into();
    }
    let end = output.floor_char_boundary(OUTPUT_LIMIT);
    format!(
        "{}\n... ({} more bytes cut)",
        &output[..end],
        output.len() - end
    )
}
