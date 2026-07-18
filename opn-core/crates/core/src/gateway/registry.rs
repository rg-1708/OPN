//! In-process connection registry (OPN-CORE.md §4.2): session → live
//! connection, topic → subscribers, presence counts. Single replica owns all
//! of a world's connections (sticky WS); replica 2+ fan-out rides Redis
//! pub/sub (`gateway::fanout`), which calls back into `publish_local` here.
//!
//! Takeover subtlety: `topics` entries are `(session_id, conn_seq)` pairs,
//! not bare session ids. A last-writer-wins takeover (§4.1) replaces the
//! handle under the same session id; if topic entries were bare ids, the old
//! connection's cleanup could strip subscriptions the *new* connection just
//! made. The per-connection `conn_seq` disambiguates; stale pairs are pruned
//! lazily on publish.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use contracts::{Evt, EvtClass, ServerMsg};
use dashmap::DashMap;
use metrics::{counter, gauge};
use smallvec::SmallVec;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::infra::auth::Identity;

/// WS close codes owned by the gateway (§4.1, §4.3).
pub mod close {
    /// Malformed first frame (anything but `auth`).
    pub const BAD_FIRST_FRAME: u16 = 4400;
    /// No/invalid auth in time, or origin re-check failed.
    pub const UNAUTHORIZED: u16 = 4401;
    /// Session taken over by a newer connection (last-writer-wins).
    pub const TAKEN_OVER: u16 = 4408;
    /// Slow consumer: send queue full on a durable event.
    pub const SLOW_CONSUMER: u16 = 4409;
}

static CONN_SEQ: AtomicU64 = AtomicU64::new(0);

/// One live authenticated connection. Created by the WS handler, owned by
/// the registry (and the reader/writer tasks via `Arc`).
pub struct ConnHandle {
    pub identity: Identity,
    /// Distinguishes this connection from a takeover successor under the
    /// same session id.
    pub conn_seq: u64,
    /// Serialized `ServerMsg` frames to the writer task.
    tx: mpsc::Sender<Arc<str>>,
    sendq_capacity: usize,
    /// Set once to a close code; reader and writer both watch it.
    closed: watch::Sender<Option<u16>>,
    /// Topics this connection is subscribed to — so disconnect is O(own
    /// subs), not a full topic scan (§4.2).
    subs: Mutex<HashSet<Arc<str>>>,
    /// Cached `characters.share_presence`, read at auth, updated live by the
    /// `identity.set_share_presence` handler on this same session.
    pub share_presence: AtomicBool,
}

