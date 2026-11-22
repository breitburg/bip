pub mod revise;

use crate::context::Context;
use async_openai::types::chat::{ChatCompletionTool, FunctionObject};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::error::Error;

/// A tool definition paired with the function that runs it on the model's
/// raw JSON arguments.
pub type Tool = (
    ChatCompletionTool,
    Box<dyn Fn(&str, &mut Context) -> Result<Outcome, Box<dyn Error>> + Send + Sync>,
);

/// What a tool hands back to the agent.
pub struct Outcome {
    /// What to tell the model.
    pub text: String,
    /// Continue the conversation from right after this message: everything
    /// after it, including the current turn, is dropped.
    pub resume_from: Option<String>,
}

/// Most tools only have text to report.
impl From<String> for Outcome {
    fn from(text: String) -> Self {
        Outcome {
            text,
            resume_from: None,
        }
    }
}

/// Build a tool from `run`. The model fills in `T`; its doc comment and field
/// docs become the description and schema.
pub fn tool<T: JsonSchema + DeserializeOwned + 'static>(
    name: &str,
    run: fn(T, &mut Context) -> Result<Outcome, Box<dyn Error>>,
) -> Tool {
    let mut schema = serde_json::to_value(schema_for!(T)).unwrap();
    let function = FunctionObject {
        name: name.into(),
        description: schema["description"].as_str().map(Into::into),
        parameters: Some(schema.take()),
        strict: None,
    };
    (
        ChatCompletionTool { function },
        Box::new(move |arguments, context| run(serde_json::from_str(arguments)?, context)),
    )
}
