//! Sprint 9 proptest layer for the two pure FSMs (calls `§10.4`, hold `§10.5`).
//! Both `apply`s take no I/O, so we run full-shrinking `proptest!` over the whole
//! input space: totality (never panics), terminal absorption, and legality
//! matched against a predicate encoded here — NOT by calling `apply` (that would
//! just mirror the implementation and prove nothing).

use contracts::{CallParticipantState as P, CallSessionState as S};
use opn_core::primitives::calls::fsm::{apply as calls_apply, Action as CallAction};
use opn_core::primitives::ledger::fsm::{apply as hold_apply, Action as HoldAction, HoldState};
use proptest::prelude::*;

fn session() -> impl Strategy<Value = S> {
    prop_oneof![Just(S::Ringing), Just(S::Active), Just(S::Ended)]
}

fn participant() -> impl Strategy<Value = P> {
    prop_oneof![
        Just(P::Ringing),
        Just(P::Joined),
        Just(P::Declined),
        Just(P::Left),
    ]
}

fn others() -> impl Strategy<Value = Vec<P>> {
    proptest::collection::vec(participant(), 0..6)
}

fn call_action() -> impl Strategy<Value = CallAction> {
    prop_oneof![
        Just(CallAction::Accept),
        Just(CallAction::Decline),
        Just(CallAction::Hangup),
    ]
}

/// The legal-precondition predicate, encoded once (the doc contract in
/// `calls/fsm.rs`): legality depends only on `(session, actor, action)`.
fn calls_legal(session: S, actor: P, action: CallAction) -> bool {
    if session == S::Ended {
        return false;
    }
    match action {
        CallAction::Accept | CallAction::Decline => actor == P::Ringing,
        CallAction::Hangup => actor == P::Joined,
    }
}

proptest! {
    /// 1. Totality: any input yields a `Result`, never a panic.
    #[test]
    fn calls_never_panics(session in session(), actor in participant(), others in others(), action in call_action()) {
        let _ = calls_apply(session, actor, &others, action);
    }

    /// 2. Terminal absorption: an ended session rejects everything.
    #[test]
    fn calls_ended_absorbs(actor in participant(), others in others(), action in call_action()) {
        prop_assert!(calls_apply(S::Ended, actor, &others, action).is_err());
    }

    /// 3. Legality matches the predicate exactly.
    #[test]
    fn calls_legality_matches(session in session(), actor in participant(), others in others(), action in call_action()) {
        prop_assert_eq!(
            calls_apply(session, actor, &others, action).is_ok(),
            calls_legal(session, actor, action)
        );
    }

    /// 4. A legal transition never lands in an illegal state: the session is one
    ///    of the three, and the actor's new state matches the action's mapping.
    #[test]
    fn calls_result_states_legal(session in session(), actor in participant(), others in others(), action in call_action()) {
        if let Ok(t) = calls_apply(session, actor, &others, action) {
            prop_assert!(matches!(t.session, S::Ringing | S::Active | S::Ended));
            let want = match action {
                CallAction::Accept => P::Joined,
                CallAction::Decline => P::Declined,
                CallAction::Hangup => P::Left,
            };
            prop_assert_eq!(t.participant, want);
        }
    }

    /// 5. Result session ends only when the doc contract says it should.
    #[test]
    fn calls_session_end_rule(session in session(), actor in participant(), others in others(), action in call_action()) {
        if let Ok(t) = calls_apply(session, actor, &others, action) {
            match action {
                CallAction::Accept => prop_assert_eq!(t.session, S::Active),
                CallAction::Decline => {
                    let others_active = others.iter().any(|p| matches!(p, P::Ringing | P::Joined));
                    prop_assert_eq!(t.session == S::Ended, !others_active);
                }
                CallAction::Hangup => {
                    let others_joined = others.contains(&P::Joined);
                    prop_assert_eq!(t.session == S::Ended, !others_joined);
                }
            }
        }
    }
}

fn hold_state() -> impl Strategy<Value = HoldState> {
    prop_oneof![
        Just(HoldState::Held),
        Just(HoldState::Captured),
        Just(HoldState::Released),
    ]
}

fn hold_action() -> impl Strategy<Value = HoldAction> {
    prop_oneof![Just(HoldAction::Capture), Just(HoldAction::Release)]
}

proptest! {
    /// 6. Totality + the full transition table: terminal absorbs, Held moves.
    #[test]
    fn hold_never_panics(state in hold_state(), action in hold_action()) {
        let got = hold_apply(state, action);
        match state {
            HoldState::Captured | HoldState::Released => prop_assert!(got.is_err()),
            HoldState::Held => {
                let want = match action {
                    HoldAction::Capture => HoldState::Captured,
                    HoldAction::Release => HoldState::Released,
                };
                prop_assert_eq!(got, Ok(want));
            }
        }
    }

    /// 7. Absorption over a stream: folding arbitrary actions from `Held`, the
    ///    first succeeds (Held → terminal) and every later one is `Err`; the
    ///    running state moves exactly once, then stays.
    #[test]
    fn hold_absorption_stream(actions in proptest::collection::vec(hold_action(), 0..12)) {
        let mut state = HoldState::Held;
        let mut terminal = false;
        for a in actions {
            let got = hold_apply(state, a);
            if terminal {
                prop_assert!(got.is_err());
            } else {
                prop_assert_eq!(state, HoldState::Held);
                let next = got.expect("Held + any action is legal");
                prop_assert!(matches!(next, HoldState::Captured | HoldState::Released));
                state = next;
                terminal = true;
            }
        }
    }
}
