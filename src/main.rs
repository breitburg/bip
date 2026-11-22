mod agent;
mod context;
mod tools;

use agent::{Agent, Input, Output};
use async_openai::{Client, config::OpenAIConfig};
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, Utf8Bytes, WebSocket},
    },
    routing::get,
};
use futures::{SinkExt, StreamExt};
use std::error::Error;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tower_http::services::ServeDir;
use tracing::{info, warn};

/// What every connection handler needs.
#[derive(Clone)]
struct Server {
    inputs: mpsc::Sender<Input>,
    /// Outputs as JSON text, serialized once for every client.
    outputs: broadcast::Sender<Utf8Bytes>,
    /// Every output sent so far, so a client that connects later can replay
    /// it and catch up. Cloning `Utf8Bytes` only copies a pointer.
    history: Arc<Mutex<Vec<Utf8Bytes>>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    // Log level comes from RUST_LOG (e.g. `RUST_LOG=debug`), defaulting to `info`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let config = OpenAIConfig::new()
        .with_api_base(std::env::var("OPENAI_BASE_URL")?)
        .with_api_key(std::env::var("OPENAI_API_KEY")?);
    let agent = Agent::new(
        Client::with_config(config),
        std::fs::read_to_string("system.md")?,
    );

    let (input_tx, input_rx) = mpsc::channel(8);
    let (output_tx, mut output_rx) = mpsc::channel(64);
    tokio::spawn(agent.run("claude-sonnet-5-5", vec![], input_rx, output_tx));

    // Every connected client gets every output, so the agent's stream is
    // fanned out through a broadcast channel and recorded in the history.
    let server = Server {
        inputs: input_tx,
        outputs: broadcast::channel(256).0,
        history: Arc::default(),
    };
    let fan_out = server.clone();
    tokio::spawn(async move {
        while let Some(output) = output_rx.recv().await {
            let json = Utf8Bytes::from(serde_json::to_string(&output).unwrap());
            fan_out.history.lock().unwrap().push(json.clone());
            let _ = fan_out.outputs.send(json); // fails only when no one is connected
        }
    });

    let app = Router::new()
        .route(
            "/ws",
            get(
                |ws: WebSocketUpgrade, State(server): State<Server>| async move {
                    ws.on_upgrade(|socket| async move {
                        info!("client connected");
                        if let Err(e) = serve(socket, server).await {
                            warn!(error = %e, "client failed");
                        }
                        info!("client disconnected");
                    })
                },
            ),
        )
        // Everything else is a file in static/, read on every request, so
        // editing the page needs no rebuild. `/` is static/index.html.
        .fallback_service(ServeDir::new("static"))
        .with_state(server);

    let listener = TcpListener::bind("127.0.0.1:8765").await?;
    info!("listening on http://127.0.0.1:8765");
    axum::serve(listener, app).await?;
    Ok(())
}

/// One client: it sends `Input`s and receives every `Output` as JSON, starting
/// with a replay of everything sent before it connected.
async fn serve(socket: WebSocket, server: Server) -> Result<(), Box<dyn Error + Send + Sync>> {
    // Subscribing while holding the lock means no output is missed or sent
    // twice between the replay and the live stream.
    let (replay, mut outputs) = {
        let history = server.history.lock().unwrap();
        (history.clone(), server.outputs.subscribe())
    };
    let (mut sink, mut source) = socket.split();
    for json in replay {
        sink.send(Message::Text(json)).await?;
    }
    loop {
        tokio::select! {
            message = source.next() => match message {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Input>(&text) {
                    Ok(input) => server.inputs.send(input).await?,
                    // Only this client sent it wrong, so only this client hears about it.
                    Err(e) => {
                        let error = Output::Error(format!("invalid input: {e}"));
                        sink.send(Message::text(serde_json::to_string(&error)?)).await?;
                    }
                },
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {} // pings and binary frames are ignored
                Some(Err(e)) => return Err(e.into()),
            },
            output = outputs.recv() => match output {
                Ok(json) => sink.send(Message::Text(json)).await?,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(n, "client fell behind, outputs dropped");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}
