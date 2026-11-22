use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionRequestSystemMessage, ChatCompletionRequestUserMessage,
        CreateChatCompletionRequestArgs,
    },
};
use std::error::Error;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    let config = OpenAIConfig::new()
        .with_api_base(std::env::var("OPENAI_BASE_URL")?)
        .with_api_key(std::env::var("OPENAI_API_KEY")?);
    let client = Client::with_config(config);
    let system_prompt = std::fs::read_to_string("system.md")?;

    let request = CreateChatCompletionRequestArgs::default()
        .model("claude-sonnet-5-5")
        .messages(vec![
            ChatCompletionRequestSystemMessage::from(system_prompt).into(),
            ChatCompletionRequestUserMessage::from("Hey Bip!").into(),
        ])
        .build()?;
    let response = client.chat().create(request).await?;

    let reply = response.choices.into_iter().next().and_then(|c| c.message.content);
    println!("{}", reply.unwrap_or_default());
    Ok(())
}
