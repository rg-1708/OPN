//! Per-character token-bucket rate limiting (OPN-CORE.md §12). In-process
//! only per CDR-4: sticky WS routing means one replica sees every command
//! from a given character, so there is no cross-replica coordination and no
//! Redis round-trip on the hot path — just a `DashMap` of lazily-created
//! buckets. The janitor evicts idle buckets so the table cannot grow without
//! bound (roadmap Sprint 2 item 6).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use uuid::Uuid;

/// Rate class per OPN-CORE.md §12. Every command maps to exactly one via
/// [`class_of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    Msg,
    Social,
    Money,
    Expensive,
    Read,
}

impl Class {
    /// `(sustained tokens/sec, burst capacity)` for this class.
    fn budget(self) -> (f64, f64) {
        match self {
            Class::Msg => (1.0, 5.0),
            Class::Social => (5.0, 20.0),
            Class::Money => (1.0, 2.0),
            Class::Expensive => (0.2, 3.0),
            Class::Read => (10.0, 30.0),
        }
    }
}

/// Exhaustive mapping from command to bucket. Intentionally has no `_`
/// catch-all: adding a `Cmd` variant must fail to compile until it is
/// assigned a rate class (roadmap Sprint 2 item 6).
pub fn class_of(cmd: &contracts::Cmd) -> Class {
    use contracts::Cmd;
    match cmd {
        Cmd::Auth { .. }
        | Cmd::Sub { .. }
        | Cmd::Unsub { .. }
        | Cmd::AuthRefresh
        | Cmd::IdentityMe
        | Cmd::IdentityGetSettings { .. }
        | Cmd::ChannelsList
        // Receipts are high-frequency, cheap watermark bumps — read-rate fits.
        | Cmd::ChannelsMarkDelivered { .. }
        | Cmd::ChannelsMarkRead { .. }
        // Directory reads: cheap indexed lookups.
        | Cmd::DirectoryContacts
        | Cmd::DirectoryBlocks
        | Cmd::DirectoryResolve { .. }
        | Cmd::DirectoryListings { .. }
        | Cmd::NotifySeen { .. } => Class::Read,
        Cmd::IdentityAppLogin { .. }
        | Cmd::IdentitySetSettings { .. }
        | Cmd::IdentitySetSharePresence { .. }
        | Cmd::ChannelsOpenDirect { .. }
        | Cmd::ChannelsCreate { .. }
        // Typing self-limits to ~1/3 s; reactions/pins/members are occasional.
        | Cmd::ChannelsTyping { .. }
        | Cmd::ChannelsReact { .. }
        | Cmd::ChannelsUnreact { .. }
        | Cmd::ChannelsPin { .. }
        | Cmd::ChannelsUnpin { .. }
        | Cmd::ChannelsMemberAdd { .. }
        | Cmd::ChannelsMemberRemove { .. }
        // A commit is a cheap owner-scoped UPDATE; social-rate is plenty.
        | Cmd::MediaCommit { .. }
        // Directory writes: occasional contact/block/listing edits.
        | Cmd::DirectoryContactUpsert { .. }
        | Cmd::DirectoryContactDelete { .. }
        | Cmd::DirectoryBlock { .. }
        | Cmd::DirectoryUnblock { .. }
        | Cmd::DirectoryListingCreate { .. }
        | Cmd::DirectoryListingDelete { .. }
        // Calls: start/accept/decline/hangup are occasional. signal carries
        // WebRTC offer/answer/ICE — a setup trickle fits Social's burst-20;
        // revisit if a real call storms it (Sprint 10 budget-tuning).
        | Cmd::CallsStart { .. }
        | Cmd::CallsAccept { .. }
        | Cmd::CallsDecline { .. }
        | Cmd::CallsHangup { .. }
        | Cmd::CallsSignal { .. }
        // Feed writes: posts/likes/comments/follows are occasional user actions;
        // reads are HTTP (part B), not rate-classed here.
        | Cmd::FeedPost { .. }
        | Cmd::FeedDelete { .. }
        | Cmd::FeedLike { .. }
        | Cmd::FeedUnlike { .. }
        | Cmd::FeedComment { .. }
        | Cmd::FeedFollow { .. }
        | Cmd::FeedUnfollow { .. }
        | Cmd::NotifyClear => Class::Social,
        // Money movement (§12): the tight Money bucket (1/s, burst 2) — the class
        // it was added for. Transfers, holds, captures, releases all move or
        // reserve funds; a low ceiling is correct.
        Cmd::LedgerTransfer { .. }
        | Cmd::LedgerHold { .. }
        | Cmd::LedgerCapture { .. }
        | Cmd::LedgerRelease { .. }
        | Cmd::LedgerWithdraw { .. } => Class::Money,
        // Issuing a presigned upload signs policies and reserves a row — the
        // costliest command that isn't the hot path. Tight budget (§12).
        Cmd::MediaRequestUpload { .. } => Class::Expensive,
        // The hot path — its own class with a message-rate budget (§12).
        Cmd::ChannelsSend { .. } => Class::Msg,
    }
}

