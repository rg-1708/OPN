//! Hold finite-state machine as data (OPN-CORE.md §10.5, roadmap Sprint 7 item
//! 3): the same pure-function pattern as `calls/fsm.rs`. A hold reserves balance;
//! `capture` settles it to a destination, `release` frees it. `Captured` and
//! `Released` are terminal — nothing leaves them (the janitor's expiry sweep is a
//! `Release`). Kept pure (no I/O, no clock) so it is the Sprint 9 proptest target.

/// A hold's lifecycle state. The DB stores these lowercase; `store.rs` maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldState {
    Held,
    Captured,
    Released,
}

/// An action against a hold. `Capture` settles it (moves the reserved amount to a
/// destination); `Release` frees it (the janitor's expiry sweep is a `Release`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Capture,
    Release,
}

/// Apply `action` to a hold in `state`. `Err(())` = illegal — a terminal hold
/// (already captured or released) absorbs everything → the caller acks
/// `conflict`.
// `Err(())` means "illegal" with no further detail — the caller maps it to
// `conflict`, exactly as calls/fsm.rs does.
#[allow(clippy::result_unit_err)]
pub fn apply(state: HoldState, action: Action) -> Result<HoldState, ()> {
    match (state, action) {
        (HoldState::Held, Action::Capture) => Ok(HoldState::Captured),
        (HoldState::Held, Action::Release) => Ok(HoldState::Released),
        // Captured / Released are terminal.
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_captures_and_releases() {
        assert_eq!(
            apply(HoldState::Held, Action::Capture),
            Ok(HoldState::Captured)
        );
        assert_eq!(
            apply(HoldState::Held, Action::Release),
            Ok(HoldState::Released)
        );
    }

    /// Terminal absorption: a captured or released hold rejects every action.
    #[test]
    fn terminal_absorbs_everything() {
        for state in [HoldState::Captured, HoldState::Released] {
            for action in [Action::Capture, Action::Release] {
                assert!(
                    apply(state, action).is_err(),
                    "{state:?} + {action:?} must be illegal"
                );
            }
        }
    }

    /// Exhaustive over every (state × action) cell — the legal set is a literal
    /// list, NOT a predicate (a predicate would mirror the implementation and
    /// prove nothing). Sprint 9's proptest sits on top of this, not instead.
    #[test]
    fn transition_table_exhaustive() {
        use Action::*;
        use HoldState::*;
        const LEGAL: &[(HoldState, Action, HoldState)] =
            &[(Held, Capture, Captured), (Held, Release, Released)];
        for state in [Held, Captured, Released] {
            for action in [Capture, Release] {
                let legal = LEGAL.iter().find(|(s, a, _)| *s == state && *a == action);
                match legal {
                    Some((_, _, want)) => {
                        assert_eq!(apply(state, action), Ok(*want), "{state:?} + {action:?}")
                    }
                    None => assert!(
                        apply(state, action).is_err(),
                        "{state:?} + {action:?} must be illegal"
                    ),
                }
            }
        }
    }
}
