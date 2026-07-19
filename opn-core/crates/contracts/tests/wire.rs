//! Golden wire-shape tests (roadmap Sprint 0 test plan): the documented JSON
//! shapes are literal strings here, not re-derived. Compared as
//! `serde_json::Value` so key order is irrelevant but content is exact.

use contracts::types::{MediaKind, MessageBody};
use contracts::{cmd::SettingsScope, ClientFrame, Cmd, ErrBody, ErrCode, Evt, ServerMsg};
use serde_json::{json, Value};
use uuid::Uuid;

/// Fixed test uuid `0198c5b6-0000-7000-8000-0000000000<xx>` — goldens embed the
/// full literal; this keeps the Rust side readable.
fn u(xx: &str) -> Uuid {
    Uuid::parse_str(&format!("0198c5b6-0000-7000-8000-0000000000{xx}")).expect("valid uuid")
}

fn roundtrip(frame: &ClientFrame, golden: &str) {
    let ser = serde_json::to_value(frame).expect("serialize");
    let want: Value = serde_json::from_str(golden).expect("golden parses");
    assert_eq!(ser, want, "serialized shape != golden");
    let back: ClientFrame = serde_json::from_str(golden).expect("golden deserializes");
    assert_eq!(
        serde_json::to_value(&back).expect("re-serialize"),
        want,
        "deserialize(golden) round-trip"
    );
}

#[test]
fn client_frame_auth() {
    roundtrip(
        &ClientFrame {
            id: 0,
            cmd: Cmd::Auth {
                token: "jwt.goes.here".into(),
            },
        },
        r#"{"id":0,"cmd":"auth","payload":{"token":"jwt.goes.here"}}"#,
    );
}

#[test]
fn client_frame_sub() {
    roundtrip(
        &ClientFrame {
            id: 1,
            cmd: Cmd::Sub {
                topic: "ch:0198c5b6-0000-7000-8000-000000000001".into(),
                last_seq: Some(41),
            },
        },
        r#"{"id":1,"cmd":"sub","payload":{"topic":"ch:0198c5b6-0000-7000-8000-000000000001","last_seq":41}}"#,
    );
}

#[test]
fn client_frame_sub_no_last_seq() {
    roundtrip(
        &ClientFrame {
            id: 2,
            cmd: Cmd::Sub {
                topic: "notify:dev".into(),
                last_seq: None,
            },
        },
        r#"{"id":2,"cmd":"sub","payload":{"topic":"notify:dev","last_seq":null}}"#,
    );
}

#[test]
fn client_frame_unsub() {
    roundtrip(
        &ClientFrame {
            id: 3,
            cmd: Cmd::Unsub {
                topic: "presence:x".into(),
            },
        },
        r#"{"id":3,"cmd":"unsub","payload":{"topic":"presence:x"}}"#,
    );
}

#[test]
fn client_frame_auth_refresh_unit_variant_has_no_payload() {
    roundtrip(
        &ClientFrame {
            id: 4,
            cmd: Cmd::AuthRefresh,
        },
        r#"{"id":4,"cmd":"auth.refresh"}"#,
    );
}

#[test]
fn client_frame_identity_me_unit_variant_has_no_payload() {
    roundtrip(
        &ClientFrame {
            id: 5,
            cmd: Cmd::IdentityMe,
        },
        r#"{"id":5,"cmd":"identity.me"}"#,
    );
}

#[test]
fn client_frame_identity_app_login() {
    roundtrip(
        &ClientFrame {
            id: 6,
            cmd: Cmd::IdentityAppLogin {
                app_id: "chirp".into(),
                account_id: Uuid::parse_str("0198c5b6-0000-7000-8000-000000000002")
                    .expect("valid uuid"),
            },
        },
        r#"{"id":6,"cmd":"identity.app_login","payload":{"app_id":"chirp","account_id":"0198c5b6-0000-7000-8000-000000000002"}}"#,
    );
}

#[test]
fn client_frame_identity_get_settings() {
    roundtrip(
        &ClientFrame {
            id: 7,
            cmd: Cmd::IdentityGetSettings {
                scope: SettingsScope::Device,
            },
        },
        r#"{"id":7,"cmd":"identity.get_settings","payload":{"scope":"device"}}"#,
    );
}

