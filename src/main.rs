mod encoder;
mod flow;
mod input;
mod virtual_keyboard;
mod wayland;
use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{Html, IntoResponse},
    routing::get,
};
use bytes::Bytes;
use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;

#[derive(Parser, Clone)]
struct Config {
    #[arg(long, default_value = "127.0.0.1:8082")]
    bind: SocketAddr,
    #[arg(long, default_value = "HDMI-A-1")]
    output: String,
    #[arg(long)]
    cert: PathBuf,
    #[arg(long)]
    key: PathBuf,
}
#[derive(Clone)]
struct App {
    config: Config,
    busy: Arc<AtomicBool>,
}
pub struct Session {
    stop: AtomicBool,
    keyframe: AtomicBool,
    flow: Mutex<flow::Flow>,
}
#[derive(Clone, Copy, Deserialize)]
#[serde(default)]
pub struct StreamConfig {
    width: u32,
    fps: u32,
    bitrate: u32,
}
impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            fps: 60,
            bitrate: 16_000_000,
        }
    }
}
impl StreamConfig {
    fn valid(&self) -> bool {
        [1280, 1920, 2560, 3840].contains(&self.width)
            && [30, 60].contains(&self.fps)
            && (2_000_000..=50_000_000).contains(&self.bitrate)
    }
}
pub enum Outgoing {
    Frame(Bytes),
    Json(serde_json::Value),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hypr2web=info".into()),
        )
        .init();
    let config = Config::parse();
    let bind = config.bind;
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&config.cert, &config.key)
        .await
        .context("load HTTPS certificate")?;
    let app = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../static/index.html")) }),
        )
        .route("/ws", get(upgrade))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(App {
            config,
            busy: Arc::new(AtomicBool::new(false)),
        });
    tracing::info!(%bind,"hypr2web serving HTTPS; one controller at a time");
    axum_server::bind_rustls(bind, tls)
        .serve(app.into_make_service())
        .await?;
    Ok(())
}
async fn upgrade(
    State(app): State<App>,
    Query(config): Query<StreamConfig>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    if !config.valid() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "invalid stream settings",
        )
            .into_response();
    }
    ws.max_message_size(16 * 1024)
        .on_upgrade(move |socket| session(socket, app, config))
        .into_response()
}
struct Guard {
    session: Arc<Session>,
    busy: Arc<AtomicBool>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.session.stop.store(true, Ordering::Relaxed);
        self.busy.store(false, Ordering::Release);
    }
}
async fn session(mut socket: WebSocket, app: App, config: StreamConfig) {
    if app.busy.swap(true, Ordering::AcqRel) {
        let _=socket.send(Message::Text(r#"{"type":"error","message":"Another browser is connected. Disconnect it first."}"#.into())).await;
        return;
    }
    let shared = Arc::new(Session {
        stop: AtomicBool::new(false),
        keyframe: AtomicBool::new(true),
        flow: Mutex::new(flow::Flow::default()),
    });
    let guard = Guard {
        session: shared.clone(),
        busy: app.busy.clone(),
    };
    let (tx, mut rx) = mpsc::channel::<Outgoing>(2);
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel(128);
    // Input is independent of screen capture / encoding.
    // A bounded ingress prevents a slow client from creating an unbounded input backlog.
    let input_shared = shared.clone();
    let input_output = app.config.output.clone();
    let input_errors = tx.clone();
    let input_thread = std::thread::spawn(move || {
        if let Err(e) = input::run(input_output, input_shared.clone(), input_rx) {
            let _ = input_errors.blocking_send(Outgoing::Json(
                serde_json::json!({"type":"error","message":format!("Input: {e:#}")}),
            ));
            input_shared.stop.store(true, Ordering::Relaxed);
        }
    });
    let capture_shared = shared.clone();
    let output = app.config.output.clone();
    let capture_tx = tx.clone();
    let capture = std::thread::spawn(move || {
        if let Err(e) = wayland::capture(config, output, capture_shared.clone(), capture_tx.clone())
            && !capture_shared.stop.load(Ordering::Relaxed)
        {
            tracing::warn!(error=%e,"capture ended");
            let _ = capture_tx.blocking_send(Outgoing::Json(
                serde_json::json!({"type":"error","message":format!("{e:#}")}),
            ));
        }
    });
    let (mut sink, mut source) = socket.split();
    let writer = tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            let fatal = matches!(&item,Outgoing::Json(v) if v["type"]=="error");
            let message = match item {
                Outgoing::Frame(v) => Message::Binary(v),
                Outgoing::Json(v) => Message::Text(v.to_string().into()),
            };
            if !matches!(
                tokio::time::timeout(Duration::from_secs(2), sink.send(message)).await,
                Ok(Ok(()))
            ) {
                break;
            }
            if fatal {
                break;
            }
        }
    });
    let mut stats = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            message=source.next()=>{
                let text=match message {Some(Ok(Message::Text(text)))=>text,Some(Ok(Message::Ping(_)|Message::Pong(_)))=>continue,_=>break};
                if input::validate_size(text.len()).is_err(){break;}
                let Ok(value)=serde_json::from_str::<serde_json::Value>(&text) else {continue};
                match value["type"].as_str(){
                    Some("ack")=>{if let Some(seq)=value["seq"].as_u64().and_then(|n|u32::try_from(n).ok()){shared.flow.lock().unwrap().ack(seq);}},
                    Some("keyframe")=>{shared.keyframe.store(true,Ordering::Relaxed);},
                    Some("ping")=>{let _=tx.try_send(Outgoing::Json(serde_json::json!({"type":"pong","at":value["at"]})));},
                    _=>{if let Ok(event)=serde_json::from_value::<input::Input>(value)&& input_tx.try_send(event).is_err(){break;}}
                }
            },
            _=stats.tick()=>{
                if writer.is_finished()||shared.stop.load(Ordering::Relaxed){break;}
                let flow=shared.flow.lock().unwrap();
                let _=tx.try_send(Outgoing::Json(serde_json::json!({"type":"stats","ackMs":flow.presented_roundtrip_ms,"frames":flow.acknowledged})));
            }
        }
    }
    shared.stop.store(true, Ordering::Relaxed);
    drop(input_tx);
    drop(tx);
    writer.abort();
    let _ = writer.await;
    // Wait off the async executor so all held keys are released before granting another lease.
    let _ = tokio::task::spawn_blocking(move || {
        let _ = input_thread.join();
        let _ = capture.join();
    })
    .await;
    drop(guard);
    tracing::info!("browser disconnected; capture and input stopped");
}
