//! WS protocol test harness (roadmap Sprint 2). A live server on an ephemeral
//! port plus a tungstenite client with frame-level ergonomics: `cmd()` sends a
//! frame and returns its ack (buffering any pushes seen on the way),
//! `expect_evt`/`expect_close` assert on the async stream. These are the
//! backbone of every future primitive's gateway tests, so they panic loudly on
//! anything unexpected — `expect` is the contract, not a smell.

#![allow(dead_code)] // each test binary compiles its own subset of common/

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use opn_core::infra::auth::Identity;
use opn_core::state::AppState;
use serde_json::{json, Value};
use sqlx::PgPool;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

/// A running `app_router` on `127.0.0.1:0`. The serving task is aborted when
/// the guard drops, so a test never leaks a listener.
pub struct TestServer {
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind an ephemeral port and serve the real router. `connect_info` is
/// required — the WS handler extracts `ConnectInfo<SocketAddr>`.
pub async fn spawn_server(state: AppState) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let router = opn_core::http::app_router(state);
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<opn_core::listener::ClientAddr>(),
        )
        .await
        .expect("serve");
    });
    TestServer { addr, task }
}

/// Mint a real session + JWT the way `SessionMintResponse` clients do, then
/// hand back the token and the `Identity` behind it (device/character ids the
/// authz tests assert against). `jwt_secret` in `test_config` is `"test"`.
pub async fn mint_token(
    app: &PgPool,
    tenant: Uuid,
    world: Uuid,
    framework_ref: &str,
) -> (String, Identity) {
    let minted =
        opn_core::primitives::identity::mint_session(app, tenant, world, framework_ref, None, 600)
            .await
            .expect("mint_session");
    let token = opn_core::infra::auth::mint_jwt("test", &minted.identity).expect("mint_jwt");
    (token, minted.identity)
}

/// Like [`mint_token`] but returns the full mint result — open_direct and
/// directory tests need the character's assigned phone number, which
/// `Minted.character.number` carries.
pub async fn mint_full(
    app: &PgPool,
    tenant: Uuid,
    world: Uuid,
    framework_ref: &str,
) -> (String, opn_core::primitives::identity::Minted) {
    let minted =
        opn_core::primitives::identity::mint_session(app, tenant, world, framework_ref, None, 600)
            .await
            .expect("mint_session");
    let token = opn_core::infra::auth::mint_jwt("test", &minted.identity).expect("mint_jwt");
    (token, minted)
}

/// `{"id":id,"cmd":"auth","payload":{"token":token}}` — the mandatory first
/// frame. JWTs and the test garbage tokens are all JSON-safe (no quotes).
pub fn auth_frame(id: u64, token: &str) -> String {
    json!({ "id": id, "cmd": "auth", "payload": { "token": token } }).to_string()
}

/// A connected client. `next_id` autoincrements frame ids; `pushes` buffers
/// Push frames `cmd()` skims past while waiting for its ack, so a later
/// `expect_evt` still sees them (snapshot-before-ack, §4.4).
pub struct TestClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    pushes: std::collections::VecDeque<Value>,
}

/// Raw connect to `ws://addr/ws` — no auth frame sent.
pub async fn connect(addr: SocketAddr) -> TestClient {
    let (ws, _) = connect_async(format!("ws://{addr}/ws"))
        .await
        .expect("ws connect");
    TestClient {
        ws,
        next_id: 1,
        pushes: std::collections::VecDeque::new(),
    }
}

/// Connect to `ws://addr/link` with the tenant API key header — the tenant link
/// (§5). No hello sent; the caller drives the handshake. Returns `Err` if the
/// upgrade is rejected (e.g. a bad API key → 401).
pub async fn connect_link(
    addr: SocketAddr,
    api_key: &str,
) -> Result<TestClient, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{addr}/link")
        .into_client_request()
        .expect("link request");
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {api_key}").parse().expect("auth header"),
    );
    let (ws, _) = connect_async(req).await?;
    Ok(TestClient {
        ws,
        next_id: 1,
        pushes: std::collections::VecDeque::new(),
    })
}

/// Connect the link and complete the hello handshake, asserting the ack is `ok`.
pub async fn connect_link_hello(addr: SocketAddr, api_key: &str) -> TestClient {
    let mut link = connect_link(addr, api_key).await.expect("link connect");
    let hello = json!({ "resource_version": "test", "contracts_version": "test" }).to_string();
    link.send_raw(&hello).await;
    let ack = link.recv_ack(Duration::from_secs(5)).await;
    assert_eq!(ack["ok"], json!(true), "link hello ack not ok: {ack}");
    assert_eq!(ack["reply_to"], json!(0), "link hello ack reply_to: {ack}");
    link
}

/// Connect and complete the auth handshake, asserting the ack is `ok`.
pub async fn connect_and_auth(addr: SocketAddr, token: &str) -> TestClient {
    let mut client = connect(addr).await;
    let id = client.take_id();
    client.send_raw(&auth_frame(id, token)).await;
    let ack = client.read_ack(id).await;
    assert_eq!(ack["ok"], json!(true), "auth ack not ok: {ack}");
    client
}

