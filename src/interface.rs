pub mod agents;
pub mod context;

pub use agents::Agents;
pub use context::Context;

use crate::agent::Agent;
use mlua::{AnyUserData, FromLuaMulti, IntoLuaMulti, Lua, Table};

/// The error type everywhere: it can cross threads, so tasks can hold it.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// The registry key under which a run finds the agent whose script it is.
/// The agent is lent for the run only: once it ends, the key is empty.
pub const AGENT: &str = "agent";

/// Something an agent's scripts can use: a global table of functions that
/// reach outside the sandbox, and what the model reads about them.
///
/// ```ignore
/// struct Clock;
///
/// impl Interface for Clock {
///     fn name(&self) -> &'static str {
///         "clock"
///     }
///
///     fn docs(&self) -> &'static str {
///         "- `clock.now()`: the time, as a string."
///     }
///
///     fn functions(&self, functions: &mut Functions) {
///         functions.add("now", |_, _, ()| Ok(Utc::now().to_string()));
///     }
/// }
/// ```
pub trait Interface: Send + Sync {
    /// The global's name: scripts call `name.function(...)`.
    fn name(&self) -> &'static str;

    /// How to use it, written for the model. It goes into the `lua` tool's
    /// description.
    fn docs(&self) -> &'static str;

    /// Add the functions scripts can call. This runs once for each new
    /// runtime.
    fn functions(&self, functions: &mut Functions);
}

/// The table an interface's functions go into, as it's being made.
pub struct Functions<'a> {
    lua: &'a Lua,
    table: Table,
    /// The first function that couldn't be made, if one couldn't.
    error: Option<mlua::Error>,
}

impl Functions<'_> {
    /// Add a function, called as `interface.name(...)`. It gets the runtime,
    /// the agent whose script called it, and the arguments. Arguments and
    /// results convert as mlua converts them: `()` for none, a tuple for
    /// several. An error becomes a Lua error, which a script can catch with
    /// `pcall`.
    pub fn add<A, R, F>(&mut self, name: &'static str, function: F) -> &mut Self
    where
        A: FromLuaMulti + 'static,
        R: IntoLuaMulti + 'static,
        F: Fn(&Lua, &mut Agent, A) -> Result<R, Error> + Send + Sync + 'static,
    {
        let made = self.lua.create_function(move |lua, arguments: A| {
            let agent: AnyUserData = lua.named_registry_value(AGENT)?;
            agent
                .borrow_mut_scoped(|agent: &mut Agent| function(lua, agent, arguments))?
                .map_err(mlua::Error::external)
        });
        if let Err(e) = made.and_then(|f| self.table.set(name, f)) {
            self.error.get_or_insert(e);
        }
        self
    }
}

/// Make `interface` a global of `lua`. Scripts can't change it.
pub fn install(interface: &dyn Interface, lua: &Lua) -> mlua::Result<()> {
    let mut functions = Functions {
        lua,
        table: lua.create_table()?,
        error: None,
    };
    interface.functions(&mut functions);
    if let Some(e) = functions.error {
        return Err(e);
    }
    functions.table.set_readonly(true);
    lua.globals().set(interface.name(), functions.table)
}
