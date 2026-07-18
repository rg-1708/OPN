//! JWT-authed HTTP extractor (§6, §11): the same session JWT the WS gateway
//! uses, verified for HTTP reads (inbox now; channel history, gallery, ledger
//! from Sprint 4 on). Reuses `infra::auth::verify`, so the live-session check
//! and the `Identity`-only-from-verify invariant hold here too.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::Response;

use crate::http::tenant::err_response;
use crate::infra::auth::{verify, Identity, VerifyError};
use crate::state::AppState;
use contracts::ErrCode;

/// The authenticated character/device behind a `Authorization: Bearer <jwt>`
/// HTTP request.
pub struct JwtIdentity(pub Identity);

impl FromRequestParts<AppState> for JwtIdentity {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let raw = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
            .ok_or_else(|| err_response(ErrCode::Unauthorized, "missing token"))?;

        match verify(&state.pg, &state.cfg.jwt_secret, raw).await {
            Ok(id) => Ok(JwtIdentity(id)),
            Err(VerifyError::Unauthorized) => {
                Err(err_response(ErrCode::Unauthorized, "invalid token"))
            }
            Err(VerifyError::Internal) => Err(err_response(ErrCode::Internal, "internal")),
        }
    }
}