impl TestClient {
    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Send a command object (e.g. `json!({"cmd":"identity.me"})`), stamp the
    /// next id in, and return the matching ack. Push frames seen before the
    /// ack are buffered for `expect_evt`.
    pub async fn cmd(&mut self, cmd: Value) -> Value {
        let id = self.take_id();
        let mut frame = cmd.as_object().expect("cmd is a JSON object").clone();
        frame.insert("id".into(), json!(id));
        self.send_raw(&Value::Object(frame).to_string()).await;
        self.read_ack(id).await
    }

    pub async fn send_raw(&mut self, text: &str) {
        self.ws
            .send(Message::Text(text.to_owned().into()))
            .await
            .expect("send frame");
    }

    /// Read frames until the ack with `reply_to == id`; buffer any pushes on
    /// the way. Uses a generous timeout — a missing ack is a test failure, not
    /// a hang.
    async fn read_ack(&mut self, id: u64) -> Value {
        let deadline = Duration::from_secs(5);
        loop {
            let v = self.next_json(deadline).await;
            if v.get("topic").is_some() {
                self.pushes.push_back(v);
                continue;
            }
            match v.get("reply_to").and_then(Value::as_u64) {
                Some(r) if r == id => return v,
                other => panic!("expected ack reply_to={id}, got reply_to={other:?}: {v}"),
            }
        }
    }

    /// Next ack frame (has `reply_to`), for frames the server salvages an id
    /// for rather than echoing ours — e.g. mid-connection garbage acked as
    /// `reply_to: 0`. Buffers pushes seen first.
    pub async fn recv_ack(&mut self, timeout: Duration) -> Value {
        loop {
            let v = self.next_json(timeout).await;
            if v.get("topic").is_some() {
                self.pushes.push_back(v);
                continue;
            }
            return v;
        }
    }

    /// Next Push frame (has a `topic`): buffered first, else read from the
    /// stream. Panics on timeout or if a non-push arrives.
    pub async fn expect_evt(&mut self, timeout: Duration) -> Value {
        if let Some(v) = self.pushes.pop_front() {
            return v;
        }
        let v = self.next_json(timeout).await;
        assert!(v.get("topic").is_some(), "expected a push, got: {v}");
        v
    }

    /// Assert NO push arrives within `timeout` (negative presence assertions).
    pub async fn expect_no_evt(&mut self, timeout: Duration) {
        if let Some(v) = self.pushes.pop_front() {
            panic!("expected no push, had one buffered: {v}");
        }
        match tokio::time::timeout(timeout, self.ws.next()).await {
            Err(_) => {} // timed out — the desired outcome
            Ok(Some(Ok(Message::Text(t)))) => panic!("expected no push, got frame: {t}"),
            Ok(Some(Ok(Message::Ping(_)))) => {} // heartbeat, ignore
            Ok(other) => panic!("expected no push, got: {other:?}"),
        }
    }

    /// Read until a Close frame, returning its code. Skips Ping frames
    /// (tungstenite auto-pongs on read) so a queued heartbeat never masks the
    /// close under test.
    pub async fn expect_close(&mut self, timeout: Duration) -> u16 {
        let overall = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = overall.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.ws.next()).await {
                Err(_) => panic!("timed out waiting for close frame"),
                Ok(None) => panic!("stream ended without a close frame"),
                Ok(Some(Ok(Message::Close(Some(frame))))) => return u16::from(frame.code),
                Ok(Some(Ok(Message::Close(None)))) => panic!("close frame without a code"),
                Ok(Some(Ok(Message::Ping(_)))) => continue,
                Ok(Some(Ok(other))) => panic!("expected close, got frame: {other:?}"),
                Ok(Some(Err(e))) => panic!("ws error waiting for close: {e}"),
            }
        }
    }

    /// Idle-soak support: read (auto-ponging pings) until the server closes.
    /// Returns the close code, `None` if the transport ends without one.
    pub async fn hold_until_close(mut self) -> Option<u16> {
        loop {
            match self.ws.next().await {
                None | Some(Err(_)) => return None,
                Some(Ok(Message::Close(c))) => return c.map(|f| u16::from(f.code)),
                Some(Ok(_)) => {}
            }
        }
    }

    /// One JSON text frame, skipping Pings. Panics on timeout/close/error —
    /// callers that expect a close use `expect_close` instead.
    async fn next_json(&mut self, timeout: Duration) -> Value {
        let overall = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = overall.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.ws.next()).await {
                Err(_) => panic!("timed out waiting for a frame"),
                Ok(None) => panic!("stream ended unexpectedly"),
                Ok(Some(Ok(Message::Text(t)))) => {
                    return serde_json::from_str(&t)
                        .unwrap_or_else(|e| panic!("frame not JSON ({e}): {t}"));
                }
                Ok(Some(Ok(Message::Ping(_)))) => continue,
                Ok(Some(Ok(Message::Close(c)))) => panic!("unexpected close frame: {c:?}"),
                Ok(Some(Ok(other))) => panic!("unexpected frame: {other:?}"),
                Ok(Some(Err(e))) => panic!("ws error: {e}"),
            }
        }
    }
}
