use crate::context::{Context, METADATA, Role};
use crate::tools::{self, Tool};
use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestSystemMessage, ChatCompletionTools, CreateChatCompletionRequestArgs,
    },
};
use futures::StreamExt;
use json_patch::Patch;
use std::error::Error;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

/// What the agent sends out. The stream never ends while the agent runs:
/// it may say things on its own, not only in reply to an input.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Output {
    /// A change to the context, as a JSON Patch. Applying them in order to
    /// `{"messages": {}, "order": []}` rebuilds the conversation.
    Change(Patch),
    /// Something went wrong with this input. The agent keeps running.
    Error(String),
}

/// What the agent accepts. Clients send these as JSON; the protocol is
/// documented in README.md.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Input {
    /// A message for Bip from a person.
    Message(String),
    /// Remove every message from the context.
    Clear,
}

/// The agent before it runs: a model client and its system prompt.
pub struct Agent {
    client: Client<OpenAIConfig>,
    /// Sent ahead of the context on every request. It isn't part of the
    /// context, so it has no ID, can't be edited, and isn't published.
    system_prompt: String,
}

/// A running agent: everything `run` was given, for the turn loop.
struct Session {
    client: Client<OpenAIConfig>,
    system_prompt: String,
    model: String,
    /// The caller's tools followed by the internal ones, like `revise`.
    tools: Vec<Tool>,
    context: Context,
}

impl Agent {
    pub fn new(client: Client<OpenAIConfig>, system_prompt: String) -> Self {
        Self {
            client,
            system_prompt,
        }
    }

    /// Handle every input from `inputs` in order with `model` and `tools`
    /// (plus the internal tools). Every change to the context goes to
    /// `outputs` as it happens. Returns when `inputs` closes.
    pub async fn run(
        self,
        model: impl Into<String>,
        mut tools: Vec<Tool>,
        mut inputs: mpsc::Receiver<Input>,
        outputs: mpsc::Sender<Output>,
    ) {
        tools.push(tools::tool("revise", tools::revise::revise));
        let mut session = Session {
            client: self.client,
            system_prompt: self.system_prompt,
            model: model.into(),
            tools,
            context: Context::new(),
        };
        let mut changes = session.context.subscribe();

        // The context publishes its own changes; the agent only passes them
        // on.
        let forward = outputs.clone();
        tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(patch) => {
                        if forward.send(Output::Change(patch)).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(n, "context changes dropped");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        while let Some(input) = inputs.recv().await {
            let message = match input {
                Input::Message(message) => message,
                Input::Clear => {
                    info!("clearing context");
                    session.context.update(|messages| messages.clear());
                    continue;
                }
            };
            session.context.push(Role::User, message);
            // The error becomes a String before the next await, since
            // `Box<dyn Error>` can't be held across one in a spawned task.
            let error = session.respond().await.err().map(|e| e.to_string());
            if let Some(error) = error {
                warn!(%error, "turn failed");
                let _ = outputs.send(Output::Error(error)).await;
            }
        }
    }
}

impl Session {
    /// Call the model until it answers without calling tools.
    async fn respond(&mut self) -> Result<(), Box<dyn Error>> {
        for step in 1..=5 {
            info!(
                step,
                messages = self.context.messages().len(),
                "sending request"
            );
            let tool_calls = self.stream_reply().await?;
            if tool_calls.is_empty() {
                info!(steps = step, "done");
                return Ok(());
            }

            // The reply that made these calls, and everything after it, is
            // the current turn, which `revise` can't touch. Results are added
            // once all the tools ran, so every call keeps its result.
            self.context.current = self.context.messages().last().map(|m| m.id.clone());
            let mut results = Vec::new();
            let mut resume_from = None;
            for call in &tool_calls {
                let ChatCompletionMessageToolCalls::Function(call) = call else {
                    continue;
                };
                let (name, arguments) = (&call.function.name, &call.function.arguments);
                info!(tool = %name, %arguments, "tool call");
                let output = match self
                    .tools
                    .iter()
                    .find(|(tool, _)| tool.function.name == *name)
                {
                    Some((_, run)) => run(arguments, &mut self.context),
                    None => Err(format!("unknown tool: {name}").into()),
                };
                let output = output.unwrap_or_else(|e| {
                    warn!(tool = %name, error = %e, "tool failed");
                    format!("error: {e}").into()
                });
                info!(tool = %name, output = %output.text, "tool result");
                resume_from = output.resume_from.or(resume_from);
                results.push((call.id.clone(), output.text));
            }
            self.context.current = None;

            // A tool asked to continue from an earlier message: drop
            // everything after it, this turn included, then go on from there.
            // The whole tail goes at once, so every call leaves with its result.
            if let Some(id) = resume_from {
                let Some(index) = self.context.messages().iter().position(|m| m.id == id) else {
                    return Err(format!("can't resume from {id}: it no longer exists").into());
                };
                info!(%id, "resuming");
                self.context.update(|messages| messages.truncate(index + 1));
                // Resuming from one of Bip's own messages means it has said
                // what it wanted to; there's nothing to reply to.
                if matches!(self.context.messages()[index].role, Role::Assistant) {
                    info!(steps = step, "done");
                    return Ok(());
                }
                continue;
            }
            for (call_id, output) in results {
                self.context.push(Role::Tool(call_id), output);
            }
        }
        Err("model did not finish within 5 steps".into())
    }