#[test]
fn client_frame_identity_set_settings() {
    roundtrip(
        &ClientFrame {
            id: 8,
            cmd: Cmd::IdentitySetSettings {
                scope: SettingsScope::Character,
                patch: json!({"theme": "dark", "volume": 3}),
            },
        },
        r#"{"id":8,"cmd":"identity.set_settings","payload":{"scope":"character","patch":{"theme":"dark","volume":3}}}"#,
    );
}

#[test]
fn client_frame_identity_set_share_presence() {
    roundtrip(
        &ClientFrame {
            id: 9,
            cmd: Cmd::IdentitySetSharePresence { on: true },
        },
        r#"{"id":9,"cmd":"identity.set_share_presence","payload":{"on":true}}"#,
    );
}

// ── channels (goldens added 2026-07-18; commands landed Sprints 3–4) ─────────

#[test]
fn client_frame_channels_send() {
    roundtrip(
        &ClientFrame {
            id: 10,
            cmd: Cmd::ChannelsSend {
                channel_id: u("11"),
                client_uuid: u("12"),
                body: MessageBody {
                    text: Some("hi".into()),
                    media_ids: None,
                    gif_url: None,
                    meta: None,
                },
            },
        },
        r#"{"id":10,"cmd":"channels.send","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","client_uuid":"0198c5b6-0000-7000-8000-000000000012","body":{"text":"hi","media_ids":null,"gif_url":null,"meta":null}}}"#,
    );
}

#[test]
fn client_frame_channels_thread_commands() {
    roundtrip(
        &ClientFrame {
            id: 11,
            cmd: Cmd::ChannelsOpenDirect {
                number: "555-1234".into(),
            },
        },
        r#"{"id":11,"cmd":"channels.open_direct","payload":{"number":"555-1234"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 12,
            cmd: Cmd::ChannelsCreate {
                name: Some("crew".into()),
                members: vec![u("13")],
            },
        },
        r#"{"id":12,"cmd":"channels.create","payload":{"name":"crew","members":["0198c5b6-0000-7000-8000-000000000013"]}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 13,
            cmd: Cmd::ChannelsList,
        },
        r#"{"id":13,"cmd":"channels.list"}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 14,
            cmd: Cmd::ChannelsMemberAdd {
                channel_id: u("11"),
                character_id: u("13"),
            },
        },
        r#"{"id":14,"cmd":"channels.member_add","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","character_id":"0198c5b6-0000-7000-8000-000000000013"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 15,
            cmd: Cmd::ChannelsMemberRemove {
                channel_id: u("11"),
                character_id: u("13"),
            },
        },
        r#"{"id":15,"cmd":"channels.member_remove","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","character_id":"0198c5b6-0000-7000-8000-000000000013"}}"#,
    );
}

#[test]
fn client_frame_channels_message_state_commands() {
    roundtrip(
        &ClientFrame {
            id: 16,
            cmd: Cmd::ChannelsMarkDelivered {
                channel_id: u("11"),
                up_to_seq: 41,
            },
        },
        r#"{"id":16,"cmd":"channels.mark_delivered","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","up_to_seq":41}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 17,
            cmd: Cmd::ChannelsMarkRead {
                channel_id: u("11"),
                up_to_seq: 41,
            },
        },
        r#"{"id":17,"cmd":"channels.mark_read","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","up_to_seq":41}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 18,
            cmd: Cmd::ChannelsTyping {
                channel_id: u("11"),
            },
        },
        r#"{"id":18,"cmd":"channels.typing","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 19,
            cmd: Cmd::ChannelsReact {
                channel_id: u("11"),
                message_id: u("14"),
                emoji: "👍".into(),
            },
        },
        r#"{"id":19,"cmd":"channels.react","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","message_id":"0198c5b6-0000-7000-8000-000000000014","emoji":"👍"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 20,
            cmd: Cmd::ChannelsUnreact {
                channel_id: u("11"),
                message_id: u("14"),
                emoji: "👍".into(),
            },
        },
        r#"{"id":20,"cmd":"channels.unreact","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","message_id":"0198c5b6-0000-7000-8000-000000000014","emoji":"👍"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 21,
            cmd: Cmd::ChannelsPin {
                channel_id: u("11"),
                message_id: u("14"),
            },
        },
        r#"{"id":21,"cmd":"channels.pin","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","message_id":"0198c5b6-0000-7000-8000-000000000014"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 22,
            cmd: Cmd::ChannelsUnpin {
                channel_id: u("11"),
                message_id: u("14"),
            },
        },
        r#"{"id":22,"cmd":"channels.unpin","payload":{"channel_id":"0198c5b6-0000-7000-8000-000000000011","message_id":"0198c5b6-0000-7000-8000-000000000014"}}"#,
    );
}

