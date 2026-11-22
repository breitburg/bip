use crate::context::{Call, Context, METADATA, Role};
use crate::interface::{Error, Interface};
use crate::lua;
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{ChatCompletionRequestSystemMessage, CreateChatCompletionRequestArgs},
};
use futures::{FutureExt, StreamExt, future::BoxFuture};
use json_patch::Patch;
use std::collections::VecDeque;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{Instrument, debug, info, info_span, warn};

/// How many times one turn may call the model before it's given up on.
const MAX_STEPS: usize = 5;

/// What the agent sends out. The stream never ends while the agent runs:
/// it may say things on its own, not only in reply to an input.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Output {
    /// A change to one agent's context, as a JSON Patch. Applying an agent's
    /// patches in order to `{"messages": {}, "order": []}` rebuilds its
    /// conversation. The primary agent is `main`; subagents are `sub-1`, ...
    Change { agent: String, patch: Patch },
    /// Something went wrong in one agent's turn. The agent keeps running.
    Error { agent: String, message: String },
    /// This agent is gone, done or stopped. Nothing more will come from it.
    Finished { agent: String },
    /// A `clear` is complete: `main` is empty and every subagent is gone, so
    /// their conversations can be forgotten.
    Cleared,
}

/// What the agent accepts. Clients send these as JSON; the protocol is
/// documented in README.md.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Input {
    /// A message for this agent: from a person, or from the agent named in
    /// `from`, like a subagent reporting back.
    Message {
        text: String,
        /// Set only by agents; clients can't claim to be one.
        #[serde(skip_deserializing)]
        from: Option<String>,
    },
    /// Remove every message from the context, and stop and clear every
    /// subagent. Unlike a message, this doesn't wait for the turn to end.
    Clear,
}

/// One agent, primary or subagent: its name, how it reaches the model, the
/// conversation it sees, and what it can do.
pub struct Agent {
    /// `main`, or `sub-N` for a subagent. Outputs and logs carry it.
    pub name: String,
    pub client: Client<OpenAIConfig>,
    pub model: String,
    /// Sent ahead of the context on every request. It isn't part of the
    /// context, so it has no ID, can't be edited, and isn't published.
    pub system_prompt: String,
    /// Where everything this agent does goes, for clients to show.
    pub outputs: mpsc::UnboundedSender<Output>,
    /// What this agent's scripts can reach outside their sandbox.
    pub interfaces: Vec<Interface>,
    pub context: Context,
    /// This agent's Luau runtime, started by its first script.
    pub lua: Option<mlua::Lua>,
    /// A way to this agent's own inputs, handed to subagents so they can
    /// report back.
    pub inbox: mpsc::Sender<Input>,
    /// Who started this agent, to report to when it's done. None for `main`.
    parent: Option<mpsc::Sender<Input>>,
    /// The subagents this agent started, each running on its own.
    pub children: Vec<JoinHandle<()>>,
}

/// However an agent goes, by finishing or by being stopped, it says so as
/// the last thing it does.
impl Drop for Agent {
    fn drop(&mut self) {
        let agent = self.name.clone();
        let _ = self.outputs.send(Output::Finished { agent });
    }
}

impl Agent {
    /// A new agent, and the inputs to `run` it on.
    pub fn new(
        name: String,
        client: Client<OpenAIConfig>,
        model: String,
        system_prompt: String,
        outputs: mpsc::UnboundedSender<Output>,
        interfaces: Vec<Interface>,
        parent: Option<mpsc::Sender<Input>>,
    ) -> (Self, mpsc::Receiver<Input>) {
        let (inbox, inputs) = mpsc::channel(8);
        let (agent, publish) = (name.clone(), outputs.clone());
        let context = Context::new(move |patch| {
            let agent = agent.clone();
            let _ = publish.send(Output::Change { agent, patch });
        });
        let agent = Self {
            name,
            client,
            model,
            system_prompt,
            outputs,
            interfaces,
            context,
            lua: None,
            inbox,
            parent,
            children: Vec::new(),
        };
        (agent, inputs)
    }

