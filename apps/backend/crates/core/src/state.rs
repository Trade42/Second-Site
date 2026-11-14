//! Port of `apps/api/app/contracts/state.py`: the guarded state machine helper used by approvals,
//! calls, handoffs and tasks.

use std::fmt;

/// `state.InvalidStateTransition`. The message is identical to Python's `str(exc)`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{machine}: transition {current} -> {target} is not allowed")]
pub struct InvalidStateTransition {
    pub machine: String,
    pub current: String,
    pub target: String,
}

/// A transition table: `(state, allowed next states)`. A state with no entry allows nothing,
/// as Python's `table.get(current, frozenset())`.
pub type TransitionTable<S> = [(S, &'static [S])];

/// `state.check_transition`: return `target` when `current -> target` is in `table`.
pub fn check_transition<S>(
    machine: &str,
    table: &TransitionTable<S>,
    current: S,
    target: S,
) -> Result<S, InvalidStateTransition>
where
    S: Copy + PartialEq + fmt::Display,
{
    let allowed = table
        .iter()
        .find(|(from, _)| *from == current)
        .map(|(_, to)| *to)
        .unwrap_or(&[]);
    if allowed.contains(&target) {
        Ok(target)
    } else {
        Err(InvalidStateTransition {
            machine: machine.to_owned(),
            current: current.to_string(),
            target: target.to_string(),
        })
    }
}
