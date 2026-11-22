mod agent;
mod context;
mod interface;
mod lua;
mod swarm;

use agent::{Agent, Output};
use async_openai::{Client, config::OpenAIConfig};
use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, Utf8Bytes, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::get,
};
use futures::{SinkExt, StreamExt};
use std::error::Error;
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc};
use tower_http::services::ServeDir;
use swarm::{Command, MAIN, Swarm};
use tracing::{info, warn};

/// What every connection handler needs.
#[derive(Clone)]
struct Server {
    /// Where clients' commands go.
    swarm: Swarm,
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
    let (output_tx, mut output_rx) = mpsc::unbounded_channel();
    let swarm = Swarm::default();
    let (main, inputs) = Agent::new(
        MAIN.into(),
        Client::with_config(config),
        "claude-sonnet-5-5".into(),
        std::fs::read_to_string("system.md")?,
        output_tx,
        vec![interface::context::interface(), interface::agents::interface()],
        None,
        swarm.clone(),
    );
    swarm.started(MAIN, tokio::spawn(main.run(inputs)));

    // Every connected client gets every output, so the agent's stream is
    // fanned out through a broadcast channel and recorded in the history.
    let server = Server {
        swarm,
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
                |ws: WebSocketUpgrade, headers: HeaderMap, State(server): State<Server>| async move {
                    // Browsers send the page's origin with a WebSocket request.
                    // Only the page Bip serves may connect, so another site open
                    // in the browser can't read or drive the conversation.
                    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
                    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
                    if origin.is_some_and(|origin| Some(origin) != host.map(|h| format!("http://{h}")).as_deref()) {
                        warn!(?origin, "refused a connection from another site");
                        return StatusCode::FORBIDDEN.into_response();
                    }
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

/// One client: it sends `Command`s and receives every `Output` as JSON, starting
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
                Some(Ok(Message::Text(text))) => {
                    let result = match serde_json::from_str::<Command>(&text) {
                        Ok(command) => server.swarm.send(command).await,
                        Err(e) => Err(format!("invalid input: {e}")),
                    };
                    // Only this client sent it wrong, so only this client hears about it.
                    if let Err(message) = result {
                        let error = Output::Error { agent: MAIN.into(), message };
                        sink.send(Message::text(serde_json::to_string(&error)?)).await?;
                    }
                }
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