impl ConnHandle {
    pub fn new(
        identity: Identity,
        share_presence: bool,
        sendq_capacity: usize,
    ) -> (
        Arc<ConnHandle>,
        mpsc::Receiver<Arc<str>>,
        watch::Receiver<Option<u16>>,
    ) {
        let (tx, rx) = mpsc::channel(sendq_capacity);
        let (closed, closed_rx) = watch::channel(None);
        let handle = Arc::new(ConnHandle {
            identity,
            conn_seq: CONN_SEQ.fetch_add(1, Ordering::Relaxed),
            tx,
            sendq_capacity,
            closed,
            subs: Mutex::new(HashSet::new()),
            share_presence: AtomicBool::new(share_presence),
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

    /// Ack delivery: acks are the reply the client is actively waiting on —
    /// a queue too full to take one is a slow consumer, same as a durable
    /// event (§4.3).
    pub fn send_ack(&self, frame: Arc<str>) {
        if self.tx.try_send(frame).is_err() {
            counter!("opn_sendq_drops_total", "class" => "durable_close").increment(1);
            self.close(close::SLOW_CONSUMER);
        }
    }

    /// Durable delivery that *waits* for queue capacity instead of closing on
    /// a transiently full queue. Resume replay (§4.4) enqueues up to
    /// `RESUME_MAX` durable frames from the dispatch task faster than the
    /// writer drains — the `send_evt` slow-consumer close would then kill a
    /// perfectly healthy client mid-catch-up (the burst is the *server's*, not
    /// a slow reader's). Awaiting the permit backpressures the replay to the
    /// client's drain rate; a genuinely dead client drops the receiver, so
    /// `reserve` errors and we return `false` for the caller to stop.
    async fn push_awaiting(&self, frame: Arc<str>) -> bool {
        match self.tx.reserve().await {
            Ok(permit) => {
                permit.send(frame);
                true
            }
            Err(_) => false,
        }
    }

    /// Class-aware event delivery (§4.3): durable + full queue closes the
    /// connection; ephemeral drops silently below ~20 % headroom.
    fn send_evt(&self, frame: Arc<str>, class: EvtClass) {
        match class {
            EvtClass::Durable => {
                if self.tx.try_send(frame).is_err() {
                    counter!("opn_sendq_drops_total", "class" => "durable_close").increment(1);
                    self.close(close::SLOW_CONSUMER);
                }
            }
            EvtClass::Ephemeral => {
                if self.tx.capacity() < self.sendq_capacity / 5 + 1
                    || self.tx.try_send(frame).is_err()
                {
                    counter!("opn_sendq_drops_total", "class" => "ephemeral").increment(1);
                }
            }
        }
    }
}

type TopicEntry = SmallVec<[(Uuid, u64); 4]>;

#[derive(Default)]
pub struct SessionRegistry {
    sessions: DashMap<Uuid, Arc<ConnHandle>>,
    topics: DashMap<(Uuid, Arc<str>), TopicEntry>,
    /// (world, character) → live connection count; presence is "any session
    /// online", and a character can hold several device sessions.
    presence: DashMap<(Uuid, Uuid), u32>,
}

impl SessionRegistry {
    /// Registers under last-writer-wins (§4.1): returns the previous handle,
    /// which the caller must `close(TAKEN_OVER)`. Also counts the presence
    /// connection; returns `true` in `.1` if the character just came online
    /// (0 → 1 connections).
    pub fn register(&self, handle: Arc<ConnHandle>) -> (Option<Arc<ConnHandle>>, bool) {
        let id = &handle.identity;
        let prev = self.sessions.insert(id.session_id, handle.clone());
        let mut count = self
            .presence
            .entry((id.world_id, id.character_id))
            .or_insert(0);
        *count += 1;
        let came_online = *count == 1;
        drop(count);
        gauge!("opn_connections").set(self.sessions.len() as f64);
        (prev, came_online)
    }

    /// Cleanup for one *connection* (not blindly the session: after a
    /// takeover the session id maps to the successor, which must survive).
    /// Returns `true` if the character just went offline (1 → 0).
    pub fn unregister(&self, handle: &Arc<ConnHandle>) -> bool {
        self.sessions
            .remove_if(&handle.identity.session_id, |_, h| {
                h.conn_seq == handle.conn_seq
            });
        let world = handle.identity.world_id;
        if let Ok(subs) = handle.subs.lock() {
            for topic in subs.iter() {
                self.remove_topic_entry(world, topic, handle);
            }
        }
        let key = (world, handle.identity.character_id);
        let went_offline = match self.presence.get_mut(&key) {
            Some(mut count) => {
                *count = count.saturating_sub(1);
                *count == 0
            }
            None => false,
        };
        if went_offline {
            self.presence.remove_if(&key, |_, c| *c == 0);
        }
        gauge!("opn_connections").set(self.sessions.len() as f64);
        went_offline
    }

    pub fn subscribe(&self, topic: &str, handle: &Arc<ConnHandle>) {
        let topic: Arc<str> = Arc::from(topic);
        if let Ok(mut subs) = handle.subs.lock() {
            if !subs.insert(topic.clone()) {
                return; // already subscribed — keep the entry list dup-free
            }
        }
        self.topics
            .entry((handle.identity.world_id, topic))
            .or_default()
            .push((handle.identity.session_id, handle.conn_seq));
    }

    pub fn unsubscribe(&self, topic: &str, handle: &Arc<ConnHandle>) {
        if let Ok(mut subs) = handle.subs.lock() {
            if !subs.remove(topic) {
                return;
            }
        }
        self.remove_topic_entry(handle.identity.world_id, topic, handle);
    }

    fn remove_topic_entry(&self, world: Uuid, topic: &str, handle: &Arc<ConnHandle>) {
        let key = (world, Arc::from(topic));
        if let Some(mut entry) = self.topics.get_mut(&key) {
            entry.retain(|&mut (sid, seq)| {
                !(sid == handle.identity.session_id && seq == handle.conn_seq)
            });
            let empty = entry.is_empty();
            drop(entry);
            if empty {
                self.topics.remove_if(&key, |_, v| v.is_empty());
            }
        }
    }

    /// Fan-out to local subscribers of a topic; the event is serialized
    /// exactly once. Cross-replica delivery is `gateway::publish`'s job.
    pub fn publish_local(&self, world: Uuid, topic: &str, evt: &Evt) {
        let key = (world, Arc::from(topic));
        let Some(entry) = self.topics.get(&key) else {
            return;
        };
        let class = evt.class();
        let frame = serialize_push(topic, evt);
        let mut stale = false;
        for &(sid, seq) in entry.iter() {
            match self.sessions.get(&sid) {
                Some(h) if h.conn_seq == seq => h.send_evt(frame.clone(), class),
                _ => stale = true,
            }
        }
        drop(entry);
        if stale {
            if let Some(mut entry) = self.topics.get_mut(&key) {
                entry.retain(|&mut (sid, seq)| {
                    matches!(self.sessions.get(&sid), Some(h) if h.conn_seq == seq)
                });
            }
        }
    }

    /// Direct push to one connection, bypassing topics — snapshot-on-sub
    /// (§4.2: snapshot delivered before the sub ack) and notify routing.
    pub fn push_to(&self, handle: &Arc<ConnHandle>, topic: &str, evt: &Evt) {
        handle.send_evt(serialize_push(topic, evt), evt.class());
    }

    /// Resume-replay push (§4.4): like `push_to` but backpressures on a full
    /// queue instead of closing (see `ConnHandle::push_awaiting`). Returns
    /// `false` if the connection went away mid-replay, so the caller stops.
    pub async fn push_to_awaiting(&self, handle: &Arc<ConnHandle>, topic: &str, evt: &Evt) -> bool {
        handle.push_awaiting(serialize_push(topic, evt)).await
    }

    pub fn get(&self, session_id: Uuid) -> Option<Arc<ConnHandle>> {
        self.sessions.get(&session_id).map(|h| h.clone())
    }

    /// Drop a character's subscription to one topic across all their live
    /// sessions — used when a member is removed from a group so their socket
    /// stops receiving at once (§10.2), without waiting for them to unsub.
    /// O(live sessions of that character); collect handles first so we do not
    /// hold a `DashMap` iterator across the `unsubscribe` mutations.
    pub fn drop_character_topic(&self, world: Uuid, character: Uuid, topic: &str) {
        let handles: Vec<Arc<ConnHandle>> = self
            .sessions
            .iter()
            .filter(|h| h.identity.world_id == world && h.identity.character_id == character)
            .map(|h| h.clone())
            .collect();
        for h in handles {
            self.unsubscribe(topic, &h);
        }
    }

    /// Presence refresh support: one pass over live connections
    /// (world, character, share_presence) — deduplication is the caller's
    /// concern (it pipelines SETs; duplicate keys are harmless).
    pub fn live_characters(&self) -> Vec<(Uuid, Uuid, bool)> {
        self.sessions
            .iter()
            .map(|h| {
                (
                    h.identity.world_id,
                    h.identity.character_id,
                    h.share_presence.load(Ordering::Relaxed),
                )
            })
            .collect()
    }

    pub fn is_character_online(&self, world: Uuid, character: Uuid) -> bool {
        self.presence
            .get(&(world, character))
            .is_some_and(|c| *c > 0)
    }

    /// Device ids of a character's live sessions, for `notify::route` to push
    /// `notify:<device>` events at (§10.8). Deduped across a character's
    /// multiple device sessions.
    // ponytail: O(live sessions) scan. The send path never reaches here (it
    // only routes to *offline* members, which short-circuit to the inbox), and
    // Sprint 3 has no online-notify caller but the direct route test. Add a
    // (world,character)→sessions index if notify-to-online becomes hot (Sprint
    // 6 rings online callees).
    pub fn online_notify_targets(&self, world: Uuid, character: Uuid) -> SmallVec<[Uuid; 4]> {
        let mut out: SmallVec<[Uuid; 4]> = SmallVec::new();
        for h in self.sessions.iter() {
            let id = &h.identity;
            if id.world_id == world && id.character_id == character && !out.contains(&id.device_id)
            {
                out.push(id.device_id);
            }
        }
        out
    }
}

fn serialize_push(topic: &str, evt: &Evt) -> Arc<str> {
    let msg = ServerMsg::Push {
        topic: topic.to_string(),
        evt: evt.clone(),
    };
    // Contracts types serialize infallibly (no maps with non-string keys).
    match serde_json::to_string(&msg) {
        Ok(s) => Arc::from(s),
        Err(e) => {
            tracing::error!(error = %e, "push serialization failed");
            Arc::from("{}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::ids::new_id;

    fn handle(cap: usize) -> (Arc<ConnHandle>, mpsc::Receiver<Arc<str>>) {
        let identity = Identity::for_new_session(new_id(), new_id(), new_id(), new_id(), new_id());
        let (h, rx, _closed) = ConnHandle::new(identity, true, cap);
        (h, rx)
    }

    fn handle_for(
        world: Uuid,
        character: Uuid,
        session: Uuid,
    ) -> (Arc<ConnHandle>, mpsc::Receiver<Arc<str>>) {
        let identity = Identity::for_new_session(session, new_id(), world, character, new_id());
        let (h, rx, _closed) = ConnHandle::new(identity, true, 8);
        (h, rx)
    }

    fn evt() -> Evt {
        Evt::PresenceState {
            character_id: Uuid::nil(),
            online: Some(true),
            last_seen_at: None,
        }
    }

    #[tokio::test]
    async fn durable_full_closes_slow_consumer() {
        let (h, _rx) = handle(2);
        h.send_ack(Arc::from("a"));
        h.send_ack(Arc::from("b"));
        assert!(!h.is_closed());
        h.send_ack(Arc::from("c")); // queue full → 4409
        assert!(h.is_closed());
    }

    #[tokio::test]
    async fn ephemeral_drops_instead_of_closing() {
        let (h, mut rx) = handle(2);
        h.send_ack(Arc::from("a"));
        h.send_ack(Arc::from("b"));
        h.send_evt(Arc::from("e"), EvtClass::Ephemeral); // full → silent drop
        assert!(!h.is_closed());
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_some());
        assert!(rx.try_recv().is_err(), "ephemeral frame must be dropped");
    }

    /// The takeover subtlety this module exists for: the old connection's
    /// cleanup must not strip the successor's registration or subs.
    #[tokio::test]
    async fn takeover_cleanup_preserves_successor() {
        let reg = SessionRegistry::default();
        let world = new_id();
        let character = new_id();
        let session = new_id();
        let (old, _rx_old) = handle_for(world, character, session);
        let (new, mut rx_new) = handle_for(world, character, session);

        let (prev, came_online) = reg.register(old.clone());
        assert!(prev.is_none());
        assert!(came_online);
        let (prev, came_online) = reg.register(new.clone());
        assert!(prev.is_some(), "takeover returns the old handle");
        assert!(!came_online, "character never went offline");

        // New conn subscribes before old cleanup runs — the race the
        // (session, conn_seq) pairs close.
        let topic = format!("presence:{character}");
        reg.subscribe(&topic, &new);
        let went_offline = reg.unregister(&old);
        assert!(!went_offline, "successor still counts");

        reg.publish_local(world, &topic, &evt());
        assert!(
            rx_new.try_recv().is_ok(),
            "successor's subscription must survive old-conn cleanup"
        );

        assert!(reg.unregister(&new), "last conn going away = offline");
    }

    #[tokio::test]
    async fn unsubscribe_stops_delivery() {
        let reg = SessionRegistry::default();
        let world = new_id();
        let (h, mut rx) = handle_for(world, new_id(), new_id());
        reg.register(h.clone());
        reg.subscribe("presence:x", &h);
        reg.publish_local(world, "presence:x", &evt());
        assert!(rx.try_recv().is_ok());
        reg.unsubscribe("presence:x", &h);
        reg.publish_local(world, "presence:x", &evt());
        assert!(rx.try_recv().is_err());
    }
}
