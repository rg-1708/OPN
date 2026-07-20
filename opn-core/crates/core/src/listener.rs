//! TCP accept wrapper setting `TCP_NODELAY` on every accepted connection.
//!
//! The gateway writes small WS frames back-to-back on one socket (fan-out
//! event, then the command ack). With Nagle on, the second frame sits in the
//! kernel until the peer's delayed ACK (~40 ms) acknowledges the first — a
//! fixed ~40 ms floor on every ack RTT that dwarfs the actual command path
//! (<2 ms). Measured, not guessed: Sprint 10 design-load runs showed ack p50
//! pinned at ~42 ms with near-zero variance while delivery p50 was 1.7 ms.

use std::net::SocketAddr;

use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use tokio::net::{TcpListener, TcpStream};

pub struct NoDelayListener(pub TcpListener);

impl axum::serve::Listener for NoDelayListener {
    type Io = TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.0.accept().await {
                Ok((io, addr)) => {
                    let _ = io.set_nodelay(true);
                    return (io, addr);
                }
                // Transient accept errors (e.g. fd exhaustion) must not kill
                // the accept loop — same policy as axum's own TcpListener impl.
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.0.local_addr()
    }
}

/// Peer address for the WS pre-auth per-IP cap. A newtype because axum's
/// `Connected<…> for SocketAddr` impl is listener-specific and foreign
/// (orphan rule); the local type carries it for both listeners we serve
/// with — the real `NoDelayListener` and the test harness's plain
/// `TcpListener`.
#[derive(Clone, Copy, Debug)]
pub struct ClientAddr(pub SocketAddr);

impl Connected<IncomingStream<'_, NoDelayListener>> for ClientAddr {
    fn connect_info(stream: IncomingStream<'_, NoDelayListener>) -> Self {
        ClientAddr(*stream.remote_addr())
    }
}

impl Connected<IncomingStream<'_, TcpListener>> for ClientAddr {
    fn connect_info(stream: IncomingStream<'_, TcpListener>) -> Self {
        ClientAddr(*stream.remote_addr())
    }
}