    /// Handle inputs in order, forever for `main`. A subagent stops once it
    /// has nothing left to do and none of its own subagents is running, and
    /// sends its last reply to its parent as a message.
    ///
    /// Boxed, because a subagent's `run` is started from inside its parent's
    /// `run`, and only a boxed future can contain itself.
    pub fn run(mut self, mut inputs: mpsc::Receiver<Input>) -> BoxFuture<'static, ()> {
        let span = info_span!("agent", name = %self.name);
        async move {
            // Messages that arrived during a turn, waiting for it to end.
            let mut queued = VecDeque::new();
            let mut failure = None;
            loop {
                let idle = queued.is_empty() && inputs.is_empty();
                // A finished subagent's report is already in `inputs`.
                if idle && self.parent.is_some() && self.children.iter().all(|c| c.is_finished()) {
                    break;
                }
                let input = match queued.pop_front() {
                    Some(input) => input,
                    None => match inputs.recv().await {
                        Some(input) => input,
                        None => break,
                    },
                };
                match input {
                    Input::Message { text, from } => {
                        self.context.push(from.map_or(Role::User, Role::Agent), text);
                    }
                    Input::Clear => {
                        self.clear().await;
                        continue;
                    }
                }

                // A clear stops the turn where it is; dropping `turn` does that.
                let result = {
                    let turn = self.respond();
                    tokio::pin!(turn);
                    loop {
                        tokio::select! {
                            result = &mut turn => break Some(result),
                            input = inputs.recv() => match input {
                                Some(Input::Clear) => break None,
                                Some(message) => queued.push_back(message),
                                None => {} // only `main` can lose its senders
                            },
                        }
                    }
                };
                failure = match result {
                    Some(Ok(())) => None,
                    Some(Err(e)) => {
                        let message = e.to_string();
                        warn!(%message, "turn failed");
                        let agent = self.name.clone();
                        let _ = self.outputs.send(Output::Error {
                            agent,
                            message: message.clone(),
                        });
                        Some(message)
                    }
                    None => {
                        // Messages from before the clear are cleared with it.
                        queued.clear();
                        self.clear().await;
                        None
                    }
                };
            }
            if let Some(parent) = self.parent.take() {
                let text = match failure {
                    Some(message) => format!("(failed: {message})"),
                    None => self.last_reply(),
                };
                let from = Some(self.name.clone());
                let _ = parent.send(Input::Message { text, from }).await;
            }
        }
        .instrument(span)
        .boxed()
    }

    fn last_reply(&self) -> String {
        let replies = self.context.messages().iter().rev();
        replies
            .filter(|m| m.role == Role::Assistant && !m.content.is_empty())
            .map(|m| m.content.clone())
            .next()
            .unwrap_or_else(|| "(finished without replying)".into())
    }

    async fn clear(&mut self) {
        info!("clearing context");
        // Wait until the subagents are really gone, so `Cleared` comes after
        // everything they sent.
        for child in self.children.drain(..) {
            child.abort();
            let _ = child.await;
        }
        self.context.replace(Vec::new());
        self.lua = None;
        let _ = self.outputs.send(Output::Cleared);
    }

    /// Take the turn: run the tools the model called and call the model
    /// again, until the conversation ends on a reply that calls none.
    ///
    /// The context alone says what to do next, so a tool that rewrites it
    /// needs no special handling here. A call that's gone from the context is
    /// never run, and its result has nowhere to go.
    async fn respond(&mut self) -> Result<(), Error> {
        let mut steps = 0;
        loop {
            while let Some(call) = self.context.pending_call().cloned() {
                info!(tool = %call.name, arguments = %call.arguments, "tool call");
                let result = match call.name.as_str() {
                    lua::NAME => lua::call(self, &call.arguments),
                    name => format!("error: unknown tool: {name}"),
                };
                info!(tool = %call.name, %result, "tool result");
                self.context.resolve(&call.id, result);
            }
            if !self.context.awaits_reply() {
                info!(steps, "done");
                return Ok(());
            }
            if steps == MAX_STEPS {
                return Err(format!("model did not finish within {MAX_STEPS} steps").into());
            }
            steps += 1;
            info!(
                step = steps,
                messages = self.context.messages().len(),
                "sending request"
            );
            self.stream_reply().await?;
        }
    }

    /// Stream one model reply into a new assistant message, tool calls
    /// included.
    async fn stream_reply(&mut self) -> Result<(), Error> {
        let system = ChatCompletionRequestSystemMessage::from(self.system_prompt.as_str());
        let mut messages = vec![system.into()];
        messages.extend(self.context.request());
        let mut request = CreateChatCompletionRequestArgs::default();
        request
            .model(self.model.as_str())
            .messages(messages)
            .tools(vec![lua::definition(&self.interfaces)]);
        let request = request.build()?;
        debug!(messages = %serde_json::to_string(&request.messages)?, "request");
        let mut stream = self.client.chat().create_stream(request).await?;

        let index = self.context.push(Role::Assistant, "");
        // Models imitate what they see, and every message they see starts
        // with a `<metadata>` tag. So the start of the reply is held back
        // until it's clear whether it's one, and a tag is dropped.
        let mut held = Some(String::new());
        let mut release = |text: String| -> String {
            let Some(start) = held.as_mut() else {
                return text;
            };
            start.push_str(&text);
            if METADATA.starts_with(start.as_str()) {
                return String::new(); // could still become a tag
            }
            if start.starts_with(METADATA) {
                // A tag ends with `/>` and its line.
                let Some(end) = start.find("/>\n") else {
                    return String::new();
                };
                start.drain(..end + 3);
            }
            held.take().unwrap_or_default()
        };
        let mut calls: Vec<Call> = Vec::new();

        while let Some(chunk) = stream.next().await {
            // A reply cut off partway would stay in the context for good and
            // go out with every later request, so it's removed instead.
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(e) => {
                    let mut before = self.context.messages().to_vec();
                    before.truncate(index);
                    self.context.replace(before);
                    return Err(e.into());
                }
            };
            for choice in chunk.choices {
                if let Some(text) = choice.delta.content {
                    self.context.append(index, &release(text));
                }
                // Each piece names the call it belongs to by index.
                for piece in choice.delta.tool_calls.unwrap_or_default() {
                    let index = piece.index as usize;
                    if calls.len() <= index {
                        calls.resize_with(index + 1, Default::default);
                    }
                    let call = &mut calls[index];
                    if let Some(id) = piece.id {
                        call.id = id;
                    }
                    if let Some(function) = piece.function {
                        call.name += &function.name.unwrap_or_default();
                        call.arguments += &function.arguments.unwrap_or_default();
                    }
                }
            }
        }
        // A reply that only ever looked like the start of a tag is text.
        self.context.append(index, &held.unwrap_or_default());
        self.context.edit(index, |m| m.tool_calls = calls);
        info!(text = %self.context.messages()[index].content, "assistant");
        Ok(())
    }
}
