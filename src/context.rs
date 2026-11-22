use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
    ChatCompletionRequestAssistantMessage, ChatCompletionRequestMessage,
    ChatCompletionRequestToolMessage, ChatCompletionRequestUserMessage, FunctionCall,
};
use chrono::{DateTime, Utc};
use json_patch::Patch;
use serde::Serialize;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};

/// Who wrote a message.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// A message from the agent with this name: a subagent's report, or the
    /// task from the agent that started this one.
    Agent(String),
}

/// A tool call the assistant made, and its result once the tool has run.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Call {
    pub id: String,
    pub name: String,
    /// The arguments as the model wrote them: JSON, if the model got it right.
    pub arguments: String,
    pub result: Option<String>,
}

/// One message in the conversation, with metadata the model sees with it.
#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    /// When the message joined the context.
    pub time: DateTime<Utc>,
    /// Tools the assistant called in this message. A result lives on its
    /// call, so it goes wherever the message goes.
    pub tool_calls: Vec<Call>,
}

/// How every tag the model sees starts. The agent also uses it to drop tags
/// the model imitates at the start of a reply.
pub const METADATA: &str = "<metadata";

/// How times are written for the model and for scripts.
pub const TIME_FORMAT: &str = "%FT%TZ";

impl Message {
    /// The message as the model sees it: a `<metadata>` tag, then the text on
    /// the next line. A message from another agent also says who it's from:
    /// `from="sub-1"`. Each tool call is followed by its result, as plain text.
    ///
    /// ```text
    /// <metadata id="0E9258" time="2026-11-22T01:47:53Z"/>
    /// Hey Ilia!
    /// ```
    fn request(&self) -> Vec<ChatCompletionRequestMessage> {
        let from = match &self.role {
            Role::Agent(agent) => format!(" from=\"{agent}\""),
            _ => String::new(),
        };
        let tagged = format!(
            "{METADATA} id=\"{}\" time=\"{}\"{from}/>\n{}",
            self.id,
            self.time.format(TIME_FORMAT),
            self.content
        );
        match &self.role {
            Role::User | Role::Agent(_) => vec![ChatCompletionRequestUserMessage::from(tagged).into()],
            Role::Assistant => {
                let calls: Vec<_> = self
                    .tool_calls
                    .iter()
                    .map(|call| {
                        ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                            id: call.id.clone(),
                            function: FunctionCall {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        })
                    })
                    .collect();
                let reply = ChatCompletionRequestAssistantMessage {
                    content: Some(tagged.into()),
                    tool_calls: (!calls.is_empty()).then_some(calls),
                    ..Default::default()
                };
                let results = self.tool_calls.iter().map(|call| {
                    ChatCompletionRequestToolMessage {
                        content: call.result.as_deref().unwrap_or_default().into(),
                        tool_call_id: call.id.clone(),
                    }
                    .into()
                });
                std::iter::once(reply.into()).chain(results).collect()
            }
        }
    }
}

/// The conversation the model sees.
///
/// Every change is published as a JSON Patch against the context's JSON form:
///
/// ```json
/// {"messages": {"0E9258": {...}}, "order": ["8468AF", "0E9258"]}
/// ```
///
/// Messages are keyed by ID rather than kept in a list, so inserting or
/// removing one doesn't resend every message after it.
pub struct Context {
    messages: Vec<Message>,
    /// How many messages were ever added, which makes each new ID different.
    added: u64,
    publish: Box<dyn Fn(Patch) + Send + Sync>,
}

