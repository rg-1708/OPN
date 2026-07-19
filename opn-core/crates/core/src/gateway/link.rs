//! Tenant link (OPN-CORE.md §5): the server→FXServer push channel. One
//! connection per world, API-key-authed (`TenantAuth`), last-writer-wins.
//! Carries **down-only** voice-target events (`calls.voice`) to the game-server
//! gateway resource; the up-direction carries nothing (all FXServer→Core traffic
//! is plain HTTPS with the API key). Link down = voice targets lag until
//! reconnect; events for a disconnected tenant are dropped by design, and the
//! resource re-syncs active calls on reconnect via
//! `GET /v1/tenants/self/calls/active`.
//!
//! **Keyed by world, not tenant** (the roadmap said `DashMap<TenantId>`): a
//! voice target is world-scoped and every call transition already holds
//! `world_id`, so keying by world removes a world→tenant lookup from the call
//! hot path. This relies on **one tenant per world** — enforced at the creation
//! site (`admin create-tenant` refuses a world that already has a tenant, §5).
//! If two tenants ever shared a world, the second link would take over (4408)
//! the first; multi-tenant hosting (§17) must re-key by tenant (routing voice
//! via the call's world→tenant) before lifting the invariant. `tenant_id` is
//! kept on the handle for logging.
//!
//! Single-replica (§9): the registry is in-process, so `send` is local-only. A
//! call transition on another replica would not reach a link here — cross-replica
//! link routing is a later concern (the same class as the rest of `replicas > 1`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use contracts::{Evt, LinkHello, ServerMsg};
use dashmap::DashMap;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use metrics::counter;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use super::registry::serialize_push;
use crate::http::tenant::TenantAuth;
use crate::state::AppState;

/// First frame (`hello`) must arrive within this window, mirroring the client
/// gateway's auth deadline (§4.1).
const HELLO_DEADLINE: Duration = Duration::from_secs(3);

/// Link close codes (§5). Distinct from the client gateway's `registry::close`
/// so a link operator never confuses a version reject with a slow consumer.
pub mod close {
    /// Missing/malformed hello frame.
    pub const BAD_HELLO: u16 = 4400;
    /// Superseded by a newer link for the same world (last-writer-wins).
    pub const TAKEN_OVER: u16 = 4408;
    /// Known-broken `(resource, contracts)` version combo (§5).
    pub const INCOMPATIBLE: u16 = 4409;
    /// Send queue full on a durable voice event — the resource reconnects and
    /// re-syncs (§5). Deliberately 4410, not the client protocol's 4409, which
    /// this link reuses for INCOMPATIBLE.
    pub const SLOW_CONSUMER: u16 = 4410;
}

static LINK_SEQ: AtomicU64 = AtomicU64::new(0);

/// One live tenant-link connection, owned by the registry and the writer task.
pub struct LinkHandle {
    pub world_id: Uuid,
    pub tenant_id: Uuid,
    /// Disambiguates this link from a takeover successor under the same world.
    link_seq: u64,
    tx: mpsc::Sender<Arc<str>>,
    closed: watch::Sender<Option<u16>>,
}

impl LinkHandle {
    fn new(
        world_id: Uuid,
        tenant_id: Uuid,
        cap: usize,
    ) -> (
        Arc<LinkHandle>,
        mpsc::Receiver<Arc<str>>,
        watch::Receiver<Option<u16>>,
    ) {
        let (tx, rx) = mpsc::channel(cap);
        let (closed, closed_rx) = watch::channel(None);
        let handle = Arc::new(LinkHandle {
            world_id,
            tenant_id,
            link_seq: LINK_SEQ.fetch_add(1, Ordering::Relaxed),
            tx,
            closed,
        });
        (handle, rx, closed_rx)
    }

    /// First close code wins; later calls are no-ops.
    pub fn close(&self, code: u16) {
        self.closed.send_if_modified(|c| {
            if c.is_none() {
                *c = Some(code);
                true
            } else {
                false
            }
        });
    }

    pub fn is_closed(&self) -> bool {
        self.closed.borrow().is_some()
    }

