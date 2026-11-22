use crate::agent::{Agent, Input};
use crate::interface::Interface;

const DOCS: &str = "\
`agents` starts and stops subagents.

- `agents.spawn(task)`: start a subagent that works on `task`, a string, in
  the background, and return its name at once, like `sub-1`. When the
  subagent finishes, its final reply arrives as a message from it, marked
  with that name. The subagent starts with an empty conversation and sees
  nothing of this one, so `task` must hold everything it needs to know. It
  can run Luau too, but has none of these interfaces.
- `agents.stop(name)`: stop a subagent you started, or one they started,
  at once. It's gone, with its own subagents, and sends no report. Returns
  whether it was still running.";

/// Lets an agent start subagents.
pub fn interface() -> Interface {
    Interface::new("agents", DOCS)
        .function("spawn", |_, agent, task: String| {
            let name = agent.swarm.next_name();
            // Another agent with the same setup, running in the background. Its
            // last reply comes back as a message from it.
            let (child, inputs) = Agent::new(
                name.clone(),
                agent.client.clone(),
                agent.model.clone(),
                agent.system_prompt.clone(),
                agent.outputs.clone(),
                Vec::new(),
                Some(agent.name.clone()),
                agent.swarm.clone(),
            );
            let from = Some(agent.name.clone());
            if let Some(inbox) = agent.swarm.inbox(&name) {
                let _ = inbox.try_send(Input::Message { text: task, from });
            }
            agent.swarm.started(&name, tokio::spawn(child.run(inputs)));
            Ok(name)
        })
        .function("stop", |_, agent, name: String| {
            // A finished subagent has left the swarm, so it's not found and
            // wasn't running.
            if agent.swarm.inbox(&name).is_none() {
                return Ok(false);
            }
            if !agent.swarm.descends_from(&name, &agent.name) {
                return Err(format!("{name} isn't a subagent of yours").into());
            }
            Ok(agent.swarm.kill(&name))
        })
}