/// Idle buckets older than this are dropped by [`RateLimitTable::sweep_idle`].
const IDLE_TTL: Duration = Duration::from_secs(600);

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// In-process rate-limit state, keyed by `(character, class)`. Cheap to clone
/// the handle via `Arc`; buckets are inserted on first use.
#[derive(Default)]
pub struct RateLimitTable {
    buckets: DashMap<(Uuid, Class), Mutex<Bucket>>,
}

impl RateLimitTable {
    /// Take one token. `Ok(())` means the command may proceed; `Err(ms)` is
    /// the caller-facing `retry_after_ms` before a token will be available.
    /// The per-bucket `Mutex` is held only for the arithmetic below — never
    /// across an await (there are none here).
    pub fn check(&self, character: Uuid, class: Class) -> Result<(), u64> {
        let (rate, burst) = class.budget();
        let entry = self.buckets.entry((character, class)).or_insert_with(|| {
            Mutex::new(Bucket {
                tokens: burst,
                last: Instant::now(),
            })
        });
        let Ok(mut bucket) = entry.lock() else {
            // A poisoned bucket means a prior holder panicked mid-arithmetic;
            // fail closed rather than trust corrupt state.
            return Err(1000);
        };

        let now = Instant::now();
        let refill = now.duration_since(bucket.last).as_secs_f64() * rate;
        bucket.tokens = burst.min(bucket.tokens + refill);
        bucket.last = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(((1.0 - bucket.tokens) / rate * 1000.0) as u64)
        }
    }

    /// Janitor sweep: drop buckets untouched for longer than [`IDLE_TTL`].
    /// Returns the number evicted.
    pub fn sweep_idle(&self) -> u64 {
        let before = self.buckets.len();
        self.buckets.retain(|_, b| {
            // `retain` hands us `&mut Mutex`, so no lock is needed. Keep
            // poisoned entries: dropping them silently would hide a panic.
            b.get_mut()
                .map(|bucket| bucket.last.elapsed() < IDLE_TTL)
                .unwrap_or(true)
        });
        (before - self.buckets.len()) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_then_sustained() {
        let table = RateLimitTable::default();
        let who = Uuid::now_v7();
        let (rate, burst) = Class::Read.budget();

        // A full burst of takes succeeds immediately.
        for _ in 0..burst as u32 {
            assert!(table.check(who, Class::Read).is_ok());
        }
        // The next is limited with a sane retry hint.
        let retry = table.check(who, Class::Read).expect_err("limited");
        assert!(retry > 0);
        assert!(retry <= (1000.0 / rate).ceil() as u64);
    }

    #[test]
    fn refills_after_wait() {
        let table = RateLimitTable::default();
        let who = Uuid::now_v7();
        let (rate, burst) = Class::Read.budget();

        for _ in 0..burst as u32 {
            assert!(table.check(who, Class::Read).is_ok());
        }
        assert!(table.check(who, Class::Read).is_err());

        // Sleep ~2 token periods; Read at 10/s means ~200 ms.
        std::thread::sleep(Duration::from_secs_f64(2.0 / rate));
        assert!(table.check(who, Class::Read).is_ok());
    }

    #[test]
    fn classes_are_independent() {
        let table = RateLimitTable::default();
        let who = Uuid::now_v7();

        // Drain Social entirely.
        let (_, social_burst) = Class::Social.budget();
        for _ in 0..social_burst as u32 {
            assert!(table.check(who, Class::Social).is_ok());
        }
        assert!(table.check(who, Class::Social).is_err());

        // Read for the same character is untouched.
        assert!(table.check(who, Class::Read).is_ok());
    }

    #[test]
    fn sweep_keeps_fresh_bucket() {
        let table = RateLimitTable::default();
        let who = Uuid::now_v7();
        assert!(table.check(who, Class::Read).is_ok());
        assert_eq!(table.sweep_idle(), 0);
    }
}
