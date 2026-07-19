//! Sprint 9 generated RLS audit (roadmap item 4, OPN-CORE.md §15): exhaustive,
//! catalog-driven coverage that replaces the hand-written per-primitive canaries.
//! It enumerates every `public` table carrying a `world_id` column straight from
//! the Postgres catalog and asserts each one ENABLEs *and* FORCEs row-level
//! security and carries a policy whose predicate references `world_id`. A new
//! domain table that forgets its RLS clause (the Sprint 0 groundwork convention:
//! ENABLE + FORCE + `USING (world_id = current_setting('app.world_id')…)`) fails
//! this test — no reviewer discipline required, which is the whole point of a
//! "generated per-table" audit over remembering to add a canary.
//!
//! This proves the *mechanism* is present on every world-scoped table. The
//! *behavior* (a second world reads empty) stays proven by `rls_canary` and by
//! each primitive's own cross-world test; together they are the isolation story.

use sqlx::PgPool;

/// Infra tables that carry a `world_id` column but are deliberately NOT
/// RLS-protected: they are not world-scoped *domain* rows, they are accessed only
/// by infra code paths, and isolation is enforced by withholding `opn_app` grants
/// rather than by a row policy (roadmap Sprint 1 item 1; OPN-CORE.md §10.1).
/// Every entry must actually exist in the enumerated set — a stale name (renamed
/// or dropped table) fails the test, so this allowlist cannot silently rot into
/// masking a table that *should* be audited.
const INFRA_EXCEPTIONS: &[&str] = &["tenants"];

#[derive(sqlx::FromRow, Debug)]
struct TableRls {
    relname: String,
    enabled: bool,
    forced: bool,
    has_policy: bool,
}

/// Every world-scoped table must ENABLE + FORCE RLS and carry a world_id policy.
/// Partition *children* (`relispartition`) are excluded — RLS on the partitioned
/// parent (`messages`) governs all access through it, which is how the app queries.
#[sqlx::test(migrator = "opn_core::MIGRATOR")]
async fn every_world_scoped_table_forces_rls(admin: PgPool) {
    // Enumerate from the catalog: ordinary ('r') + partitioned ('p') public
    // tables that have a live `world_id` column, joined to their RLS flags and to
    // whether any policy predicate mentions `world_id`.
    let rows: Vec<TableRls> = sqlx::query_as(
        "SELECT c.relname, \
                c.relrowsecurity AS enabled, \
                c.relforcerowsecurity AS forced, \
                EXISTS ( \
                  SELECT 1 FROM pg_policies p \
                  WHERE p.schemaname = 'public' AND p.tablename = c.relname \
                    AND p.qual LIKE '%world_id%' \
                ) AS has_policy \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'public' \
           AND c.relkind IN ('r', 'p') \
           AND NOT c.relispartition \
           AND EXISTS ( \
             SELECT 1 FROM pg_attribute a \
             WHERE a.attrelid = c.oid AND a.attname = 'world_id' AND NOT a.attisdropped \
           ) \
         ORDER BY c.relname",
    )
    .fetch_all(&admin)
    .await
    .expect("catalog audit query");

    // A broken enumeration passing vacuously is the failure mode a generated
    // audit must guard against: there are a dozen world-scoped tables by Sprint 8
    // (identity set, inbox, channels/messages, reactions/pins, media, directory,
    // calls, ledger, exchanges, feed, plus the canary). If far fewer show up, the
    // query — not the schema — regressed.
    assert!(
        rows.len() >= 10,
        "RLS audit found only {} world-scoped tables — enumeration likely broke: {rows:#?}",
        rows.len()
    );

    // Keep the allowlist honest: every exception must still name a real
    // world_id-bearing table, else its coverage claim is stale.
    for ex in INFRA_EXCEPTIONS {
        assert!(
            rows.iter().any(|t| t.relname == *ex),
            "INFRA_EXCEPTIONS names `{ex}`, which is not a world_id table — stale allowlist entry"
        );
    }

    let offenders: Vec<String> = rows
        .iter()
        .filter(|t| !INFRA_EXCEPTIONS.contains(&t.relname.as_str()))
        .filter(|t| !(t.enabled && t.forced && t.has_policy))
        .map(|t| {
            format!(
                "{}: enabled={} forced={} world_id_policy={}",
                t.relname, t.enabled, t.forced, t.has_policy
            )
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "world-scoped tables missing ENABLE/FORCE RLS or a world_id policy \
         (add to INFRA_EXCEPTIONS only with a documented reason):\n  {}",
        offenders.join("\n  ")
    );

    // Coverage is legible with `--nocapture`; a reviewer can eyeball the set.
    let forced: Vec<&str> = rows
        .iter()
        .map(|t| t.relname.as_str())
        .filter(|n| !INFRA_EXCEPTIONS.contains(n))
        .collect();
    println!(
        "RLS audit: {} world-scoped tables FORCE RLS: {forced:?} (infra exceptions, not RLS: {INFRA_EXCEPTIONS:?})",
        forced.len()
    );
}