// ── media (Sprint 5 part A) ──────────────────────────────────────────────────

#[test]
fn client_frame_media_commands() {
    roundtrip(
        &ClientFrame {
            id: 23,
            cmd: Cmd::MediaRequestUpload {
                kind: MediaKind::Photo,
                bytes: 1024,
                mime: "image/jpeg".into(),
            },
        },
        r#"{"id":23,"cmd":"media.request_upload","payload":{"kind":"photo","bytes":1024,"mime":"image/jpeg"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 24,
            cmd: Cmd::MediaCommit { media_id: u("15") },
        },
        r#"{"id":24,"cmd":"media.commit","payload":{"media_id":"0198c5b6-0000-7000-8000-000000000015"}}"#,
    );
}

// ── directory (Sprint 5 part B) ──────────────────────────────────────────────

#[test]
fn client_frame_directory_contact_and_block_commands() {
    roundtrip(
        &ClientFrame {
            id: 25,
            cmd: Cmd::DirectoryContactUpsert {
                number: "555-1234".into(),
                display_name: "Bob".into(),
                avatar_media: None,
                meta: None,
            },
        },
        r#"{"id":25,"cmd":"directory.contact_upsert","payload":{"number":"555-1234","display_name":"Bob","avatar_media":null,"meta":null}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 26,
            cmd: Cmd::DirectoryContactDelete {
                number: "555-1234".into(),
            },
        },
        r#"{"id":26,"cmd":"directory.contact_delete","payload":{"number":"555-1234"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 27,
            cmd: Cmd::DirectoryContacts,
        },
        r#"{"id":27,"cmd":"directory.contacts"}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 28,
            cmd: Cmd::DirectoryBlock {
                number: "555-9999".into(),
            },
        },
        r#"{"id":28,"cmd":"directory.block","payload":{"number":"555-9999"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 29,
            cmd: Cmd::DirectoryUnblock {
                number: "555-9999".into(),
            },
        },
        r#"{"id":29,"cmd":"directory.unblock","payload":{"number":"555-9999"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 30,
            cmd: Cmd::DirectoryBlocks,
        },
        r#"{"id":30,"cmd":"directory.blocks"}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 31,
            cmd: Cmd::DirectoryResolve {
                number: "555-1234".into(),
            },
        },
        r#"{"id":31,"cmd":"directory.resolve","payload":{"number":"555-1234"}}"#,
    );
}

#[test]
fn client_frame_directory_listing_commands() {
    roundtrip(
        &ClientFrame {
            id: 32,
            cmd: Cmd::DirectoryListingCreate {
                app_id: "yp".into(),
                kind: "sale".into(),
                title: "Bike".into(),
                body: None,
                contact_number: "555-1234".into(),
                ttl_secs: Some(3600),
            },
        },
        r#"{"id":32,"cmd":"directory.listing_create","payload":{"app_id":"yp","kind":"sale","title":"Bike","body":null,"contact_number":"555-1234","ttl_secs":3600}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 33,
            cmd: Cmd::DirectoryListingDelete { id: u("16") },
        },
        r#"{"id":33,"cmd":"directory.listing_delete","payload":{"id":"0198c5b6-0000-7000-8000-000000000016"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 34,
            cmd: Cmd::DirectoryListings {
                app_id: "yp".into(),
                cursor: None,
                limit: Some(25),
            },
        },
        r#"{"id":34,"cmd":"directory.listings","payload":{"app_id":"yp","cursor":null,"limit":25}}"#,
    );
}

