use async_openai::types::chat::{
    ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
    ChatCompletionRequestMessage, ChatCompletionRequestToolMessage,
    ChatCompletionRequestUserMessage,
};
use chrono::{DateTime, Utc};
use json_patch::Patch;
use serde::Serialize;
use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use tokio::sync::broadcast;

/// Who wrote a message.
#[derive(Clone, Debug, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// The result of the tool call with this ID.
    Tool(String),
}

/// One message in the conversation, with metadata the model sees with it.
#[derive(Clone, Debug, Serialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    /// When the message joined the context.
    pub time: DateTime<Utc>,
    /// Tools the assistant called in this message.
    pub tool_calls: Vec<ChatCompletionMessageToolCalls>,
}

impl Message {
    /// A new message to add after `existing`.
    ///
    /// Its ID is a hash of the previous message's ID plus its own role and
    /// text, computed once here. Edits elsewhere don't change it, so the model
    /// can keep referring to it.
    pub fn new(existing: &[Message], role: Role, content: String) -> Self {
        let mut hasher = DefaultHasher::new();
        (existing.last().map(|m| &m.id), &role, &content).hash(&mut hasher);
        // Re-hash on the rare collision with an existing ID.
        let id = loop {
            let id = format!("{:06X}", hasher.finish() & 0xFF_FFFF);
            if existing.iter().all(|m| m.id != id) {
                break id;
            }
            id.hash(&mut hasher);
        };
        Message {
            id,
            role,
            content,
            time: Utc::now(),
            tool_calls: Vec::new(),
        }
    }
}

/// How every tag the model sees starts. The agent also uses it to drop tags
/// the model imitates at the start of a reply.
pub const METADATA: &str = "<metadata";

/// The message as the model sees it: a `<metadata>` tag, then the text on the
/// next line. Tool results are the exception and are sent as plain text.
///
/// ```text
/// <metadata id="0E9258" time="2026-11-22T01:47:53Z"/>
/// Hey Ilia!
/// ```
impl From<Message> for ChatCompletionRequestMessage {
    fn from(message: Message) -> Self {
        let content = format!(
            "{METADATA} id=\"{}\" time=\"{}\"/>\n{}",
            message.id,
            message.time.format("%FT%TZ"),
            message.content
        );
        match message.role {
            Role::User => ChatCompletionRequestUserMessage::from(content).into(),
            Role::Assistant => ChatCompletionRequestAssistantMessage {
                content: Some(content.into()),
                tool_calls: (!message.tool_calls.is_empty()).then_some(message.tool_calls),
                ..Default::default()
            }
            .into(),
            // Tool results get no metadata: they belong to the assistant
            // message that made the call, and go wherever it goes.
            Role::Tool(tool_call_id) => ChatCompletionRequestToolMessage {
                content: message.content.into(),
                tool_call_id,
            }
            .into(),
        }
    }
}

/// The conversation the model sees.
///
/// Every change goes through `update`, which publishes it to subscribers as a
/// JSON Patch against the context's JSON form:
///
/// ```json
/// {"messages": {"0E9258": {...}}, "order": ["8468AF", "0E9258"]}
/// ```
///
/// Messages are keyed by ID rather than kept in a list, so inserting or
/// removing one doesn't resend every message after it.
pub struct Context {
    messages: Vec<Message>,
    /// The message being written right now. It and everything after it can't
    /// be edited.
    pub current: Option<String>,
    /// The JSON form as of the last published change.
    published: Value,
    changes: broadcast::Sender<Patch>,
}

impl Context {
    pub fn new() -> Self {
        let (changes, _) = broadcast::channel(1024);
        let mut context = Self {
            messages: Vec::new(),
            current: None,
            published: Value::Null,
            changes,
        };
        context.published = context.to_json();
        context
    }

    /// Every change from now on, as a patch against the previous state. The
    /// state before any change is `{"messages": {}, "order": []}`.
    pub fn subscribe(&self) -> broadcast::Receiver<Patch> {
        self.changes.subscribe()
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Change the messages, then publish what changed.
    pub fn update<R>(&mut self, change: impl FnOnce(&mut Vec<Message>) -> R) -> R {
        let result = change(&mut self.messages);
        let json = self.to_json();
        let patch = json_patch::diff(&self.published, &json);
        if !patch.0.is_empty() {
            let _ = self.changes.send(patch); // fails only with no subscribers
        }
        self.published = json;
        result
    }

    /// Add a message at the end and return its index.
    pub fn push(&mut self, role: Role, content: impl Into<String>) -> usize {
        self.update(|messages| {
            let message = Message::new(messages, role, content.into());
            messages.push(message);
            messages.len() - 1
        })
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