    /// Durable delivery (§5): link events are all durable-class, so a full queue
    /// closes the link — the resource reconnects and re-syncs. No ephemeral path.
    fn send_durable(&self, frame: Arc<str>) {
        if self.tx.try_send(frame).is_err() {
            counter!("opn_sendq_drops_total", "class" => "link_close").increment(1);
            self.close(close::SLOW_CONSUMER);
        }
    }
}

/// World → live link connection. `send` is the one path primitives use to push
/// a down event to a tenant's FXServer.
#[derive(Default)]
pub struct LinkRegistry {
    links: DashMap<Uuid, Arc<LinkHandle>>,
}

impl LinkRegistry {
    /// Registers under last-writer-wins (§5): returns the previous handle, which
    /// the caller must `close(TAKEN_OVER)`.
    pub fn register(&self, handle: Arc<LinkHandle>) -> Option<Arc<LinkHandle>> {
        self.links.insert(handle.world_id, handle)
    }

    /// Cleanup for one *connection* — not blindly the world's slot: after a
    /// takeover the world maps to the successor, which must survive.
    pub fn unregister(&self, handle: &Arc<LinkHandle>) {
        self.links
            .remove_if(&handle.world_id, |_, cur| cur.link_seq == handle.link_seq);
    }

    /// Push a down event to a world's link, if one is connected. Serialized once;
    /// a disconnected world drops the event (the resource re-syncs on connect).
    pub fn send(&self, world: Uuid, evt: &Evt) {
        if let Some(handle) = self.links.get(&world) {
            handle.send_durable(serialize_push("link", evt));
        }
    }

    #[cfg(test)]
    pub fn is_connected(&self, world: Uuid) -> bool {
        self.links.contains_key(&world)
    }
}

/// Known-broken `(resource, contracts)` version combos (§5). Empty at v1 — the
/// hello field is the seam, enforcement slots in without a protocol change.
// ponytail: hardcoded false. Wire a config list here if a real incompatibility
// ever ships; the close path (INCOMPATIBLE) already exists and is exercised by
// this returning true.
fn is_broken_combo(_hello: &LinkHello) -> bool {
    false
}

/// `GET /link` (§5): API-key-authed WS upgrade. `TenantAuth` runs as an
/// extractor, so an unknown/missing key is rejected pre-upgrade (401) exactly
/// like the other tenant routes. No origin check or pre-auth caps — the link is
/// a native FXServer, not a browser, and the API key is the admission gate.
pub async fn link_handler(
    State(state): State<AppState>,
    tenant: TenantAuth,
    ws: WebSocketUpgrade,
) -> Response {
    ws.on_upgrade(move |socket| {
        handle_link_socket(socket, state, tenant.world_id, tenant.tenant_id)
    })
}

async fn handle_link_socket(
    mut socket: WebSocket,
    state: AppState,
    world_id: Uuid,
    tenant_id: Uuid,
) {
    // ---- hello phase: first frame must be a `hello` within the deadline ----
    let first = match tokio::time::timeout(HELLO_DEADLINE, socket.recv()).await {
        Err(_) => return close_socket(socket, close::BAD_HELLO, "hello timeout").await,
        Ok(None) | Ok(Some(Err(_))) => return,
        Ok(Some(Ok(msg))) => msg,
    };
    let Message::Text(text) = first else {
        return close_socket(socket, close::BAD_HELLO, "expected hello frame").await;
    };
    let Ok(hello) = serde_json::from_str::<LinkHello>(&text) else {
        return close_socket(socket, close::BAD_HELLO, "expected hello frame").await;
    };
    tracing::info!(
        world = %world_id,
        tenant = %tenant_id,
        resource_version = %hello.resource_version,
        contracts_version = %hello.contracts_version,
        "tenant link hello",
    );
    if is_broken_combo(&hello) {
        return close_socket(socket, close::INCOMPATIBLE, "incompatible versions").await;
    }

    // ---- register (last-writer-wins takeover, §5) ----
    let (handle, rx, closed_rx) = LinkHandle::new(world_id, tenant_id, state.cfg.sendq_capacity);
    // Enqueue the hello ack before registering so it is the first frame the
    // writer drains — a voice event that races registration then queues behind
    // it, never ahead. Same envelope as the client protocol (§5).
    handle.send_durable(hello_ack());
    if let Some(prev) = state.links.register(handle.clone()) {
        prev.close(close::TAKEN_OVER);
    }

    let (sink, stream) = socket.split();
    let last_pong = Arc::new(Mutex::new(Instant::now()));
    let heartbeat = Duration::from_secs(state.cfg.heartbeat_secs);
    let writer = tokio::spawn(link_write_loop(
        sink,
        rx,
        closed_rx.clone(),
        heartbeat,
        last_pong.clone(),
        handle.clone(),
    ));

    link_read_loop(stream, closed_rx, last_pong).await;

    // ---- cleanup: also runs when takeover or slow-consumer closed us ----
    state.links.unregister(&handle);
    handle.close(1000); // stops the writer if nothing closed us earlier
    let _ = writer.await;
}

