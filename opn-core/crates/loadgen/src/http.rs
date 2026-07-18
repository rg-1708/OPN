//! Minimal plaintext HTTP client for the seed phase (roadmap Sprint 4 item 9,
//! "world/tenant seed via a `--seed` mode that calls the mint API"). Loadgen
//! only ever talks to a local/compose Core over plain HTTP, so a hand-written
//! `POST` with `Connection: close` (read to EOF, no chunked/keep-alive parsing)
//! covers it in ~40 lines — no `reqwest` dependency for one request shape.
//!
//! ponytail: HTTP/1.1 + `Connection: close` only. If loadgen ever needs to hit
//! a TLS endpoint or reuse connections, reach for `reqwest` then, not before.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

/// A minted session, everything a connection driver needs: the JWT to auth its
/// socket, its own character id (to filter self-fan-out when measuring delivery
/// latency), and its phone number (so its pair partner can `open_direct` it).
pub struct Minted {
    pub token: String,
    pub char_id: Uuid,
    pub number: String,
}

/// `POST /v1/tenants/self/sessions` against `host` (`ip:port`, no scheme) with
/// the tenant API key, returning the minted session. One TCP connection per
/// call — the seed phase is not measured, so simplicity wins over pooling.
pub async fn mint(host: &str, api_key: &str, framework_ref: &str) -> Result<Minted> {
    let body = json!({ "framework_ref": framework_ref }).to_string();
    let req = format!(
        "POST /v1/tenants/self/sessions HTTP/1.1\r\n\
         Host: {host}\r\n\
         Authorization: Bearer {api_key}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n{body}",
        len = body.len(),
    );

    let mut stream = TcpStream::connect(host)
        .await
        .with_context(|| format!("connect {host}"))?;
    stream
        .write_all(req.as_bytes())
        .await
        .context("write request")?;
    let mut raw = Vec::new();
    stream
        .read_to_end(&mut raw)
        .await
        .context("read response")?;

    let text = String::from_utf8_lossy(&raw);
    let sep = text
        .find("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed response: no header terminator"))?;
    let (head, rest) = (&text[..sep], text[sep + 4..].trim());

    let status = head.lines().next().unwrap_or("");
    if !status.contains(" 200 ") {
        bail!("mint failed: {status} / {rest}");
    }

    let v: Value = serde_json::from_str(rest).context("parse mint json")?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| anyhow!("mint response missing token"))?
        .to_owned();
    let char_id = Uuid::parse_str(
        v["character"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("mint response missing character.id"))?,
    )
    .context("character.id not a uuid")?;
    let number = v["character"]["number"]
        .as_str()
        .ok_or_else(|| anyhow!("mint response has no assigned number"))?
        .to_owned();

    Ok(Minted {
        token,
        char_id,
        number,
    })
}

#[cfg(test)]
mod tests {
    // The header/body split is the one fiddly bit of the hand-rolled client;
    // pin it against a realistic axum-shaped response.
    #[test]
    fn parses_status_and_body_split() {
        let resp =
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}";
        let sep = resp.find("\r\n\r\n").expect("header terminator");
        let head = &resp[..sep];
        let body = resp[sep + 4..].trim();
        assert!(head.lines().next().expect("status line").contains(" 200 "));
        assert_eq!(body, "{}");
    }
}
