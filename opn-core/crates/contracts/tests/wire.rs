//! Golden wire-shape tests (roadmap Sprint 0 test plan): the documented JSON
//! shapes are literal strings here, not re-derived. Compared as
//! `serde_json::Value` so key order is irrelevant but content is exact.

use contracts::{ClientFrame, Cmd, ErrBody, ErrCode, ServerMsg};
use serde_json::{json, Value};

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
        r#"{"id":4,"cmd":"auth_refresh"}"#,
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