/// Up-direction is nothing (§5): the reader only tracks pongs and the close /
/// takeover signal. Any text/binary frame from the resource is ignored, never a
/// close — a stray frame is not the protocol's business.
async fn link_read_loop(
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
            // Up carries nothing; ignore stray frames, don't close.
            Some(Ok(_)) => {}
        }
    }
}

async fn link_write_loop(
    mut sink: SplitSink<WebSocket, Message>,
    mut rx: mpsc::Receiver<Arc<str>>,
    mut closed_rx: watch::Receiver<Option<u16>>,
    heartbeat: Duration,
    last_pong: Arc<Mutex<Instant>>,
    handle: Arc<LinkHandle>,
) {
    let mut tick = tokio::time::interval(heartbeat);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = closed_rx.changed() => {
                let code = closed_rx.borrow().unwrap_or(1000);
                let _ = sink
                    .send(Message::Close(Some(CloseFrame { code, reason: "".into() })))
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
                // 2 missed pongs → gone (§4.1). Reaps a crashed FXServer that
                // never reconnects to trigger a takeover.
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

/// The link hello ack — same `ServerMsg::Ack` envelope as the client protocol
/// (§5), `reply_to: 0` since the hello has no frame id. Lets the resource (and
/// tests) confirm the link is live before the first voice event.
fn hello_ack() -> Arc<str> {
    let msg = ServerMsg::Ack {
        reply_to: 0,
        ok: true,
        payload: None,
        err: None,
    };
    match serde_json::to_string(&msg) {
        Ok(s) => Arc::from(s),
        Err(_) => Arc::from("{}"),
    }
}

/// Pre-registration failure path: close with a link code, best-effort.
async fn close_socket(mut socket: WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::ids::new_id;
    use contracts::VoiceAction;

    fn handle(world: Uuid, cap: usize) -> (Arc<LinkHandle>, mpsc::Receiver<Arc<str>>) {
        let (h, rx, _closed) = LinkHandle::new(world, new_id(), cap);
        (h, rx)
    }

    fn voice() -> Evt {
        Evt::CallsVoice {
            call_id: Uuid::nil(),
            action: VoiceAction::Clear,
            characters: Vec::new(),
        }
    }

    #[tokio::test]
    async fn durable_full_closes_link() {
        let (h, _rx) = handle(new_id(), 2);
        h.send_durable(Arc::from("a"));
        h.send_durable(Arc::from("b"));
        assert!(!h.is_closed());
        h.send_durable(Arc::from("c")); // queue full → SLOW_CONSUMER
        assert!(h.is_closed());
    }

    /// Takeover returns the old handle, and the old connection's cleanup must
    /// not evict the successor registered under the same world.
    #[tokio::test]
    async fn register_takeover_and_seq_guard() {
        let reg = LinkRegistry::default();
        let world = new_id();
        let (old, _r1) = handle(world, 4);
        let (new, _r2) = handle(world, 4);
        assert!(reg.register(old.clone()).is_none());
        assert!(reg.register(new.clone()).is_some(), "takeover returns old");
        reg.unregister(&old);
        assert!(reg.is_connected(world), "successor survives old unregister");
        reg.unregister(&new);
        assert!(!reg.is_connected(world));
    }

    #[tokio::test]
    async fn send_reaches_connected_world_only() {
        let reg = LinkRegistry::default();
        let world = new_id();
        let (h, mut rx) = handle(world, 4);
        reg.register(h.clone());
        reg.send(world, &voice());
        assert!(rx.try_recv().is_ok(), "connected world receives");
        reg.send(new_id(), &voice()); // no link for that world → silent no-op
    }
}
