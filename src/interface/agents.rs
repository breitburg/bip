use crate::agent::{Agent, Input};
use crate::interface::Interface;

const DOCS: &str = "\
`agents` starts subagents.

- `agents.spawn(task)`: start a subagent that works on `task`, a string, in
  the background, and return its name at once, like `sub-1`. When the
  subagent finishes, its final reply arrives as a message from it, marked
  with that name. The subagent starts with an empty conversation and sees
  nothing of this one, so `task` must hold everything it needs to know. It
  can run Luau too, but has none of these interfaces.";

/// Lets an agent start subagents.
pub fn interface() -> Interface {
    Interface::new("agents", DOCS).function("spawn", |_, agent, task: String| {
        // Numbers restart after a clear, which forgets every subagent.
        let name = format!("sub-{}", agent.children.len() + 1);
        // Another agent with the same setup, running in the background. Its
        // last reply comes back as a message from it.
        let (child, inputs) = Agent::new(
            name.clone(),
            agent.client.clone(),
            agent.model.clone(),
            agent.system_prompt.clone(),
            agent.outputs.clone(),
            Vec::new(),
            Some(agent.inbox.clone()),
        );
        let from = Some(agent.name.clone());
        let _ = child.inbox.try_send(Input::Message { text: task, from });
        agent.children.push(tokio::spawn(child.run(inputs)));
        Ok(name)
    })
}
