use crate::agent::Input;
use crate::context::short_id;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// The name of the primary agent, the one that's always there.
pub const MAIN: &str = "main";

/// What a client asks of Bip. Each names the agent it's for, `main` when it
/// names none. The protocol is documented in README.md.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Command {
    /// A message for an agent from the person at this client.
    Message {
        text: String,
        #[serde(default = "main")]
        agent: String,
    },
    /// End an agent's turn where it is and stop its subagents. Its context
    /// stays.
    Stop(#[serde(default)] Option<Target>),
    /// Empty `main` and remove every subagent.
    Clear,
}

#[derive(Debug, serde::Deserialize)]
pub struct Target {
    #[serde(default = "main")]
    pub agent: String,
}

fn main() -> String {
    MAIN.into()
}

/// Every running agent, by name, so any of them can be reached from
/// anywhere: by clients, and by agents' scripts. Cloning it is cheap; every
/// clone is the same swarm.
#[derive(Clone, Default)]
pub struct Swarm(Arc<Mutex<Registry>>);

#[derive(Default)]
struct Registry {
    agents: HashMap<String, Member>,
}

struct Member {
    inbox: mpsc::Sender<Input>,
    /// The agent that started this one. None for `main`.
    parent: Option<String>,
    /// The task running the agent, once it's started.
    task: Option<JoinHandle<()>>,
}

impl Swarm {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.0.lock().unwrap()
    }

    /// Pass a client's command to the agent it names. Fails if there's no
    /// such agent, as when it has already finished.
    pub async fn send(&self, command: Command) -> Result<(), String> {
        let (agent, input) = match command {
            Command::Message { text, agent } => (agent, Input::Message { text, from: None }),
            Command::Stop(target) => (target.map_or_else(main, |t| t.agent), Input::Stop),
            // `main` clears, because it's what keeps the conversation the
            // clear empties.
            Command::Clear => (main(), Input::Clear),
        };
        let inbox = self.inbox(&agent).ok_or_else(|| format!("no agent named {agent:?}"))?;
        inbox.send(input).await.map_err(|_| format!("{agent} has finished"))
    }

    /// A name for a new subagent: a short ID, like a message's, that no
    /// running agent has.
    pub fn new_name(&self) -> String {
        let registry = self.lock();
        short_id(|id| registry.agents.contains_key(id))
    }

    pub fn join(&self, name: &str, inbox: mpsc::Sender<Input>, parent: Option<String>) {
        let member = Member { inbox, parent, task: None };
        self.lock().agents.insert(name.into(), member);
    }

    /// Record the task running `name`, so it can be stopped. An agent that
    /// already finished has left, and stays gone.
    pub fn started(&self, name: &str, task: JoinHandle<()>) {
        match self.lock().agents.get_mut(name) {
            Some(member) => member.task = Some(task),
            None => task.abort(),
        }
    }

    /// Called by an agent as it goes.
    pub fn leave(&self, name: &str) {
        self.lock().agents.remove(name);
    }

    pub fn inbox(&self, name: &str) -> Option<mpsc::Sender<Input>> {
        self.lock().agents.get(name).map(|m| m.inbox.clone())
    }

    /// The subagents `name` started that are still running.
    pub fn children(&self, name: &str) -> Vec<String> {
        let registry = self.lock();
        let children = registry.agents.iter();
        children
            .filter(|(_, m)| m.parent.as_deref() == Some(name))
            .map(|(child, _)| child.clone())
            .collect()
    }

    /// Whether `name` was started by `ancestor`, or by one of its subagents,
    /// and so on.
    pub fn descends_from(&self, name: &str, ancestor: &str) -> bool {
        let registry = self.lock();
        let mut parent = registry.agents.get(name).and_then(|m| m.parent.as_deref());
        while let Some(p) = parent {
            if p == ancestor {
                return true;
            }
            parent = registry.agents.get(p).and_then(|m| m.parent.as_deref());
        }
        false
    }

    /// End an agent at once: it sends no report. Its subagents go with it.
    /// Returns whether it was running.
    ///
    /// It leaves the swarm right away rather than when its task is dropped,
    /// so its parent never waits for it.
    pub fn kill(&self, name: &str) -> bool {
        let member = self.lock().agents.remove(name);
        let Some(member) = member else {
            return false;
        };
        // Dropping the agent kills its own subagents.
        if let Some(task) = member.task {
            task.abort();
        }
        true
    }

    /// End every subagent, and wait until they're really gone, so nothing
    /// they send comes after.
    pub async fn kill_subagents(&self) {
        let tasks: Vec<_> = {
            let mut registry = self.lock();
            let names: Vec<_> = registry.agents.keys().filter(|n| *n != MAIN).cloned().collect();
            let members = names.iter().filter_map(|n| registry.agents.remove(n));
            members.filter_map(|m| m.task).collect()
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Command {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn commands_default_to_main() {
        assert!(matches!(parse(r#"{"type":"message","data":{"text":"hi"}}"#),
            Command::Message { agent, .. } if agent == MAIN));
        assert!(matches!(parse(r#"{"type":"message","data":{"text":"hi","agent":"4C1D7A"}}"#),
            Command::Message { agent, .. } if agent == "4C1D7A"));
        assert!(matches!(parse(r#"{"type":"stop"}"#), Command::Stop(None)));
        assert!(matches!(parse(r#"{"type":"stop","data":{}}"#),
            Command::Stop(Some(Target { agent })) if agent == MAIN));
        assert!(matches!(parse(r#"{"type":"stop","data":{"agent":"0B3E95"}}"#),
            Command::Stop(Some(Target { agent })) if agent == "0B3E95"));
        assert!(matches!(parse(r#"{"type":"clear"}"#), Command::Clear));
    }
}
