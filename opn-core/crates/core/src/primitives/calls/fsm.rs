//! Call finite-state machine as data (OPN-CORE.md §10.4, roadmap Sprint 6
//! item 2): one pure `apply` function, no transition logic scattered across
//! handlers. Handlers load the session + participant rows `FOR UPDATE`, call
//! `apply`, persist the result, and emit; an illegal transition is `conflict`.
//!
//! Terminal rule: nothing leaves `Ended`, enforced structurally — `apply`
//! returns `Err` for any action against an ended session, and no arm produces a
//! transition *from* `Ended`. This is the Sprint 9 proptest target; keep it
//! pure (no I/O, no clock, no allocation beyond the slice it is handed).

use contracts::{CallParticipantState as P, CallSessionState as S};

/// A participant-initiated action. `calls.start` is not here — it creates a
/// fresh session rather than transitioning one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Accept,
    Decline,
    Hangup,
}

/// A legal transition: the actor's new participant state and the session's
/// (possibly unchanged) new state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transition {
    pub participant: P,
    pub session: S,
}

/// Apply `action` by the actor whose current state is `actor`, given the states
/// of the *other* participants. `Err(())` = illegal transition → the caller
/// acks `conflict`.
///
/// - **Accept**: actor must be `Ringing` → `Joined`, session → `Active`.
/// - **Decline**: actor must be `Ringing` → `Declined`; the session ends iff no
///   other participant is still `Ringing` or `Joined` (decline-all / timeout).
/// - **Hangup**: actor must be `Joined` → `Left`; the session ends iff no other
///   participant is still `Joined` (last-hangup). A caller hanging up a ring the
///   callee never answered is exactly this: the caller is the only `Joined`, so
///   the session ends and the ring is cancelled.
///
/// An ended session absorbs everything: any action → `Err`.
// `Err(())` means "illegal" with no further detail — the caller maps it to
// `conflict`. A richer error type would carry nothing this FSM produces.
#[allow(clippy::result_unit_err)]
pub fn apply(session: S, actor: P, others: &[P], action: Action) -> Result<Transition, ()> {
    if session == S::Ended {
        return Err(());
    }
    match action {
        Action::Accept => {
            if actor != P::Ringing {
                return Err(());
            }
            Ok(Transition {
                participant: P::Joined,
                session: S::Active,
            })
        }
        Action::Decline => {
            if actor != P::Ringing {
                return Err(());
            }
            let others_active = others.iter().any(|s| matches!(s, P::Ringing | P::Joined));
            Ok(Transition {
                participant: P::Declined,
                session: if others_active { session } else { S::Ended },
            })
        }
        Action::Hangup => {
            if actor != P::Joined {
                return Err(());
            }
            let others_joined = others.contains(&P::Joined);
            Ok(Transition {
                participant: P::Left,
                session: if others_joined { session } else { S::Ended },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1:1 happy path: callee accepts a ringing call → both joined, session
    /// active; then the last hangup ends it.
    #[test]
    fn accept_then_last_hangup() {
        // Callee (Ringing) accepts; caller is the other party (Joined).
        let t =
            apply(S::Ringing, P::Ringing, &[P::Joined], Action::Accept).expect("legal transition");
        assert_eq!(t.participant, P::Joined);
        assert_eq!(t.session, S::Active);

        // One of two joined hangs up; the other still joined → stays active.
        let t =
            apply(S::Active, P::Joined, &[P::Joined], Action::Hangup).expect("legal transition");
        assert_eq!(t.participant, P::Left);
        assert_eq!(t.session, S::Active);

        // The last joined hangs up (other already Left) → session ends.
        let t = apply(S::Active, P::Joined, &[P::Left], Action::Hangup).expect("legal transition");
        assert_eq!(t.participant, P::Left);
        assert_eq!(t.session, S::Ended);
    }

    /// Caller cancels a ring the callee never answered: caller (only Joined)
    /// hangs up while the callee is still Ringing → session ends.
    #[test]
    fn caller_hangup_cancels_ring() {
        let t =
            apply(S::Ringing, P::Joined, &[P::Ringing], Action::Hangup).expect("legal transition");
        assert_eq!(t.participant, P::Left);
        assert_eq!(t.session, S::Ended);
    }

    /// Sole callee declines a 1:1 ring → session ends (decline-all).
    #[test]
    fn decline_all_ends_session() {
        // Others = the caller, who is Joined, so declining does NOT end it in a
        // 1:1 dialer flow — the caller is still "in" and will hang up. But if
        // the only other party had already left/declined, it ends.
        let t =
            apply(S::Ringing, P::Ringing, &[P::Joined], Action::Decline).expect("legal transition");
        assert_eq!(t.participant, P::Declined);
        assert_eq!(t.session, S::Ringing, "caller still Joined keeps it alive");

        let t =
            apply(S::Ringing, P::Ringing, &[P::Left], Action::Decline).expect("legal transition");
        assert_eq!(t.session, S::Ended, "no one left active → ends");
    }

    /// Illegal transitions all map to `Err` (→ conflict).
    #[test]
    fn illegal_transitions_conflict() {
        // Accept when not ringing.
        assert!(apply(S::Active, P::Joined, &[], Action::Accept).is_err());
        assert!(apply(S::Active, P::Declined, &[], Action::Accept).is_err());
        // Decline when not ringing.
        assert!(apply(S::Active, P::Joined, &[], Action::Decline).is_err());
        // Hangup when not joined.
        assert!(apply(S::Ringing, P::Ringing, &[], Action::Hangup).is_err());
        assert!(apply(S::Ringing, P::Declined, &[], Action::Hangup).is_err());
    }

    /// Terminal absorption: an ended session rejects every action.
    #[test]
    fn ended_absorbs_everything() {
        for actor in [P::Ringing, P::Joined, P::Declined, P::Left] {
            for action in [Action::Accept, Action::Decline, Action::Hangup] {
                assert!(
                    apply(S::Ended, actor, &[P::Joined], action).is_err(),
                    "Ended + {actor:?} + {action:?} must be illegal"
                );
            }
        }
    }

    /// Exhaustive legality over every (session × actor × action) cell — the
    /// Sprint 6 exit criterion. Legality depends only on this triple (`others`
    /// affects the *resulting* session state, covered by the targeted tests
    /// above), so 36 cells settle the whole table. The legal set is a literal
    /// list, NOT a predicate — a predicate would just mirror the
    /// implementation and prove nothing.
    #[test]
    fn transition_table_exhaustive() {
        use Action::*;
        // (session, actor, action, actor's resulting state)
        const LEGAL: &[(S, P, Action, P)] = &[
            (S::Ringing, P::Ringing, Accept, P::Joined),
            (S::Active, P::Ringing, Accept, P::Joined),
            (S::Ringing, P::Ringing, Decline, P::Declined),
            (S::Active, P::Ringing, Decline, P::Declined),
            (S::Ringing, P::Joined, Hangup, P::Left),
            (S::Active, P::Joined, Hangup, P::Left),
        ];
        for session in [S::Ringing, S::Active, S::Ended] {
            for actor in [P::Ringing, P::Joined, P::Declined, P::Left] {
                for action in [Accept, Decline, Hangup] {
                    let legal = LEGAL
                        .iter()
                        .find(|(s, a, ac, _)| *s == session && *a == actor && *ac == action);
                    // `others` context is irrelevant to legality; use one with
                    // an active party so a legal cell never ends the session.
                    let got = apply(session, actor, &[P::Joined], action);
                    match legal {
                        Some((_, _, _, want)) => {
                            let t = got.unwrap_or_else(|()| {
                                panic!("{session:?}+{actor:?}+{action:?} must be legal")
                            });
                            assert_eq!(
                                t.participant, *want,
                                "{session:?}+{actor:?}+{action:?} wrong actor state"
                            );
                        }
                        None => assert!(
                            got.is_err(),
                            "{session:?}+{actor:?}+{action:?} must be illegal"
                        ),
                    }
                }
            }
        }
    }
}
