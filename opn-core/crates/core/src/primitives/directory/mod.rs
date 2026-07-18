//! directory primitive (OPN-CORE.md §10.7) — the single number → character
//! choke point (§10.1 note). Nothing else reads `characters.number` directly,
//! so future virtual/burner numbers slot in behind `resolve` without touching
//! callers.
//!
//! Sprint 3 lands only the resolution seam that `channels.open_direct` needs.
//! Contacts, blocks, listings, and block-gating join here in Sprint 5 — at
//! which point `resolve` also filters blocked pairs (§10.7).

use sqlx::{Postgres, Transaction};

/// Resolve a phone number to the character that holds it, within the caller's
/// world (RLS scopes the read). `None` = no such number.
///
/// Runs inside a caller-supplied `world_tx` so it composes into the
/// open_direct / calls.start transactions.
// ponytail: block-checking joins here in Sprint 5 (§10.7) — a blocked number
// must then resolve to None so it is indistinguishable from unknown (privacy).
pub async fn resolve(
    tx: &mut Transaction<'_, Postgres>,
    number: &str,
) -> sqlx::Result<Option<uuid::Uuid>> {
    sqlx::query_scalar("SELECT id FROM characters WHERE number = $1")
        .bind(number)
        .fetch_optional(&mut **tx)
        .await
}
