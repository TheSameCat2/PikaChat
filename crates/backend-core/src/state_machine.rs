use crate::{
    error::BackendError,
    types::{BackendCommand, BackendEvent, BackendLifecycleState},
};

#[derive(Debug, Clone)]
pub struct BackendStateMachine {
    state: BackendLifecycleState,
}

impl Default for BackendStateMachine {
    fn default() -> Self {
        Self {
            state: BackendLifecycleState::Cold,
        }
    }
}

impl BackendStateMachine {
    pub fn state(&self) -> BackendLifecycleState {
        self.state
    }

    pub fn apply(&mut self, command: &BackendCommand) -> Result<Vec<BackendEvent>, BackendError> {
        use BackendCommand::*;

        match command {
            Init { .. } => self.transition_from_cold(BackendLifecycleState::Configured, "init"),
            LoginPassword { .. } | RestoreSession => self.transition_from_any_of(
                &[
                    BackendLifecycleState::Configured,
                    BackendLifecycleState::LoggedOut,
                ],
                BackendLifecycleState::Authenticating,
                "login_or_restore",
            ),
            StartSync => self.transition_from_state(
                BackendLifecycleState::Authenticated,
                BackendLifecycleState::Syncing,
                "start_sync",
            ),
            StopSync => self.transition_from_state(
                BackendLifecycleState::Syncing,
                BackendLifecycleState::Authenticated,
                "stop_sync",
            ),
            Logout => self.transition_from_any_of(
                &[
                    BackendLifecycleState::Configured,
                    BackendLifecycleState::Authenticating,
                    BackendLifecycleState::Authenticated,
                    BackendLifecycleState::Syncing,
                ],
                BackendLifecycleState::LoggedOut,
                "logout",
            ),
            ListRooms
            | OpenRoom { .. }
            | PaginateBack { .. }
            | SendMessage { .. }
            | EditMessage { .. }
            | RedactMessage { .. } => {
                if self.is_authenticated_context() {
                    Ok(Vec::new())
                } else {
                    Err(BackendError::invalid_state(
                        self.state,
                        "room/timeline command",
                    ))
                }
            }
        }
    }

    pub fn on_auth_result(&mut self, success: bool) -> Result<BackendEvent, BackendError> {
        if self.state != BackendLifecycleState::Authenticating {
            return Err(BackendError::invalid_state(self.state, "on_auth_result"));
        }

        let next = if success {
            BackendLifecycleState::Authenticated
        } else {
            BackendLifecycleState::Configured
        };

        self.state = next;
        Ok(BackendEvent::StateChanged { state: next })
    }

    pub fn on_fatal(&mut self) -> BackendEvent {
        self.state = BackendLifecycleState::Fatal;
        BackendEvent::StateChanged {
            state: BackendLifecycleState::Fatal,
        }
    }

    fn is_authenticated_context(&self) -> bool {
        matches!(
            self.state,
            BackendLifecycleState::Authenticated | BackendLifecycleState::Syncing
        )
    }

    fn transition_from_cold(
        &mut self,
        next: BackendLifecycleState,
        action: &str,
    ) -> Result<Vec<BackendEvent>, BackendError> {
        if self.state != BackendLifecycleState::Cold {
            return Err(BackendError::invalid_state(self.state, action));
        }
        self.state = next;
        Ok(vec![BackendEvent::StateChanged { state: next }])
    }

    fn transition_from_state(
        &mut self,
        expected: BackendLifecycleState,
        next: BackendLifecycleState,
        action: &str,
    ) -> Result<Vec<BackendEvent>, BackendError> {
        if self.state != expected {
            return Err(BackendError::invalid_state(self.state, action));
        }
        self.state = next;
        Ok(vec![BackendEvent::StateChanged { state: next }])
    }

    fn transition_from_any_of(
        &mut self,
        expected: &[BackendLifecycleState],
        next: BackendLifecycleState,
        action: &str,
    ) -> Result<Vec<BackendEvent>, BackendError> {
        if !expected.contains(&self.state) {
            return Err(BackendError::invalid_state(self.state, action));
        }
        self.state = next;
        Ok(vec![BackendEvent::StateChanged { state: next }])
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::types::{BackendCommand, MessageType};

    fn init_command() -> BackendCommand {
        BackendCommand::Init {
            homeserver: "https://matrix.example.org".to_owned(),
            data_dir: PathBuf::from("/tmp/pikachat"),
        }
    }

    #[test]
    fn runs_happy_path_state_transitions() {
        let mut sm = BackendStateMachine::default();

        sm.apply(&init_command()).expect("init must work");
        assert_eq!(sm.state(), BackendLifecycleState::Configured);

        sm.apply(&BackendCommand::LoginPassword {
            user_id_or_localpart: "@alice:example.org".into(),
            password: "secret".into(),
        })
        .expect("login command must work");
        assert_eq!(sm.state(), BackendLifecycleState::Authenticating);

        sm.on_auth_result(true).expect("auth should resolve");
        assert_eq!(sm.state(), BackendLifecycleState::Authenticated);

        sm.apply(&BackendCommand::StartSync)
            .expect("start sync should work");
        assert_eq!(sm.state(), BackendLifecycleState::Syncing);

        sm.apply(&BackendCommand::StopSync)
            .expect("stop sync should work");
        assert_eq!(sm.state(), BackendLifecycleState::Authenticated);

        sm.apply(&BackendCommand::Logout)
            .expect("logout should work");
        assert_eq!(sm.state(), BackendLifecycleState::LoggedOut);
    }

    #[test]
    fn rejects_sync_without_auth() {
        let mut sm = BackendStateMachine::default();
        sm.apply(&init_command()).expect("init must work");

        let err = sm
            .apply(&BackendCommand::StartSync)
            .expect_err("start sync should fail without auth");
        assert_eq!(err.code, "invalid_state_transition");
    }

    #[test]
    fn rejects_timeline_commands_outside_authenticated_context() {
        let mut sm = BackendStateMachine::default();
        sm.apply(&init_command()).expect("init must work");

        let err = sm
            .apply(&BackendCommand::SendMessage {
                room_id: "!abc:example.org".into(),
                client_txn_id: "tx-1".into(),
                body: "hello".into(),
                msgtype: MessageType::Text,
            })
            .expect_err("timeline command should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");
    }
}
