//! Worker-side prepare/execute session state: which digest may execute, once.
//!
//! Portable on purpose: the rule "exactly one prepared plan executes,
//! exactly once, in the session that prepared it" is the same on every
//! platform. Native identity evidence - which peer, which process, which
//! elevation - stays in the backend crates; this tracks only the protocol
//! state that binding needs.
//!
//! The worker enforces, in order:
//!
//! ```text
//! Prepare   → at most one per session; binds the prepared digest
//! Execute   → only the prepared digest, only once
//! anything else after Execute → replay, refused
//! ```

use crate::{SessionId, WireError};

/// One worker session's prepare/execute state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivilegedSession {
    session: SessionId,
    state: PrivilegedSessionState,
}

/// Where one session is in the prepare/execute handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PrivilegedSessionState {
    /// Nothing prepared yet.
    AcceptingPrepare,
    /// One plan prepared and awaiting its exact execute.
    Prepared { plan_digest: String },
    /// The prepared plan executed. Terminal: nothing further executes.
    Executed,
}

impl PrivilegedSession {
    /// A fresh session that has prepared nothing.
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            state: PrivilegedSessionState::AcceptingPrepare,
        }
    }

    /// The session this tracker serves.
    pub fn session(&self) -> SessionId {
        self.session
    }

    /// Record a preparation, binding its digest to this session.
    ///
    /// At most one preparation per session: a second intent is a new
    /// authorization, and a new authorization is a new session. Collapsing
    /// two prepares into one session would let a later intent inherit an
    /// earlier confirmation.
    pub fn prepared(&mut self, session: SessionId, plan_digest: &str) -> Result<(), WireError> {
        if session != self.session {
            return Err(WireError::SessionMismatch);
        }
        if !matches!(self.state, PrivilegedSessionState::AcceptingPrepare) {
            return Err(WireError::UnexpectedMessage);
        }
        if plan_digest.len() != 64 || !plan_digest.chars().all(|char| char.is_ascii_hexdigit()) {
            return Err(WireError::Malformed(
                "plan digest is not SHA-256 hex".into(),
            ));
        }
        self.state = PrivilegedSessionState::Prepared {
            plan_digest: plan_digest.to_lowercase(),
        };
        Ok(())
    }

    /// Authorize execution of exactly the prepared plan, once.
    ///
    /// A digest that differs from the prepared one is a substitution; an
    /// execute after execution is a replay; an execute before any preparation
    /// authorizes nothing. All three are refused without mutating anything.
    pub fn execute(&mut self, session: SessionId, plan_digest: &str) -> Result<(), WireError> {
        if session != self.session {
            return Err(WireError::SessionMismatch);
        }
        match &self.state {
            PrivilegedSessionState::Prepared { plan_digest: bound } => {
                if plan_digest.to_lowercase() != *bound {
                    return Err(WireError::PlanHashMismatch);
                }
                self.state = PrivilegedSessionState::Executed;
                Ok(())
            }
            PrivilegedSessionState::AcceptingPrepare => Err(WireError::UnexpectedMessage),
            PrivilegedSessionState::Executed => Err(WireError::Replay),
        }
    }

    /// Whether this session has executed and must serve nothing further.
    pub fn executed(&self) -> bool {
        matches!(self.state, PrivilegedSessionState::Executed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST_A: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    const DIGEST_B: &str = "5feceb66ffc86f38d952786c6d696c79c2dbc239dd4e91b46729d73a27fb57e9";

    fn session() -> SessionId {
        SessionId(uuid::Uuid::now_v7())
    }

    #[test]
    fn prepare_then_execute_with_the_prepared_digest_succeeds_once() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(!tracker.executed());
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        tracker.execute(id, DIGEST_A).expect("exact execute runs");
        assert!(tracker.executed());
    }

    #[test]
    fn execute_before_prepare_authorizes_nothing() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(matches!(
            tracker.execute(id, DIGEST_A),
            Err(WireError::UnexpectedMessage)
        ));
    }

    #[test]
    fn a_different_digest_is_a_substitution_not_an_authorization() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        assert!(matches!(
            tracker.execute(id, DIGEST_B),
            Err(WireError::PlanHashMismatch)
        ));
        assert!(!tracker.executed());
    }

    #[test]
    fn a_second_execute_is_a_replay() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        tracker.execute(id, DIGEST_A).expect("first execute runs");
        assert!(tracker.execute(id, DIGEST_A).is_err());
    }

    #[test]
    fn a_second_prepare_is_a_new_authorization_not_an_update() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        tracker.prepared(id, DIGEST_A).expect("prepare binds");
        assert!(matches!(
            tracker.prepared(id, DIGEST_B),
            Err(WireError::UnexpectedMessage)
        ));
    }

    #[test]
    fn another_session_binds_nothing_here() {
        let mut tracker = PrivilegedSession::new(session());
        let stranger = session();
        assert!(matches!(
            tracker.prepared(stranger, DIGEST_A),
            Err(WireError::SessionMismatch)
        ));
        assert!(matches!(
            tracker.execute(stranger, DIGEST_A),
            Err(WireError::SessionMismatch)
        ));
    }

    #[test]
    fn a_malformed_digest_prepares_nothing() {
        let id = session();
        let mut tracker = PrivilegedSession::new(id);
        assert!(tracker.prepared(id, "not-a-digest").is_err());
        assert!(matches!(
            tracker.execute(id, DIGEST_A),
            Err(WireError::UnexpectedMessage)
        ));
    }
}
