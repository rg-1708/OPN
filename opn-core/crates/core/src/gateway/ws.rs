//! Connection lifecycle (§4.1): origin pre-check, pre-auth caps, auth within
//! 3 s, takeover, reader/writer split, heartbeat.
//!
//! Origin checking is two-phase: the tenant is unknown pre-auth, so the
//! pre-upgrade check runs against the *union* of all tenants' origins (plus
//! `cfx-nui-*`); the authoritative per-tenant check re-runs after the auth
//! frame resolves the tenant.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use contracts::{Cmd, ErrBody, ErrCode, ServerMsg};
use dashmap::DashMap;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};

use super::registry::{close, ConnHandle};
use crate::infra::auth::{verify, VerifyError};
use crate::infra::db::world_tx;
use crate::state::AppState;

const AUTH_DEADLINE: Duration = Duration::from_secs(3);

/// Sockets that have not yet authenticated (§4.1). Over cap → rejected
/// pre-upgrade, no handshake work.
#[derive(Default)]
pub struct PreauthCaps {
    global: AtomicU32,
    per_ip: DashMap<IpAddr, u32>,
}

impl PreauthCaps {
    fn try_acquire(self: &Arc<Self>, ip: IpAddr, state: &AppState) -> Option<PreauthGuard> {
        if self.global.fetch_add(1, Ordering::AcqRel) >= state.cfg.preauth_global_max {
            self.global.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        let mut per_ip = self.per_ip.entry(ip).or_insert(0);
        if *per_ip >= state.cfg.preauth_per_ip_max {
            drop(per_ip);
            self.global.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        *per_ip += 1;
        drop(per_ip);
        Some(PreauthGuard {
            caps: self.clone(),
            ip,
        })
    }
}

/// Releases both counters on drop — auth success and every failure path
/// alike.
struct PreauthGuard {
    caps: Arc<PreauthCaps>,
    ip: IpAddr,
}

impl Drop for PreauthGuard {
    fn drop(&mut self) {
        self.caps.global.fetch_sub(1, Ordering::AcqRel);
        if let Some(mut n) = self.caps.per_ip.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
        }
        self.caps.per_ip.remove_if(&self.ip, |_, n| *n == 0);
    }
}

/// FiveM NUI origins — always allowed, both phases (§11).
fn is_nui_origin(origin: &str) -> bool {
    origin.starts_with("https://cfx-nui-") || origin.starts_with("nui://")
}

pub async fn ws_handler(
    State(state): State<AppState>,
    ConnectInfo(crate::listener::ClientAddr(addr)): ConnectInfo<crate::listener::ClientAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    // Absent Origin = non-browser client (loadgen, native shells) — allowed;
    // browsers always send it.
    if let Some(o) = &origin {
        if !is_nui_origin(o) {
            match state.tenants.origin_allowed_any(&state.pg, o).await {
                Ok(true) => {}
                Ok(false) => return StatusCode::FORBIDDEN.into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "origin union lookup failed");
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
            }
        }
    }
    let Some(guard) = state.preauth.try_acquire(addr.ip(), &state) else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, origin, guard))
}

async fn handle_socket(
    mut socket: WebSocket,
    state: AppState,
    origin: Option<String>,
    preauth: PreauthGuard,
) {
    // ---- auth phase: first frame must be `auth` within 3 s (§4.1) ----
    let first = match tokio::time::timeout(AUTH_DEADLINE, socket.recv()).await {
        Err(_) => return close_socket(socket, close::UNAUTHORIZED, "auth timeout").await,
        Ok(None) | Ok(Some(Err(_))) => return,
        Ok(Some(Ok(msg))) => msg,
    };
    let Message::Text(text) = first else {
        return close_socket(socket, close::BAD_FIRST_FRAME, "expected auth frame").await;
    };
    let Ok(frame) = serde_json::from_str::<contracts::ClientFrame>(&text) else {
        return close_socket(socket, close::BAD_FIRST_FRAME, "expected auth frame").await;
    };
    let Cmd::Auth { token } = frame.cmd else {
        return close_socket(socket, close::BAD_FIRST_FRAME, "expected auth frame").await;
    };

    let identity = match verify(&state.pg, &state.cfg.jwt_secret, &token).await {
        Ok(id) => id,
        Err(VerifyError::Unauthorized) => {
            return close_socket(socket, close::UNAUTHORIZED, "bad token").await
        }
        Err(VerifyError::Internal) => return close_socket(socket, 1011, "internal").await,
    };

    // ---- authoritative per-tenant origin re-check (phase two) ----
    if let Some(o) = &origin {
        if !is_nui_origin(o) {
            let allowed = match state.tenants.get(&state.pg, identity.tenant_id).await {
                Ok(Some(cfg)) => cfg.allowed_origins.iter().any(|a| a == o),
                Ok(None) => false,
                Err(e) => {
                    tracing::error!(error = %e, "tenant lookup failed");
                    return close_socket(socket, 1011, "internal").await;
                }
            };
            if !allowed {
                return close_socket(socket, close::UNAUTHORIZED, "origin not allowed").await;
            }
        }
    }

    let share_presence = {
        let read = async {
            let mut tx = world_tx(&state.pg, identity.world_id).await?;
            let share: bool =
                sqlx::query_scalar("SELECT share_presence FROM characters WHERE id = $1")
                    .bind(identity.character_id)
                    .fetch_one(&mut *tx)
                    .await?;
            anyhow::Ok(share)
        };
        match read.await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "share_presence read failed");
                return close_socket(socket, 1011, "internal").await;
            }
        }
    };

    drop(preauth); // authenticated — out of the pre-auth caps

    // ---- register (last-writer-wins takeover, §4.1) ----
    let (handle, rx, closed_rx) =
        ConnHandle::new(identity, share_presence, state.cfg.sendq_capacity);
    let (prev, came_online) = state.registry.register(handle.clone());
    if let Some(prev) = prev {
        prev.close(close::TAKEN_OVER);
    }
    super::presence::on_connect(&state, &handle, came_online).await;

    handle.send_ack(ack_frame(frame.id, true, None));

    let (sink, stream) = socket.split();
    let last_pong = Arc::new(Mutex::new(Instant::now()));
    let heartbeat = Duration::from_secs(state.cfg.heartbeat_secs);
    let writer = tokio::spawn(write_loop(
        sink,
        rx,
        closed_rx.clone(),
        heartbeat,
        last_pong.clone(),
        handle.clone(),
    ));

    read_loop(&state, &handle, stream, closed_rx, last_pong).await;

    // ---- cleanup: also runs when takeover or slow-consumer closed us ----
    let went_offline = state.registry.unregister(&handle);
    super::presence::on_disconnect(&state, &handle, went_offline).await;
    handle.close(1000); // stops the writer if nothing closed us earlier
    let _ = writer.await;
}

