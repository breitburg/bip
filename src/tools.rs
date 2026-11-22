pub mod lua;
pub mod revise;
pub mod spawn;

use crate::agent::Agent;
use async_openai::types::chat::{ChatCompletionTool, ChatCompletionTools, FunctionObject};
use futures::{FutureExt, future::BoxFuture};
use schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// The error type everywhere: it can cross threads, so tasks can hold it.
pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Something the model can call. A tool takes its arguments and returns text
/// for the model; where that text goes, and what happens next, is the
/// agent's business.
pub trait Tool: Send + Sync + 'static {
    const NAME: &'static str;
    /// What the model fills in. Its doc comment and field docs become the
    /// tool's description and schema.
    type Args: JsonSchema + DeserializeOwned + Send;

    /// Run on the agent that called it.
    fn call(
        &self,
        args: Self::Args,
        agent: &mut Agent,
    ) -> impl Future<Output = Result<String, Error>> + Send;
}

/// Any `Tool`, with its argument type hidden, so tools of different types
/// fit in one list.
pub trait AnyTool: Send + Sync {
    /// The tool as a model request lists it.
    fn definition(&self) -> ChatCompletionTools;
    fn name(&self) -> &'static str;
    fn call<'a>(&'a self, arguments: &'a str, agent: &'a mut Agent) -> BoxFuture<'a, Result<String, Error>>;
}

impl<T: Tool> AnyTool for T {
    fn definition(&self) -> ChatCompletionTools {
        let mut schema = serde_json::to_value(schema_for!(T::Args)).expect("a schema is JSON");
        let function = FunctionObject {
            name: T::NAME.into(),
            description: schema["description"].as_str().map(Into::into),
            parameters: Some(schema.take()),
            strict: None,
        };
        ChatCompletionTools::Function(ChatCompletionTool { function })
    }

    fn name(&self) -> &'static str {
        T::NAME
    }

    fn call<'a>(&'a self, arguments: &'a str, agent: &'a mut Agent) -> BoxFuture<'a, Result<String, Error>> {
        async move { Tool::call(self, serde_json::from_str(arguments)?, agent).await }.boxed()
    }
}

/// The tools an agent has.
pub type Tools = Vec<Arc<dyn AnyTool>>;

/// Run the tool `name` from `tools` on the model's raw JSON `arguments`. The
/// model reads the result either way, so a failure comes back as text too.
pub async fn call(tools: &[Arc<dyn AnyTool>], name: &str, arguments: &str, agent: &mut Agent) -> String {
    let result = match tools.iter().find(|tool| tool.name() == name) {
        Some(tool) => tool.call(arguments, agent).await,
        None => Err(format!("unknown tool: {name}").into()),
    };
    result.unwrap_or_else(|e| format!("error: {e}"))
}
