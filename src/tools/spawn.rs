use crate::agent::{Agent, Input};
use crate::tools::{Error, Tool, Tools};
use schemars::JsonSchema;
use serde::Deserialize;

/// Start a subagent that works on `task` in the background, then carry on.
/// The call returns at once with the subagent's ID. When the subagent
/// finishes, its final reply arrives as a message from it, marked with its ID.
///
/// The subagent starts with an empty conversation and no tools. It sees
/// nothing of this conversation, so `task` must hold everything it needs to
/// know.
#[derive(Deserialize, JsonSchema)]
pub struct Args {
    /// The complete instructions for the subagent.
    task: String,
}

/// Lets an agent start subagents.
pub struct Spawn;

impl Tool for Spawn {
    const NAME: &'static str = "spawn";
    type Args = Args;

    async fn call(&self, args: Args, agent: &mut Agent) -> Result<String, Error> {
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
            Tools::new(),
            Some(agent.inbox.clone()),
        );
        let from = Some(agent.name.clone());
        let _ = child.inbox.try_send(Input::Message { text: args.task, from });
        agent.children.push(tokio::spawn(child.run(inputs)));
        Ok(format!("Started subagent {name}."))
    }
}