/// Sequential dispatch (CDR-5): one command at a time per connection; a bad
/// JSON frame acks `invalid` and continues — never closes (§7).
async fn read_loop(
    state: &AppState,
    handle: &Arc<ConnHandle>,
    mut stream: SplitStream<WebSocket>,
    mut closed_rx: watch::Receiver<Option<u16>>,
    last_pong: Arc<Mutex<Instant>>,
) {
    loop {
        let msg = tokio::select! {
            _ = closed_rx.changed() => break,
            msg = stream.next() => msg,
        };
        match msg {
            None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
            Some(Ok(Message::Pong(_))) => {
                if let Ok(mut t) = last_pong.lock() {
                    *t = Instant::now();
                }
            }
            Some(Ok(Message::Text(text))) => {
                match serde_json::from_str::<contracts::ClientFrame>(&text) {
                    Ok(frame) => {
                        let ack =
                            super::dispatch::dispatch(state, handle, frame.id, frame.cmd).await;
                        match serde_json::to_string(&ack) {
                            Ok(s) => handle.send_ack(Arc::from(s)),
                            Err(e) => tracing::error!(error = %e, "ack serialization failed"),
                        }
                    }
                    Err(_) => {
                        // Salvage the id if the JSON at least has one.
                        let id = serde_json::from_str::<serde_json::Value>(&text)
                            .ok()
                            .and_then(|v| v.get("id").and_then(serde_json::Value::as_u64))
                            .unwrap_or(0);
                        handle.send_ack(ack_frame(id, false, Some(ErrCode::Invalid)));
                    }
                }
            }
            // Binary frames are not part of the protocol; axum answers Pings.
            Some(Ok(_)) => {}
        }
    }
}

async fn write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Arc<str>>,
    mut closed_rx: watch::Receiver<Option<u16>>,
    heartbeat: Duration,
    last_pong: Arc<Mutex<Instant>>,
    handle: Arc<ConnHandle>,
) {
    let mut tick = tokio::time::interval(heartbeat);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = closed_rx.changed() => {
                let code = closed_rx.borrow().unwrap_or(1000);
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code,
                        reason: "".into(),
                    })))
                    .await;
                break;
            }
            frame = rx.recv() => match frame {
                Some(f) => {
                    if sink.send(Message::Text(f.to_string().into())).await.is_err() {
                        handle.close(1006);
                        break;
                    }
                }
                None => break,
            },
            _ = tick.tick() => {
                // 2 missed pongs → gone (§4.1).
                let stale = last_pong
                    .lock()
                    .map(|t| t.elapsed() > 2 * heartbeat)
                    .unwrap_or(false);
                if stale {
                    let _ = sink
                        .send(Message::Close(Some(CloseFrame {
                            code: 1001,
                            reason: "heartbeat timeout".into(),
                        })))
                        .await;
                    handle.close(1001);
                    break;
                }
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    handle.close(1006);
                    break;
                }
            }
        }
    }
}

fn ack_frame(reply_to: u64, ok: bool, err: Option<ErrCode>) -> Arc<str> {
    let msg = ServerMsg::Ack {
        reply_to,
        ok,
        payload: None,
        err: err.map(|code| ErrBody {
            code,
            msg: String::new(),
        }),
    };
    match serde_json::to_string(&msg) {
        Ok(s) => Arc::from(s),
        Err(_) => Arc::from("{}"),
    }
}

/// Pre-registration failure path: close with a gateway code, best-effort.
async fn close_socket(mut socket: WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}
