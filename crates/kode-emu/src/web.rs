//! The page: web/ served as it is, its WebSocket, and its app upload.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tower_http::services::ServeDir;

use crate::bridge::{Bridge, Msg, Page};
use crate::proto;

/// The largest app image a page may upload.
const MAX_APP: usize = 16 * 1024 * 1024;
/// The largest message a page may send (a microphone frame is 8 KB).
const MAX_MSG: usize = 1 << 20;

/// The page's files: `$KODE_EMU_WEB`, or web/ in the checkout this binary
/// was built from. Read
/// from disk on every request, so the page can be edited without a rebuild.
pub fn web_dir() -> PathBuf {
    match std::env::var_os("KODE_EMU_WEB") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../web")),
    }
}

pub fn app(bridge: Arc<Bridge>) -> Router {
    let mut r = Router::new()
        .route("/ws", get(ws))
        .route("/api/stats", get(stats))
        .route("/api/install", post(install))
        .route("/api/mailbox", post(mailbox));
    let web = web_dir();
    if web.exists() {
        r = r.fallback_service(ServeDir::new(web));
    }
    r.with_state(bridge)
}

fn origin(headers: &HeaderMap) -> Option<&str> {
    headers.get("origin").map(|v| v.to_str().unwrap_or("\u{0}"))
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, "kode-emu takes pages from itself only").into_response()
}

async fn ws(State(bridge): State<Arc<Bridge>>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if !bridge.origin_ok(origin(&headers)) {
        return forbidden();
    }
    upgrade.max_message_size(MAX_MSG).on_upgrade(move |socket| page(bridge, socket))
}

async fn page(bridge: Arc<Bridge>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (page, mut queue) = Page::new();
    let mut let_go = page.let_go();
    let id = bridge.join(page);
    let pictures = bridge.clone();
    let writer = tokio::spawn(async move {
        let send = |m: Msg| match m {
            Msg::Text(t) => Message::Text(t.into()),
            Msg::Bin(b) => Message::Binary(b),
        };
        while let Some((m, caught_up)) = queue.next().await {
            if sink.send(send(m)).await.is_err() {
                break;
            }
            if caught_up {
                for m in pictures.picture() {
                    if sink.send(send(m)).await.is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut dec = proto::Decoder::new();
    loop {
        // A page the bridge let go (too far behind) is closed under it,
        // even with its writer stuck on a full socket: it reconnects.
        let m = tokio::select! {
            m = stream.next() => m,
            _ = let_go.wait_for(|g| *g) => break,
        };
        let Some(Ok(m)) = m else { break };
        match m {
            Message::Binary(b) => {
                for (t, p) in dec.feed(&b) {
                    bridge.from_page(t, &p).await;
                }
            }
            Message::Text(t) => {
                if let Ok(j) = serde_json::from_str::<serde_json::Value>(&t) {
                    bridge.from_page_json(&j).await;
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    bridge.leave(id);
    writer.abort();
}

/// How the simulator is doing: `uart_bytes`, what has crossed UART1 from
/// the firmware (screen frames packed, as they travel), `serial_dropped`,
/// the console lines its serial port had no room for, and `sim_ms`, the
/// emulated time since the chip's last reset (0 while it is off): two
/// readings against the wall clock say how fast the Kode Dot runs.
async fn stats(State(bridge): State<Arc<Bridge>>, headers: HeaderMap) -> Response {
    if !bridge.origin_ok(origin(&headers)) {
        return forbidden();
    }
    Json(json!({
        "uart_bytes": bridge.uart_bytes(),
        "serial_dropped": bridge.serial_dropped(),
        "sim_ms": bridge.sim_time_ns().unwrap_or(0) / 1_000_000,
    }))
    .into_response()
}

fn refuse(code: StatusCode, error: &str) -> Response {
    (code, Json(json!({"ok": false, "error": error}))).into_response()
}

/// The upload's image, read only once its Origin is one of ours: a body
/// of up to [`MAX_APP`] is not taken in for a site that may not send it.
async fn upload(bridge: &Bridge, headers: &HeaderMap, body: Body) -> Result<Bytes, Box<Response>> {
    if !bridge.origin_ok(origin(headers)) {
        return Err(Box::new(forbidden()));
    }
    let image = axum::body::to_bytes(body, MAX_APP)
        .await
        .map_err(|_| Box::new(refuse(StatusCode::PAYLOAD_TOO_LARGE, "an app image is 16 MB at most")))?;
    if !crate::install::is_app_image(&image) {
        return Err(Box::new(refuse(StatusCode::BAD_REQUEST, "not an ESP-IDF app image (.bin)")));
    }
    if !bridge.is_running() {
        return Err(Box::new(refuse(StatusCode::CONFLICT, "the Kode Dot is off")));
    }
    Ok(image)
}

/// POST an app built for the simulator: into this Kode Dot's kodeOS.
async fn install(State(bridge): State<Arc<Bridge>>, headers: HeaderMap, body: Body) -> Response {
    let image = match upload(&bridge, &headers, body).await {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let Some(one) = bridge.try_installing() else {
        return refuse(StatusCode::CONFLICT, "an install is already under way");
    };
    match bridge.install_with(one, image.to_vec()).await {
        Ok(()) => Json(json!({"ok": true, "bytes": image.len()})).into_response(),
        Err(e) => refuse(StatusCode::UNPROCESSABLE_ENTITY, &e.0),
    }
}

/// POST an app image: into the mailbox (app_pending), and the chip reset,
/// as an Arduino upload does - `kode-emu install --mailbox`.
async fn mailbox(State(bridge): State<Arc<Bridge>>, headers: HeaderMap, body: Body) -> Response {
    let image = match upload(&bridge, &headers, body).await {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let Some(one) = bridge.try_installing() else {
        return refuse(StatusCode::CONFLICT, "an install is already under way");
    };
    match bridge.mailbox(one, image.to_vec()).await {
        Ok(at) => Json(json!({"ok": true, "bytes": image.len(), "offset": at})).into_response(),
        Err(e) => refuse(StatusCode::UNPROCESSABLE_ENTITY, &e),
    }
}
