use uuid::Uuid;

/// The only way any code mints an id (OPN-CORE.md §9): UUIDv7, time-ordered,
/// index-friendly, safe to expose.
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}
