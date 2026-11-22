use crate::agent::Agent;
use crate::interface::{AGENT, Error, Interface};
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use mlua::{LuaOptions, MultiValue, StdLib, Value, VmState};
use serde::Deserialize;
use serde_json::json;
use std::time::{Duration, Instant};

/// How long one run may take before it's stopped.
const TIME_LIMIT: Duration = Duration::from_secs(10);
/// How much memory a session's scripts may hold, all runs together.
const MEMORY_LIMIT: usize = 64 << 20;
/// How much of a run's output goes back, so a runaway `print` can't flood
/// the conversation.
const OUTPUT_LIMIT: usize = 16 << 10;

/// The one tool every agent has.
pub const NAME: &str = "lua";

const DOCS: &str = "\
Run Luau code. Globals persist between calls, so you can build things up over
several; locals don't. Code that is a single expression returns its value,
like a REPL: `1 + 1` gives `2`. `print` output comes back too.

The runtime is sandboxed: it has the `string`, `table`, `math`, `utf8`,
`bit32` and `buffer` libraries, and nothing that reaches files, processes or
the network. Each call may run for 10 seconds.";

/// What the model fills in.
#[derive(Deserialize)]
struct Args {
    code: String,
}

/// The tool as a model request lists it: how to run Luau, then how to use
/// each of `interfaces`.
pub fn definition(interfaces: &[Interface]) -> ChatCompletionTools {
    let mut description = DOCS.to_string();
    if !interfaces.is_empty() {
        description += "\n\nEverything else you can do is through these globals.";
    }
    for interface in interfaces {
        description += "\n\n";
        description += interface.docs;
    }
    let function = FunctionObject {
        name: NAME.into(),
        description: Some(description),
        parameters: Some(json!({
            "type": "object",
            "properties": {"code": {"type": "string", "description": "The Luau code to run."}},
            "required": ["code"],
        })),
        strict: None,
    };
    ChatCompletionTools::Function(ChatCompletionTool { function })
}

/// Run the code in the model's raw JSON `arguments` on `agent`, in a runtime
/// of its own that lasts until a clear. The model reads the result either
/// way, so a failure comes back as text too.
pub fn call(agent: &mut Agent, arguments: &str) -> String {
    let result = (|| {
        let Args { code } = serde_json::from_str(arguments)?;
        // The runtime leaves the agent for the run, so the script can borrow
        // the whole agent.
        let lua = match agent.lua.take() {
            Some(lua) => lua,
            None => start(&agent.interfaces)?,
        };
        let result = run(&lua, &code, agent);
        agent.lua = Some(lua);
        result
    })();
    result.unwrap_or_else(|e: Error| format!("error: {e}"))
}

/// When the current run must stop.
struct Deadline(Instant);

/// What the current run printed.
#[derive(Default)]
struct Printed(String);

/// A new sandboxed runtime for one agent, with its `interfaces` as globals.
fn start(interfaces: &[Interface]) -> Result<mlua::Lua, Error> {
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
    for interface in interfaces {
        interface.install(&lua)?;
    }
    // From here on the built-in globals are read-only, and what scripts
    // set goes to a table of their own that persists between runs.
    lua.sandbox(true)?;
    Ok(lua)
}

/// Run `code` with `agent` lent to it, for its interfaces to act on, and
/// return what it printed and returned.
fn run(lua: &mlua::Lua, code: &str, agent: &mut Agent) -> Result<String, Error> {
    lua.set_app_data(Deadline(Instant::now() + TIME_LIMIT));
    lua.set_app_data(Printed::default());
    // The agent is lent for this run only: when the scope ends, so does the
    // interfaces' way to it.
    let result = lua.scope(|scope| {
        lua.set_named_registry_value(AGENT, scope.create_any_userdata_ref_mut(agent)?)?;
        lua.load(code).set_name("=code").eval::<MultiValue>()
    });
    lua.unset_named_registry_value(AGENT)?;
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