impl Context {
    /// An empty context: `{"messages": {}, "order": []}`. Every change from
    /// now on goes to `publish` as a patch against the previous state.
    pub fn new(publish: impl Fn(Patch) + Send + Sync + 'static) -> Self {
        Self {
            messages: Vec::new(),
            added: 0,
            publish: Box::new(publish),
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The conversation as the messages of a model request.
    pub fn request(&self) -> Vec<ChatCompletionRequestMessage> {
        self.messages.iter().flat_map(Message::request).collect()
    }

    /// Add a message at the end and return its index.
    pub fn push(&mut self, role: Role, content: impl Into<String>) -> usize {
        // The ID only has to be short, unique here, and unlike its neighbours,
        // so the model doesn't mix them up. Re-hash on the rare collision.
        let mut hasher = DefaultHasher::new();
        self.added += 1;
        self.added.hash(&mut hasher);
        let id = loop {
            let id = format!("{:06X}", hasher.finish() & 0xFF_FFFF);
            if self.messages.iter().all(|m| m.id != id) {
                break id;
            }
            id.hash(&mut hasher);
        };
        let message = Message {
            id,
            role,
            content: content.into(),
            time: Utc::now(),
            tool_calls: Vec::new(),
        };
        let index = self.messages.len();
        self.send(patch(json!([
            {"op": "add", "path": format!("/messages/{}", message.id), "value": message},
            {"op": "add", "path": format!("/order/{index}"), "value": message.id},
        ])));
        self.messages.push(message);
        index
    }

    /// Change one message. Only that message is compared to find what
    /// changed, so this is cheap enough to do for every streamed piece.
    pub fn edit<R>(&mut self, index: usize, change: impl FnOnce(&mut Message) -> R) -> R {
        let alone = |m: &Message| json!({"messages": {m.id.as_str(): m}});
        let before = alone(&self.messages[index]);
        let result = change(&mut self.messages[index]);
        self.send(json_patch::diff(&before, &alone(&self.messages[index])));
        result
    }

    /// Swap in a whole new conversation. This compares everything, so it's
    /// for the rare wholesale change, like a rewind or a clear.
    pub fn replace(&mut self, messages: Vec<Message>) {
        let before = self.to_json();
        self.messages = messages;
        self.send(json_patch::diff(&before, &self.to_json()));
    }

    /// The first tool call that has no result yet.
    pub fn pending_call(&self) -> Option<&Call> {
        let reply = &self.messages[frozen_from(&self.messages)?];
        reply.tool_calls.iter().find(|call| call.result.is_none())
    }

    /// Add `text` to the end of a message. This runs for every streamed
    /// piece, so it writes its patch directly rather than comparing.
    pub fn append(&mut self, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }
        let message = &mut self.messages[index];
        message.content.push_str(text);
        let path = format!("/messages/{}/content", message.id);
        let operation = json!([{"op": "replace", "path": path, "value": message.content}]);
        self.send(patch(operation));
    }

    /// Give a tool call its result. A call that's no longer in the context
    /// has nowhere to put one, so its result is dropped.
    pub fn resolve(&mut self, call_id: &str, result: String) {
        let position = self.messages.iter().enumerate().find_map(|(index, m)| {
            let call = m.tool_calls.iter().position(|call| call.id == call_id)?;
            Some((index, call))
        });
        if let Some((index, call)) = position {
            self.edit(index, |m| m.tool_calls[call].result = Some(result));
        }
    }

    /// Whether the conversation ends on something the model should reply to:
    /// a message to it, or its own tool calls.
    pub fn awaits_reply(&self) -> bool {
        self.messages
            .last()
            .is_some_and(|m| m.role != Role::Assistant || !m.tool_calls.is_empty())
    }

    fn send(&self, patch: Patch) {
        if !patch.0.is_empty() {
            (self.publish)(patch);
        }
    }

    fn to_json(&self) -> Value {
        let messages: serde_json::Map<_, _> = self
            .messages
            .iter()
            .map(|m| (m.id.clone(), json!(m)))
            .collect();
        let order: Vec<&str> = self.messages.iter().map(|m| m.id.as_str()).collect();
        json!({ "messages": messages, "order": order })
    }
}

/// Where the current turn starts in `messages`: the reply whose tool calls
/// are still running. It and everything after it can't be edited.
pub fn frozen_from(messages: &[Message]) -> Option<usize> {
    messages
        .iter()
        .position(|m| m.tool_calls.iter().any(|call| call.result.is_none()))
}

/// A patch written out by hand. Its paths are built from message IDs and
/// indexes, so it's always well formed.
fn patch(operations: Value) -> Patch {
    serde_json::from_value(operations).expect("a well-formed patch")
}