    /// Stream one model reply into a new assistant message and return the
    /// tool calls it made.
    async fn stream_reply(
        &mut self,
    ) -> Result<Vec<ChatCompletionMessageToolCalls>, Box<dyn Error>> {
        let request = CreateChatCompletionRequestArgs::default()
            .model(&self.model)
            .messages(
                std::iter::once(
                    ChatCompletionRequestSystemMessage::from(self.system_prompt.as_str()).into(),
                )
                .chain(self.context.messages().iter().cloned().map(Into::into))
                .collect::<Vec<_>>(),
            )
            .tools(
                self.tools
                    .iter()
                    .map(|(tool, _)| ChatCompletionTools::Function(tool.clone()))
                    .collect::<Vec<_>>(),
            )
            .build()?;
        debug!(messages = %serde_json::to_string(&request.messages)?, "request");
        let mut stream = self.client.chat().create_stream(request).await?;

        let index = self.context.push(Role::Assistant, "");
        // Models imitate what they see, so a reply may start with a made-up
        // `<metadata>` tag. The start of the reply is held back until it's
        // clear, and a tag is dropped.
        let mut held = Some(String::new());
        let mut calls: Vec<ChatCompletionMessageToolCall> = Vec::new();

        while let Some(chunk) = stream.next().await {
            for choice in chunk?.choices {
                if let Some(mut text) = choice.delta.content {
                    if let Some(start) = held.as_mut() {
                        start.push_str(&text);
                        if METADATA.starts_with(start.as_str()) {
                            continue; // could still become a tag
                        }
                        if start.starts_with(METADATA) {
                            // A tag ends with `/>` and its line.
                            let Some(end) = start.find("/>\n").map(|i| i + 3) else {
                                continue;
                            };
                            start.drain(..end);
                        }
                        text = held.take().unwrap_or_default();
                    }
                    self.context
                        .update(|messages| messages[index].content.push_str(&text));
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
                        call.function.name += &function.name.unwrap_or_default();
                        call.function.arguments += &function.arguments.unwrap_or_default();
                    }
                }
            }
        }
        if let Some(text) = held.filter(|text| !text.is_empty()) {
            self.context
                .update(|messages| messages[index].content.push_str(&text));
        }
        info!(text = %self.context.messages()[index].content, "assistant");

        let calls: Vec<_> = calls
            .into_iter()
            .map(ChatCompletionMessageToolCalls::Function)
            .collect();
        // Setting no calls changes nothing, so no patch is published.
        let tool_calls = calls.clone();
        self.context
            .update(|messages| messages[index].tool_calls = tool_calls);
        Ok(calls)
    }
}
