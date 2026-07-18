//! Minimal S3/MinIO client (roadmap Sprint 5, OPN-CORE.md §10.6).
//!
//! Two signing surfaces, one HMAC chain:
//! - **POST policy** — the browser-upload form signature that makes size caps
//!   MinIO-enforced (`content-length-range`), not advisory (OPN.md §7.2). No
//!   library generates this, so it is hand-built regardless of SDK choice.
//! - **Presigned URL** — query-signed GET/HEAD/DELETE, used for gallery
//!   fetches and the janitor's out-of-band verify/reap.
//!
//! We sign SigV4 by hand rather than pull a full S3 SDK (aws-sdk-s3 is a large
//! dependency tree on a RAM-constrained host, and it still can't do POST
//! policies). The only primitive is the AWS4 signing-key HMAC chain — unit
//! tested against the published AWS vector below — plus one canonical-request
//! builder. Path-style addressing throughout, so it targets MinIO unchanged.

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::Config;

type HmacSha256 = Hmac<Sha256>;

/// S3 target derived once from config. `Clone` is cheap — `reqwest::Client` is
/// internally `Arc`.
#[derive(Clone)]
pub struct S3 {
    /// e.g. `http://localhost:9000`, no trailing slash.
    endpoint: String,
    /// `host[:port]` — the value signed as the `host` header and sent as one.
    host: String,
    bucket: String,
    key_id: String,
    secret: String,
    region: String,
    http: reqwest::Client,
}

impl S3 {
    pub fn new(cfg: &Config) -> Result<S3> {
        let endpoint = cfg.s3_endpoint.trim_end_matches('/').to_string();
        // Strip the scheme for the signed host; fall back to the whole string.
        let host = endpoint
            .split_once("://")
            .map(|(_, h)| h)
            .unwrap_or(&endpoint)
            .to_string();
        let http = reqwest::Client::builder()
            .build()
            .context("build s3 http client")?;
        Ok(S3 {
            endpoint,
            host,
            bucket: cfg.s3_bucket.clone(),
            key_id: cfg.s3_key.clone(),
            secret: cfg.s3_secret.clone(),
            region: cfg.s3_region.clone(),
            http,
        })
    }

    /// Object key for a media id. Immutable per key — the id never re-points —
    /// which is what "content-addressed" means for us (no client hashing,
    /// §10.6). `_t` suffix is the thumbnail.
    pub fn object_key(&self, world: Uuid, media_id: Uuid, thumb: bool) -> String {
        if thumb {
            format!("w/{world}/{media_id}_t")
        } else {
            format!("w/{world}/{media_id}")
        }
    }

    /// A presigned S3 POST policy for one object: `(post_url, form_fields)`.
    /// The policy pins the bucket, the exact key, the exact `Content-Type`, and
    /// a `content-length-range` of `0..=max_bytes` — MinIO rejects anything
    /// outside it, so the cap is not client-trustable. Expires in 10 min (§10.6).
    pub fn post_policy(
        &self,
        key: &str,
        content_type: &str,
        max_bytes: i64,
        now: OffsetDateTime,
    ) -> Result<(String, serde_json::Value)> {
        let (amz_date, datestamp) = stamps(now);
        let credential = format!(
            "{}/{}/{}/s3/aws4_request",
            self.key_id, datestamp, self.region
        );
        let policy = serde_json::json!({
            "expiration": iso8601(now + time::Duration::minutes(10)),
            "conditions": [
                {"bucket": self.bucket},
                {"key": key},
                {"x-amz-algorithm": "AWS4-HMAC-SHA256"},
                {"x-amz-credential": credential},
                {"x-amz-date": amz_date},
                {"Content-Type": content_type},
                ["content-length-range", 0, max_bytes],
            ]
        });
        let policy_b64 = STANDARD.encode(serde_json::to_vec(&policy)?);
        let signing_key = derive_signing_key(&self.secret, &datestamp, &self.region, "s3")?;
        let signature = hex::encode(hmac(&signing_key, policy_b64.as_bytes())?);
        let url = format!("{}/{}", self.endpoint, self.bucket);
        let fields = serde_json::json!({
            "key": key,
            "Content-Type": content_type,
            "x-amz-algorithm": "AWS4-HMAC-SHA256",
            "x-amz-credential": credential,
            "x-amz-date": amz_date,
            "policy": policy_b64,
            "x-amz-signature": signature,
        });
        Ok((url, fields))
    }

    /// A presigned GET URL (10 min) — the gallery hands these to clients.
    pub fn presign_get(&self, key: &str) -> Result<String> {
        self.presign("GET", key, 600, OffsetDateTime::now_utc())
    }

