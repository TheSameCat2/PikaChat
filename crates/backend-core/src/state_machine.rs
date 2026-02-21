use crate::{
    error::BackendError,
    types::{BackendCommand, BackendEvent, BackendLifecycleState},
};

/// Deterministic backend lifecycle transition guard.
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
    /// Current backend lifecycle state.
    pub fn state(&self) -> BackendLifecycleState {
        self.state
    }

    /// Apply a command transition and return state-change events to emit.
    ///
    /// Commands that do not transition state may return an empty event vector.
    pub fn apply(&mut self, command: &BackendCommand) -> Result<Vec<BackendEvent>, BackendError> {
        use BackendCommand::*;

        match command {
            Init { .. } => self.transition_from_any_of(
                &[
                    BackendLifecycleState::Cold,
                    BackendLifecycleState::Configured,
                    BackendLifecycleState::LoggedOut,
                ],
                BackendLifecycleState::Configured,
                "init",
            ),
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
            | AcceptRoomInvite { .. }
            | RejectRoomInvite { .. }
            | OpenRoom { .. }
            | PaginateBack { .. }
            | SendDmText { .. }
            | SendMessage { .. }
            | SendMediaMessage { .. }
            | EditMessage { .. }
            | RedactMessage { .. }
            | UploadMedia { .. }
            | DownloadMedia { .. }
            | GetRecoveryStatus
            | EnableRecovery { .. }
            | ResetRecovery { .. }
            | RecoverSecrets { .. } => {
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

    /// Resolve an auth attempt and transition out of `Authenticating`.
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

    /// Force state machine into fatal state.
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
            config: None,
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
            persist_session: true,
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

        let err = sm
            .apply(&BackendCommand::SendDmText {
                user_id: "@alice:example.org".into(),
                client_txn_id: "tx-2".into(),
                body: "hello".into(),
            })
            .expect_err("dm command should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::UploadMedia {
                client_txn_id: "tx-3".into(),
                content_type: "text/plain".into(),
                data: vec![1, 2, 3],
            })
            .expect_err("media upload command should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::DownloadMedia {
                client_txn_id: "tx-4".into(),
                source: "mxc://example.org/media".into(),
            })
            .expect_err("media download command should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::GetRecoveryStatus)
            .expect_err("recovery status should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::EnableRecovery {
                client_txn_id: "tx-5".into(),
                passphrase: None,
                wait_for_backups_to_upload: false,
            })
            .expect_err("enable recovery should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::RecoverSecrets {
                client_txn_id: "tx-6".into(),
                recovery_key: "key".into(),
            })
            .expect_err("recover secrets should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::ResetRecovery {
                client_txn_id: "tx-7".into(),
                passphrase: None,
                wait_for_backups_to_upload: false,
            })
            .expect_err("reset recovery should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::AcceptRoomInvite {
                room_id: "!abc:example.org".into(),
                client_txn_id: "tx-8".into(),
            })
            .expect_err("accept invite should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");

        let err = sm
            .apply(&BackendCommand::RejectRoomInvite {
                room_id: "!abc:example.org".into(),
                client_txn_id: "tx-9".into(),
            })
            .expect_err("reject invite should fail when not authenticated");
        assert_eq!(err.code, "invalid_state_transition");
    }

    #[test]
    fn allows_invite_commands_in_authenticated_context() {
        let mut sm = BackendStateMachine::default();
        sm.apply(&init_command()).expect("init must work");
        sm.apply(&BackendCommand::LoginPassword {
            user_id_or_localpart: "@alice:example.org".into(),
            password: "secret".into(),
            persist_session: true,
        })
        .expect("login command must work");
        sm.on_auth_result(true).expect("auth should resolve");

        sm.apply(&BackendCommand::AcceptRoomInvite {
            room_id: "!abc:example.org".into(),
            client_txn_id: "tx-1".into(),
        })
        .expect("accept invite should work in authenticated state");

        sm.apply(&BackendCommand::StartSync)
            .expect("start sync should work");
        sm.apply(&BackendCommand::RejectRoomInvite {
            room_id: "!abc:example.org".into(),
            client_txn_id: "tx-2".into(),
        })
        .expect("reject invite should work in syncing state");
    }

    #[test]
    fn allows_reinit_after_logout() {
        let mut sm = BackendStateMachine::default();
        sm.apply(&init_command()).expect("init must work");
        sm.apply(&BackendCommand::Logout)
            .expect("logout should work");
        assert_eq!(sm.state(), BackendLifecycleState::LoggedOut);

        sm.apply(&init_command())
            .expect("init should work again after logout");
        assert_eq!(sm.state(), BackendLifecycleState::Configured);
    }
}
