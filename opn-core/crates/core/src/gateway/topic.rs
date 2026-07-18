//! Topic strings parsed into a typed shape before any authorization runs
//! (§4.4): unknown shape is `invalid`, authorization is the owning
//! primitive's job.

use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TopicKind {
    Ch(Uuid),
    Feed(String),
    Call(Uuid),
    Notify(Uuid),
    Presence(Uuid),
}

impl TopicKind {
    pub fn parse(topic: &str) -> Option<TopicKind> {
        let (kind, rest) = topic.split_once(':')?;
        match kind {
            "ch" => Some(TopicKind::Ch(rest.parse().ok()?)),
            "call" => Some(TopicKind::Call(rest.parse().ok()?)),
            "notify" => Some(TopicKind::Notify(rest.parse().ok()?)),
            "presence" => Some(TopicKind::Presence(rest.parse().ok()?)),
            // App ids are free-form slugs; cap keeps topic keys bounded.
            "feed" if !rest.is_empty() && rest.len() <= 64 => {
                Some(TopicKind::Feed(rest.to_string()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_shapes() {
        let id = Uuid::nil();
        assert_eq!(
            TopicKind::parse(&format!("ch:{id}")),
            Some(TopicKind::Ch(id))
        );
        assert_eq!(
            TopicKind::parse(&format!("presence:{id}")),
            Some(TopicKind::Presence(id))
        );
        assert_eq!(
            TopicKind::parse("feed:instapic"),
            Some(TopicKind::Feed("instapic".into()))
        );
    }

    #[test]
    fn rejects_garbage() {
        for t in ["", "ch:", "ch:not-a-uuid", "nope:123", "feed:", "presence"] {
            assert_eq!(TopicKind::parse(t), None, "{t}");
        }
    }
}