// ── calls (Sprint 6 part A) ──────────────────────────────────────────────────

#[test]
fn client_frame_calls_commands() {
    roundtrip(
        &ClientFrame {
            id: 40,
            cmd: Cmd::CallsStart {
                callee_number: "555-1234".into(),
                video: true,
            },
        },
        r#"{"id":40,"cmd":"calls.start","payload":{"callee_number":"555-1234","video":true}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 41,
            cmd: Cmd::CallsAccept { call_id: u("40") },
        },
        r#"{"id":41,"cmd":"calls.accept","payload":{"call_id":"0198c5b6-0000-7000-8000-000000000040"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 42,
            cmd: Cmd::CallsDecline { call_id: u("40") },
        },
        r#"{"id":42,"cmd":"calls.decline","payload":{"call_id":"0198c5b6-0000-7000-8000-000000000040"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 43,
            cmd: Cmd::CallsHangup { call_id: u("40") },
        },
        r#"{"id":43,"cmd":"calls.hangup","payload":{"call_id":"0198c5b6-0000-7000-8000-000000000040"}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 44,
            cmd: Cmd::CallsSignal {
                call_id: u("40"),
                to: u("42"),
                payload: json!({"sdp": "v=0"}),
            },
        },
        r#"{"id":44,"cmd":"calls.signal","payload":{"call_id":"0198c5b6-0000-7000-8000-000000000040","to":"0198c5b6-0000-7000-8000-000000000042","payload":{"sdp":"v=0"}}}"#,
    );
}

#[test]
fn push_calls_state() {
    use contracts::types::{CallKind, CallParticipant, CallParticipantState, CallSessionState};
    let push = ServerMsg::Push {
        topic: "call:0198c5b6-0000-7000-8000-000000000040".into(),
        evt: Evt::CallsState {
            call_id: u("40"),
            kind: CallKind::Video,
            state: CallSessionState::Ringing,
            participants: vec![
                CallParticipant {
                    character_id: u("42"),
                    state: CallParticipantState::Joined,
                },
                CallParticipant {
                    character_id: u("43"),
                    state: CallParticipantState::Ringing,
                },
            ],
            ice_servers: json!([]),
        },
    };
    assert_eq!(
        serde_json::to_value(&push).expect("serialize"),
        json!({
            "topic": "call:0198c5b6-0000-7000-8000-000000000040",
            "evt": "calls.state",
            "payload": {
                "call_id": "0198c5b6-0000-7000-8000-000000000040",
                "kind": "video",
                "state": "ringing",
                "participants": [
                    {"character_id": "0198c5b6-0000-7000-8000-000000000042", "state": "joined"},
                    {"character_id": "0198c5b6-0000-7000-8000-000000000043", "state": "ringing"}
                ],
                "ice_servers": []
            }
        })
    );
}

#[test]
fn push_calls_signal() {
    let push = ServerMsg::Push {
        topic: "call:0198c5b6-0000-7000-8000-000000000040".into(),
        evt: Evt::CallsSignal {
            call_id: u("40"),
            from: u("42"),
            to: u("43"),
            payload: json!({"candidate": "a=x"}),
        },
    };
    assert_eq!(
        serde_json::to_value(&push).expect("serialize"),
        json!({
            "topic": "call:0198c5b6-0000-7000-8000-000000000040",
            "evt": "calls.signal",
            "payload": {
                "call_id": "0198c5b6-0000-7000-8000-000000000040",
                "from": "0198c5b6-0000-7000-8000-000000000042",
                "to": "0198c5b6-0000-7000-8000-000000000043",
                "payload": {"candidate": "a=x"}
            }
        })
    );
}

// ── tenant link (Sprint 6 part B, §5) ────────────────────────────────────────

