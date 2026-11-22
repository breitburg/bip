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

/// A `Tool` with its argument type hidden, so tools of different types fit
/// in one list.
trait Erased: Send + Sync {
    fn call<'a>(&'a self, arguments: &'a str, agent: &'a mut Agent) -> BoxFuture<'a, Result<String, Error>>;
}

impl<T: Tool> Erased for T {
    fn call<'a>(&'a self, arguments: &'a str, agent: &'a mut Agent) -> BoxFuture<'a, Result<String, Error>> {
        async move { Tool::call(self, serde_json::from_str(arguments)?, agent).await }.boxed()
    }
}

/// The tools an agent has. Cloning is cheap, so agents can share them.
#[derive(Clone, Default)]
pub struct Tools(Arc<Vec<(ChatCompletionTool, Arc<dyn Erased>)>>);

impl Tools {
    pub fn with<T: Tool>(mut self, tool: T) -> Self {
        let mut schema = serde_json::to_value(schema_for!(T::Args)).expect("a schema is JSON");
        let function = FunctionObject {
            name: T::NAME.into(),
            description: schema["description"].as_str().map(Into::into),
            parameters: Some(schema.take()),
            strict: None,
        };
        Arc::make_mut(&mut self.0).push((ChatCompletionTool { function }, Arc::new(tool)));
        self
    }

    /// The tools as a model request lists them.
    pub fn definitions(&self) -> Vec<ChatCompletionTools> {
        let definitions = self.0.iter().map(|(definition, _)| definition.clone());
        definitions.map(ChatCompletionTools::Function).collect()
    }

    /// Run the tool `name` on the model's raw JSON `arguments`. The model
    /// reads the result either way, so a failure comes back as text too.
    pub async fn call(&self, name: &str, arguments: &str, agent: &mut Agent) -> String {
        let tool = self.0.iter().find(|(tool, _)| tool.function.name == name);
        let result = match tool {
            Some((_, tool)) => tool.call(arguments, agent).await,
            None => Err(format!("unknown tool: {name}").into()),
        };
        result.unwrap_or_else(|e| format!("error: {e}"))
    }
}