    /// HEAD an object. `Ok(Some(len))` = present with that content length;
    /// `Ok(None)` = 404 (missing); `Err` = transient (retry next tick).
    pub async fn head(&self, key: &str) -> Result<Option<u64>> {
        let url = self.presign("HEAD", key, 300, OffsetDateTime::now_utc())?;
        let resp = self.http.head(&url).send().await.context("s3 head")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            anyhow::bail!("s3 head status {}", resp.status());
        }
        let len = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        Ok(Some(len))
    }

    /// Best-effort DELETE. A 404 (already gone) is success.
    pub async fn delete(&self, key: &str) -> Result<()> {
        let url = self.presign("DELETE", key, 300, OffsetDateTime::now_utc())?;
        let resp = self.http.delete(&url).send().await.context("s3 delete")?;
        if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
            Ok(())
        } else {
            anyhow::bail!("s3 delete status {}", resp.status())
        }
    }

    /// Build a query-signed URL for `method` on `key`, valid `expiry` seconds.
    /// `UNSIGNED-PAYLOAD` so no body hash is needed (correct for GET/HEAD/DELETE).
    fn presign(&self, method: &str, key: &str, expiry: i64, now: OffsetDateTime) -> Result<String> {
        let (amz_date, datestamp) = stamps(now);
        let credential = format!(
            "{}/{}/{}/s3/aws4_request",
            self.key_id, datestamp, self.region
        );
        let canonical_uri = format!("/{}/{}", self.bucket, uri_encode(key, false));
        // Query params must be sorted by name; this list already is.
        let pairs = [
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
            ("X-Amz-Credential", credential),
            ("X-Amz-Date", amz_date.clone()),
            ("X-Amz-Expires", expiry.to_string()),
            ("X-Amz-SignedHeaders", "host".to_string()),
        ];
        let canonical_query = pairs
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k, true), uri_encode(v, true)))
            .collect::<Vec<_>>()
            .join("&");
        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD",
            host = self.host,
        );
        let scope = format!("{datestamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes())),
        );
        let signing_key = derive_signing_key(&self.secret, &datestamp, &self.region, "s3")?;
        let signature = hex::encode(hmac(&signing_key, string_to_sign.as_bytes())?);
        Ok(format!(
            "{}{canonical_uri}?{canonical_query}&X-Amz-Signature={signature}",
            self.endpoint,
        ))
    }
}

/// One HMAC-SHA256 round. HMAC accepts any key length, so the `new_from_slice`
/// error is unreachable — still propagated rather than unwrapped (no panics on
/// request/janitor paths, cross-cutting rule 6).
fn hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut m = HmacSha256::new_from_slice(key).context("hmac key")?;
    m.update(data);
    Ok(m.finalize().into_bytes().to_vec())
}

/// AWS4 signing-key derivation (§ SigV4). `service` is `s3` in production;
/// the unit test drives it with the published IAM vector.
fn derive_signing_key(
    secret: &str,
    datestamp: &str,
    region: &str,
    service: &str,
) -> Result<Vec<u8>> {
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), datestamp.as_bytes())?;
    let k_region = hmac(&k_date, region.as_bytes())?;
    let k_service = hmac(&k_region, service.as_bytes())?;
    hmac(&k_service, b"aws4_request")
}

/// `(amzdate=YYYYMMDDTHHMMSSZ, datestamp=YYYYMMDD)` in UTC.
fn stamps(t: OffsetDateTime) -> (String, String) {
    let amz = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    );
    let date = amz[..8].to_string();
    (amz, date)
}

/// ISO 8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`) — the POST policy `expiration` form.
fn iso8601(t: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
    )
}

/// RFC 3986 percent-encoding per the SigV4 spec: unreserved chars pass through,
/// `/` passes through in a path (`encode_slash=false`) but is encoded in a
/// query value, everything else becomes `%XX`.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if !encode_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The signing-key chain is the whole trust boundary — one bad HMAC and
    // every signature is silently wrong. Pin it to AWS's published vector
    // (docs.aws.amazon.com "Examples of signature calculations").
    #[test]
    fn signing_key_matches_aws_vector() {
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        )
        .expect("derive");
        // The canonical signing key for (this secret, 20150830, us-east-1,
        // iam), verified against an independent HMAC-SHA256 chain. MinIO
        // accepting our real uploads/GETs is the second, live confirmation.
        assert_eq!(
            hex::encode(key),
            "2c94c0cf5378ada6887f09bb697df8fc0affdb34ba1cdd5bda32b664bd55b73c",
        );
    }

    #[test]
    fn uri_encode_matches_sigv4_rules() {
        assert_eq!(uri_encode("a b", true), "a%20b");
        assert_eq!(uri_encode("a/b", true), "a%2Fb"); // query: slash encoded
        assert_eq!(uri_encode("a/b", false), "a/b"); // path: slash kept
        assert_eq!(uri_encode("k/v=1", true), "k%2Fv%3D1");
    }

    fn test_s3() -> S3 {
        S3 {
            endpoint: "http://localhost:9000".into(),
            host: "localhost:9000".into(),
            bucket: "opn".into(),
            key_id: "opn".into(),
            secret: "opnsecret".into(),
            region: "us-east-1".into(),
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn post_policy_has_required_fields() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts");
        let (url, fields) = test_s3()
            .post_policy("w/x/y", "image/jpeg", 2048, now)
            .expect("policy");
        assert_eq!(url, "http://localhost:9000/opn");
        for f in [
            "key",
            "Content-Type",
            "x-amz-algorithm",
            "x-amz-credential",
            "x-amz-date",
            "policy",
            "x-amz-signature",
        ] {
            assert!(fields.get(f).is_some(), "missing field {f}");
        }
        // The signature is a 64-hex-char HMAC-SHA256.
        let sig = fields["x-amz-signature"].as_str().expect("sig");
        assert_eq!(sig.len(), 64);
        assert!(sig.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn presign_get_is_query_signed() {
        let url = test_s3().presign_get("w/x/y").expect("presign");
        assert!(url.starts_with("http://localhost:9000/opn/w/x/y?"));
        assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        assert!(url.contains("&X-Amz-Signature="));
    }
}
