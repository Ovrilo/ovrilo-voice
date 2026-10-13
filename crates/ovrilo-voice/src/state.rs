use std::fmt;

/// Voice room connection lifecycle. The engine is authoritative for this state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

/// Returned when a requested lifecycle transition is not allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: ConnectionState,
    pub to: ConnectionState,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid voice state transition {:?} -> {:?}",
            self.from, self.to
        )
    }
}

impl std::error::Error for InvalidTransition {}

impl ConnectionState {
    /// Validates a transition. Moving to `Disconnected` is always allowed so leave stays idempotent.
    pub fn transition(self, to: ConnectionState) -> Result<ConnectionState, InvalidTransition> {
        use ConnectionState::*;
        let allowed = matches!(
            (self, to),
            (_, Disconnected)
                | (Disconnected, Connecting)
                | (Connecting, Connected)
                | (Connected, Reconnecting)
                | (Reconnecting, Connected)
        );
        if allowed {
            Ok(to)
        } else {
            Err(InvalidTransition { from: self, to })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConnectionState::*;

    #[test]
    fn follows_happy_path() {
        let s = Disconnected.transition(Connecting).unwrap();
        let s = s.transition(Connected).unwrap();
        let s = s.transition(Reconnecting).unwrap();
        let s = s.transition(Connected).unwrap();
        assert_eq!(s.transition(Disconnected), Ok(Disconnected));
    }

    #[test]
    fn leave_is_idempotent() {
        assert_eq!(Disconnected.transition(Disconnected), Ok(Disconnected));
    }

    #[test]
    fn rejects_skipping_connecting() {
        assert!(Disconnected.transition(Connected).is_err());
        assert!(Connecting.transition(Reconnecting).is_err());
    }
}
