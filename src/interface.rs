pub mod agents;
pub mod context;

use crate::agent::Agent;
use mlua::{AnyUserData, FromLuaMulti, Function, IntoLuaMulti, Lua};
use std::sync::Arc;

/// The error type everywhere: it can cross threads, so tasks can hold it.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// The registry key under which a run finds the agent whose script it is.
/// The agent is lent for the run only: once it ends, the key is empty.
pub const AGENT: &str = "agent";

/// Makes one of an interface's functions in a new runtime.
type Install = Arc<dyn Fn(&Lua) -> mlua::Result<Function> + Send + Sync>;

/// Something an agent's scripts can use: a global table of functions that
/// reach outside the sandbox, and what the model reads about them.
///
/// ```ignore
/// Interface::new("clock", "- `clock.now()`: the time, as a string.")
///     .function("now", |_, _, ()| Ok(Utc::now().to_string()))
/// ```
#[derive(Clone)]
pub struct Interface {
    /// The global's name: scripts call `name.function(...)`.
    pub name: &'static str,
    /// How to use it, written for the model. It goes into the `lua` tool's
    /// description.
    pub docs: &'static str,
    functions: Vec<(&'static str, Install)>,
}

impl Interface {
    pub fn new(name: &'static str, docs: &'static str) -> Self {
        Self {
            name,
            docs,
            functions: Vec::new(),
        }
    }

    /// Add a function, called as `interface.name(...)`. It gets the runtime,
    /// the agent whose script called it, and the arguments. Arguments and
    /// results convert as mlua converts them: `()` for none, a tuple for
    /// several. An error becomes a Lua error, which a script can catch with
    /// `pcall`.
    pub fn function<A, R, F>(mut self, name: &'static str, function: F) -> Self
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut Agent, A) -> Result<R, Error> + Send + Sync + 'static,
    {
        let function = Arc::new(function);
        let install = move |lua: &Lua| {
            let function = function.clone();
            lua.create_function(move |lua, arguments: A| {
                let agent: AnyUserData = lua.named_registry_value(AGENT)?;
                agent
                    .borrow_mut_scoped(|agent: &mut Agent| function(lua, agent, arguments))?
                    .map_err(mlua::Error::external)
            })
        };
        self.functions.push((name, Arc::new(install)));
        self
    }

    /// Make the interface a global of `lua`. Scripts can't change it.
    pub fn install(&self, lua: &Lua) -> mlua::Result<()> {
        let table = lua.create_table()?;
        for (name, install) in &self.functions {
            table.set(*name, install(lua)?)?;
        }
        table.set_readonly(true);
        lua.globals().set(self.name, table)
    }
}
