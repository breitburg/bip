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
use serde_json::{Value, json};
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
    /// The swarm document as it is now, with every change applied, so a
    /// client that connects later can start from it. Outputs are broadcast
    /// while it's locked, so a snapshot and the outputs after it never
    /// overlap or leave a gap.
    document: Arc<Mutex<Value>>,
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
        vec![Arc::new(interface::Context), Arc::new(interface::Agents)],
        None,
        swarm.clone(),
    );
    swarm.started(MAIN, tokio::spawn(main.run(inputs)));

    // Every connected client gets every output, so the agents' stream is
    // fanned out through a broadcast channel, and every change is applied to
    // the document.
    let server = Server {
        swarm,
        outputs: broadcast::channel(256).0,
        document: Arc::new(Mutex::new(json!({"agents": {}}))),
    };
    let fan_out = server.clone();
    tokio::spawn(async move {
        while let Some(output) = output_rx.recv().await {
            let json = Utf8Bytes::from(serde_json::to_string(&output).unwrap());
            let mut document = fan_out.document.lock().unwrap();
            if let Output::Change { patch } = &output
                && let Err(e) = json_patch::patch(&mut document, patch)
            {
                warn!(error = %e, "a change didn't apply to the document");
            }
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

/// The document as it is now, as a change that replaces a client's whole
/// copy, and the outputs that come after it.
fn snapshot(server: &Server) -> (Utf8Bytes, broadcast::Receiver<Utf8Bytes>) {
    // Subscribing while holding the lock means no output is missed or
    // applied twice between the snapshot and the live stream.
    let document = server.document.lock().unwrap();
    let agents = &document["agents"];
    let change = json!({"type": "change", "data": {"patch": [
        {"op": "replace", "path": "/agents", "value": agents},
    ]}});
    (change.to_string().into(), server.outputs.subscribe())
}

/// One client: it sends `Command`s and receives every `Output` as JSON,
/// starting with a snapshot of the swarm document.
async fn serve(socket: WebSocket, server: Server) -> Result<(), Box<dyn Error + Send + Sync>> {
    let (snapshot_json, mut outputs) = snapshot(&server);
    let (mut sink, mut source) = socket.split();
    sink.send(Message::Text(snapshot_json)).await?;
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
                // Changes it missed would leave its copy wrong, so it starts
                // over from a new snapshot.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(n, "client fell behind, sending a new snapshot");
                    let (snapshot_json, live) = snapshot(&server);
                    outputs = live;
                    sink.send(Message::Text(snapshot_json)).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}