#[test]
fn push_calls_voice() {
    use contracts::types::VoiceAction;
    let push = ServerMsg::Push {
        topic: "link".into(),
        evt: Evt::CallsVoice {
            call_id: u("40"),
            action: VoiceAction::SetTargets,
            characters: vec![u("42"), u("43")],
        },
    };
    assert_eq!(
        serde_json::to_value(&push).expect("serialize"),
        json!({
            "topic": "link",
            "evt": "calls.voice",
            "payload": {
                "call_id": "0198c5b6-0000-7000-8000-000000000040",
                "action": "set_targets",
                "characters": [
                    "0198c5b6-0000-7000-8000-000000000042",
                    "0198c5b6-0000-7000-8000-000000000043"
                ]
            }
        })
    );
}

#[test]
fn link_hello_shape() {
    use contracts::types::LinkHello;
    let hello = LinkHello {
        resource_version: "1.2.3".into(),
        contracts_version: "0.1.0".into(),
    };
    let golden = json!({ "resource_version": "1.2.3", "contracts_version": "0.1.0" });
    assert_eq!(serde_json::to_value(&hello).expect("serialize"), golden);
    let back: LinkHello = serde_json::from_value(golden).expect("deserialize");
    assert_eq!(back.resource_version, "1.2.3");
    assert_eq!(back.contracts_version, "0.1.0");
}

// ── notify (Sprint 3) ────────────────────────────────────────────────────────

#[test]
fn client_frame_notify_commands() {
    roundtrip(
        &ClientFrame {
            id: 35,
            cmd: Cmd::NotifySeen { ids: vec![u("17")] },
        },
        r#"{"id":35,"cmd":"notify.seen","payload":{"ids":["0198c5b6-0000-7000-8000-000000000017"]}}"#,
    );
    roundtrip(
        &ClientFrame {
            id: 36,
            cmd: Cmd::NotifyClear,
        },
        r#"{"id":36,"cmd":"notify.clear"}"#,
    );
}

#[test]
fn push_presence_state() {
    let push = ServerMsg::Push {
        topic: "presence:0198c5b6-0000-7000-8000-000000000003".into(),
        evt: Evt::PresenceState {
            character_id: Uuid::parse_str("0198c5b6-0000-7000-8000-000000000003")
                .expect("valid uuid"),
            online: Some(false),
            last_seen_at: Some("2026-07-18T12:00:00Z".into()),
        },
    };
    assert_eq!(
        serde_json::to_value(&push).expect("serialize"),
        json!({
            "topic": "presence:0198c5b6-0000-7000-8000-000000000003",
            "evt": "presence.state",
            "payload": {
                "character_id": "0198c5b6-0000-7000-8000-000000000003",
                "online": false,
                "last_seen_at": "2026-07-18T12:00:00Z"
            }
        })
    );
}

#[test]
fn err_codes_are_snake_case() {
    for (code, wire) in [
        (ErrCode::Unauthorized, "unauthorized"),
        (ErrCode::Forbidden, "forbidden"),
        (ErrCode::NotFound, "not_found"),
        (ErrCode::Invalid, "invalid"),
        (ErrCode::Conflict, "conflict"),
        (ErrCode::RateLimited, "rate_limited"),
        (ErrCode::TooLarge, "too_large"),
        (ErrCode::Internal, "internal"),
    ] {
        assert_eq!(serde_json::to_value(code).expect("serialize"), json!(wire));
    }
}

#[test]
fn ack_shapes() {
    let ok = ServerMsg::Ack {
        reply_to: 7,
        ok: true,
        payload: Some(json!({"x": 1})),
        err: None,
    };
    assert_eq!(
        serde_json::to_value(&ok).expect("serialize"),
        json!({"reply_to": 7, "ok": true, "payload": {"x": 1}})
    );

    let err = ServerMsg::Ack {
        reply_to: 8,
        ok: false,
        payload: None,
        err: Some(ErrBody {
            code: ErrCode::RateLimited,
            msg: "slow down".into(),
        }),
    };
    assert_eq!(
        serde_json::to_value(&err).expect("serialize"),
        json!({"reply_to": 8, "ok": false, "err": {"code": "rate_limited", "msg": "slow down"}})
    );
}
