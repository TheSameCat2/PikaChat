//! Backend bridge that wires Matrix runtime events into UI state snapshots.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use arboard::Clipboard;
use backend_core::types::InviteAction;
use backend_core::{BackendCommand, BackendEvent, MessageType};
use backend_matrix::MatrixFrontendAdapter;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn};

use crate::{
    config::DesktopConfig,
    state::{DesktopSnapshot, DesktopState},
};

/// Callback used to publish new UI snapshots.
pub type UiUpdateCallback = Arc<dyn Fn(DesktopSnapshot) + Send + Sync + 'static>;

/// Bridges UI actions and backend events using the shared frontend adapter.
pub struct DesktopBridge {
    command_tx: mpsc::UnboundedSender<BackendCommand>,
    state: Arc<Mutex<DesktopState>>,
    clipboard: Mutex<Option<Clipboard>>,
    ui_update: UiUpdateCallback,
    next_txn_id: AtomicU64,
    paginate_limit: u16,
    pagination_top_threshold_px: f32,
    pagination_cooldown_ms: u64,
    command_task: tokio::task::JoinHandle<()>,
    event_task: tokio::task::JoinHandle<()>,
}

impl DesktopBridge {
    /// Start backend command/event workers and enqueue startup auth/sync commands.
    pub fn spawn(
        config: DesktopConfig,
        adapter: Arc<MatrixFrontendAdapter>,
        runtime_handle: tokio::runtime::Handle,
        ui_update: UiUpdateCallback,
    ) -> Arc<Self> {
        info!(
            homeserver = %config.homeserver,
            user_id = %config.user_id,
            data_dir = %config.data_dir.display(),
            timeline_max_items = config.timeline_max_items,
            paginate_limit = config.paginate_limit,
            "spawning desktop bridge"
        );
        let state = Arc::new(Mutex::new(DesktopState::new(
            config.user_id.clone(),
            config.timeline_max_items,
        )));

        let (command_tx, mut command_rx) = mpsc::unbounded_channel::<BackendCommand>();
        let adapter_for_command_task = Arc::clone(&adapter);
        let command_task = runtime_handle.spawn(async move {
            debug!("desktop command worker started");
            while let Some(command) = command_rx.recv().await {
                debug!(command = command_kind(&command), "queueing backend command");
                adapter_for_command_task.enqueue(command);
                if let Err(err) = adapter_for_command_task.flush_all().await {
                    error!(error = %err, "failed to flush backend command queue");
                }
            }
            debug!("desktop command worker exiting");
        });

        let state_for_events = Arc::clone(&state);
        let ui_update_for_events = Arc::clone(&ui_update);
        let command_tx_for_events = command_tx.clone();
        let login_command = login_password_command(&config);
        let mut events = adapter.subscribe();
        let event_task = runtime_handle.spawn(async move {
            debug!("desktop event worker started");
            let mut post_auth_enqueued = false;
            enum AuthPhase {
                RestoringSession,
                LoggingIn,
                Done,
            }
            let mut auth_phase = AuthPhase::RestoringSession;
            loop {
                let event = match recv_event(&mut events).await {
                    Ok(event) => event,
                    Err(()) => break,
                };
                debug!(event = event_kind(&event), "received backend event");
                if let BackendEvent::AuthResult { success, .. } = &event {
                    if *success && !post_auth_enqueued {
                        for command in post_auth_command_sequence() {
                            debug!(
                                command = command_kind(&command),
                                "enqueue post-auth startup command"
                            );
                            if command_tx_for_events.send(command).is_err() {
                                error!("failed to enqueue post-auth startup command");
                                break;
                            }
                        }
                        post_auth_enqueued = true;
                        auth_phase = AuthPhase::Done;
                    } else if !*success {
                        if matches!(auth_phase, AuthPhase::RestoringSession) {
                            debug!(
                                command = command_kind(&login_command),
                                "session restore failed; enqueueing password login"
                            );
                            if command_tx_for_events.send(login_command.clone()).is_err() {
                                error!("failed to enqueue fallback password login");
                            }
                            auth_phase = AuthPhase::LoggingIn;
                        } else if matches!(auth_phase, AuthPhase::LoggingIn) {
                            auth_phase = AuthPhase::Done;
                        }
                    }
                }

                let accepted_invite_room_id = match &event {
                    BackendEvent::InviteActionAck(ack)
                        if ack.error_code.is_none() && ack.action == InviteAction::Accept =>
                    {
                        Some(ack.room_id.clone())
                    }
                    _ => None,
                };

                let snapshot = {
                    let mut state = state_for_events
                        .lock()
                        .expect("desktop state lock poisoned while handling backend event");
                    state.handle_backend_event(event);
                    if let Some(room_id) = accepted_invite_room_id.as_ref() {
                        state.select_room(room_id.clone());
                    }
                    state.snapshot()
                };
                (ui_update_for_events)(snapshot);

                if let Some(room_id) = accepted_invite_room_id {
                    debug!(%room_id, "invite accepted; opening room timeline");
                    if command_tx_for_events
                        .send(room_open_command(room_id))
                        .is_err()
                    {
                        error!("failed to enqueue OpenRoom after invite accept");
                    }
                }
            }
            warn!("desktop event worker exiting: backend event stream closed");
        });

        let bridge = Arc::new(Self {
            command_tx,
            state,
            clipboard: Mutex::new(None),
            ui_update,
            next_txn_id: AtomicU64::new(1),
            paginate_limit: config.paginate_limit.max(1),
            pagination_top_threshold_px: config.pagination_top_threshold_px,
            pagination_cooldown_ms: config.pagination_cooldown_ms,
            command_task,
            event_task,
        });

        bridge.publish_snapshot();
        for command in startup_command_sequence(&config) {
            debug!(command = command_kind(&command), "enqueue startup command");
            bridge.enqueue_command(command);
        }

        bridge
    }

    /// Select a room by sidebar index and load timeline history for it.
    pub fn select_room_by_index(&self, index: i32) {
        debug!(index, "select_room_by_index called");
        if index < 0 {
            warn!(index, "ignoring negative room index");
            return;
        }

        let maybe_room_id = {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while selecting room");
            let room_id = state.select_room_by_index(index as usize);
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
            room_id
        };

        if let Some(room_id) = maybe_room_id {
            info!(%room_id, "selected room");
            self.enqueue_command(room_open_command(room_id));
        } else {
            warn!(index, "room selection ignored: index out of bounds");
        }
    }

    /// Attempt to send message text to the currently selected room.
    ///
    /// Returns `true` when a send command was queued.
    pub fn send_message(&self, body: String) -> bool {
        let body = body.trim().to_owned();
        if body.is_empty() {
            debug!("ignoring empty send request");
            return false;
        }

        let (room_id, client_txn_id) = {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while sending message");
            let Some(room_id) = state.selected_room_id().map(|value| value.to_owned()) else {
                state.set_error_text("Select a room before sending messages.");
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                warn!("send request rejected: no room selected");
                return false;
            };
            if !state.can_send_message() {
                state.set_error_text("Accept this invite before sending messages.");
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                warn!(%room_id, "send request rejected: selected room is not joined");
                return false;
            }

            let client_txn_id = format!(
                "desktop-send-{}",
                self.next_txn_id.fetch_add(1, Ordering::Relaxed)
            );
            state.mark_send_requested(client_txn_id.clone());
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);

            (room_id, client_txn_id)
        };

        info!(
            room_id = %room_id,
            client_txn_id = %client_txn_id,
            body_len = body.len(),
            "queueing room send"
        );
        self.enqueue_command(send_message_command(room_id, client_txn_id, body));
        true
    }

    /// Accept a room invite by room ID.
    pub fn accept_room_invite(&self, room_id: String) {
        self.request_invite_action(room_id, InviteAction::Accept);
    }

    /// Reject a room invite by room ID.
    pub fn reject_room_invite(&self, room_id: String) {
        self.request_invite_action(room_id, InviteAction::Reject);
    }

    /// Called by UI scroll updates to trigger near-top pagination.
    pub fn on_timeline_scrolled(&self, viewport_y: f32) {
        let now_ms = now_millis();
        let maybe_request = {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while handling timeline scroll");
            state.request_pagination_if_needed(
                viewport_y,
                now_ms,
                self.pagination_top_threshold_px,
                self.pagination_cooldown_ms,
                self.paginate_limit,
            )
        };

        if let Some((room_id, limit)) = maybe_request {
            debug!(%room_id, limit, viewport_y, "queueing pagination request");
            self.enqueue_command(paginate_back_command(room_id, limit));
        }
    }

    /// Request and display current identity-backup status.
    pub fn request_recovery_status(&self) {
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while requesting recovery status");
            state.show_security_status_dialog("Checking backup status...");
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        self.enqueue_command(get_recovery_status_command());
    }

    /// Enable identity backup and display generated recovery key.
    pub fn backup_identity(&self) {
        let client_txn_id = format!(
            "desktop-recovery-enable-{}",
            self.next_txn_id.fetch_add(1, Ordering::Relaxed)
        );
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while enabling identity backup");
            state.show_security_creating_dialog("Creating backup and generating recovery key...");
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        self.enqueue_command(enable_recovery_command(client_txn_id));
    }

    /// Reset existing identity backup and generate a new recovery key.
    pub fn reset_identity_backup(&self) {
        let client_txn_id = format!(
            "desktop-recovery-reset-{}",
            self.next_txn_id.fetch_add(1, Ordering::Relaxed)
        );
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while resetting identity backup");
            state.show_security_creating_dialog(
                "Resetting identity backup and generating a new recovery key...",
            );
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        self.enqueue_command(reset_recovery_command(client_txn_id));
    }

    /// Show prompt to restore identity secrets using recovery key/passphrase.
    pub fn prompt_restore_identity(&self) {
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while prompting identity restore");
            state.show_security_restore_prompt_dialog(
                "Identity Restore",
                "Paste your recovery key or passphrase below, then click Restore.",
            );
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
    }

    /// Restore identity secrets using recovery key/passphrase.
    pub fn restore_identity(&self, recovery_key: String) {
        let recovery_key = recovery_key.trim().to_owned();
        if recovery_key.is_empty() {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while validating identity restore");
            state.set_error_text("Enter a recovery key before restoring identity.");
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
            return;
        }

        let client_txn_id = format!(
            "desktop-recovery-restore-{}",
            self.next_txn_id.fetch_add(1, Ordering::Relaxed)
        );
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while restoring identity");
            if !state.begin_restore_request() {
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        self.enqueue_command(recover_secrets_command(client_txn_id, recovery_key));
    }

    /// Dismiss the in-app security dialog.
    pub fn dismiss_security_dialog(&self) {
        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while dismissing security dialog");
        state.dismiss_security_dialog();
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
    }

    /// Copy the currently displayed recovery key to the system clipboard.
    pub fn copy_recovery_key(&self) {
        let Some(recovery_key) = self
            .state
            .lock()
            .expect("desktop state lock poisoned while reading recovery key")
            .recovery_key_for_copy()
        else {
            warn!("copy recovery key requested without recovery key in state");
            return;
        };

        match self.copy_to_clipboard(&recovery_key) {
            Ok(()) => {
                info!("recovery key copied to clipboard");
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while marking copied state");
                state.mark_recovery_key_copied();
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
            }
            Err(err) => {
                error!(error = %err, "failed to copy recovery key to clipboard");
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while handling copy failure");
                state.set_error_text("Failed to copy recovery key to clipboard.");
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
            }
        }
    }

    fn enqueue_command(&self, command: BackendCommand) {
        trace!(command = command_kind(&command), "enqueue_command");
        if self.command_tx.send(command).is_err() {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while enqueueing command");
            state.set_error_text("Backend command channel closed.");
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
            error!("backend command channel closed");
        }
    }

    fn copy_to_clipboard(&self, text: &str) -> Result<(), String> {
        let mut clipboard_slot = self
            .clipboard
            .lock()
            .expect("clipboard lock poisoned while copying recovery key");

        if clipboard_slot.is_none() {
            *clipboard_slot = Some(Clipboard::new().map_err(|err| err.to_string())?);
        }

        let write_result = clipboard_slot
            .as_mut()
            .expect("clipboard slot must be initialized")
            .set_text(text.to_owned());

        if write_result.is_ok() {
            return Ok(());
        }

        // Retry once with a fresh clipboard handle in case the previous backend became stale.
        *clipboard_slot = Some(Clipboard::new().map_err(|err| err.to_string())?);
        clipboard_slot
            .as_mut()
            .expect("clipboard slot must be initialized")
            .set_text(text.to_owned())
            .map_err(|err| err.to_string())
    }

    fn publish_snapshot(&self) {
        let snapshot = self
            .state
            .lock()
            .expect("desktop state lock poisoned while publishing snapshot")
            .snapshot();
        trace!(
            rooms = snapshot.rooms.len(),
            messages = snapshot.messages.len(),
            selected = snapshot.selected_room_id.as_deref().unwrap_or(""),
            "publishing initial snapshot"
        );
        (self.ui_update)(snapshot);
    }

    fn request_invite_action(&self, room_id: String, action: InviteAction) {
        let room_id = room_id.trim().to_owned();
        if room_id.is_empty() {
            return;
        }

        let command = {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while requesting invite action");
            if !state.is_invite_room(&room_id) {
                state.set_error_text("Invite is no longer pending for this room.");
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
            if state.has_pending_invite_action(&room_id) {
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }

            let client_txn_id = format!(
                "desktop-invite-{}-{}",
                match action {
                    InviteAction::Accept => "accept",
                    InviteAction::Reject => "reject",
                },
                self.next_txn_id.fetch_add(1, Ordering::Relaxed)
            );
            if !state.mark_invite_action_requested(room_id.clone(), client_txn_id.clone(), action) {
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);

            match action {
                InviteAction::Accept => accept_room_invite_command(room_id, client_txn_id),
                InviteAction::Reject => reject_room_invite_command(room_id, client_txn_id),
            }
        };
        self.enqueue_command(command);
    }
}

impl Drop for DesktopBridge {
    fn drop(&mut self) {
        info!("shutting down desktop bridge tasks");
        self.command_task.abort();
        self.event_task.abort();
    }
}

/// Build startup command sequence for initialization and session restore.
pub fn startup_command_sequence(config: &DesktopConfig) -> Vec<BackendCommand> {
    vec![
        BackendCommand::Init {
            homeserver: config.homeserver.clone(),
            data_dir: config.data_dir.clone(),
            config: config.init_config.clone(),
        },
        BackendCommand::RestoreSession,
    ]
}

fn login_password_command(config: &DesktopConfig) -> BackendCommand {
    BackendCommand::LoginPassword {
        user_id_or_localpart: config.user_id.clone(),
        password: config.password.clone(),
    }
}

fn post_auth_command_sequence() -> [BackendCommand; 2] {
    [BackendCommand::StartSync, BackendCommand::ListRooms]
}

fn room_open_command(room_id: String) -> BackendCommand {
    BackendCommand::OpenRoom { room_id }
}

fn accept_room_invite_command(room_id: String, client_txn_id: String) -> BackendCommand {
    BackendCommand::AcceptRoomInvite {
        room_id,
        client_txn_id,
    }
}

fn reject_room_invite_command(room_id: String, client_txn_id: String) -> BackendCommand {
    BackendCommand::RejectRoomInvite {
        room_id,
        client_txn_id,
    }
}

fn send_message_command(room_id: String, client_txn_id: String, body: String) -> BackendCommand {
    BackendCommand::SendMessage {
        room_id,
        client_txn_id,
        body,
        msgtype: MessageType::Text,
    }
}

fn paginate_back_command(room_id: String, limit: u16) -> BackendCommand {
    BackendCommand::PaginateBack {
        room_id,
        limit: limit.max(1),
    }
}

fn get_recovery_status_command() -> BackendCommand {
    BackendCommand::GetRecoveryStatus
}

fn enable_recovery_command(client_txn_id: String) -> BackendCommand {
    BackendCommand::EnableRecovery {
        client_txn_id,
        passphrase: None,
        wait_for_backups_to_upload: true,
    }
}

fn reset_recovery_command(client_txn_id: String) -> BackendCommand {
    BackendCommand::ResetRecovery {
        client_txn_id,
        passphrase: None,
        wait_for_backups_to_upload: true,
    }
}

fn recover_secrets_command(client_txn_id: String, recovery_key: String) -> BackendCommand {
    BackendCommand::RecoverSecrets {
        client_txn_id,
        recovery_key,
    }
}

async fn recv_event(events: &mut broadcast::Receiver<BackendEvent>) -> Result<BackendEvent, ()> {
    loop {
        match events.recv().await {
            Ok(event) => return Ok(event),
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return Err(()),
        }
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn command_kind(command: &BackendCommand) -> &'static str {
    match command {
        BackendCommand::Init { .. } => "Init",
        BackendCommand::LoginPassword { .. } => "LoginPassword",
        BackendCommand::RestoreSession => "RestoreSession",
        BackendCommand::StartSync => "StartSync",
        BackendCommand::StopSync => "StopSync",
        BackendCommand::ListRooms => "ListRooms",
        BackendCommand::AcceptRoomInvite { .. } => "AcceptRoomInvite",
        BackendCommand::RejectRoomInvite { .. } => "RejectRoomInvite",
        BackendCommand::OpenRoom { .. } => "OpenRoom",
        BackendCommand::PaginateBack { .. } => "PaginateBack",
        BackendCommand::SendDmText { .. } => "SendDmText",
        BackendCommand::SendMessage { .. } => "SendMessage",
        BackendCommand::EditMessage { .. } => "EditMessage",
        BackendCommand::RedactMessage { .. } => "RedactMessage",
        BackendCommand::UploadMedia { .. } => "UploadMedia",
        BackendCommand::DownloadMedia { .. } => "DownloadMedia",
        BackendCommand::GetRecoveryStatus => "GetRecoveryStatus",
        BackendCommand::EnableRecovery { .. } => "EnableRecovery",
        BackendCommand::ResetRecovery { .. } => "ResetRecovery",
        BackendCommand::RecoverSecrets { .. } => "RecoverSecrets",
        BackendCommand::Logout => "Logout",
    }
}

fn event_kind(event: &BackendEvent) -> &'static str {
    match event {
        BackendEvent::StateChanged { .. } => "StateChanged",
        BackendEvent::AuthResult { .. } => "AuthResult",
        BackendEvent::SyncStatus(_) => "SyncStatus",
        BackendEvent::RoomListUpdated { .. } => "RoomListUpdated",
        BackendEvent::RoomTimelineDelta { .. } => "RoomTimelineDelta",
        BackendEvent::RoomTimelineSnapshot { .. } => "RoomTimelineSnapshot",
        BackendEvent::SendAck(_) => "SendAck",
        BackendEvent::InviteActionAck(_) => "InviteActionAck",
        BackendEvent::MediaUploadAck(_) => "MediaUploadAck",
        BackendEvent::MediaDownloadAck(_) => "MediaDownloadAck",
        BackendEvent::RecoveryStatus(_) => "RecoveryStatus",
        BackendEvent::RecoveryEnableAck(_) => "RecoveryEnableAck",
        BackendEvent::RecoveryRestoreAck(_) => "RecoveryRestoreAck",
        BackendEvent::CryptoStatus(_) => "CryptoStatus",
        BackendEvent::FatalError { .. } => "FatalError",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use backend_core::BackendInitConfig;

    use super::*;

    fn sample_config() -> DesktopConfig {
        DesktopConfig {
            homeserver: "https://matrix.example.org".to_owned(),
            user_id: "@alice:example.org".to_owned(),
            password: "secret".to_owned(),
            data_dir: PathBuf::from("/tmp/pika"),
            init_config: Some(BackendInitConfig {
                sync_request_timeout_ms: Some(10_000),
                default_open_room_limit: Some(40),
                pagination_limit_cap: Some(80),
            }),
            timeline_max_items: 500,
            paginate_limit: 30,
            pagination_top_threshold_px: 80.0,
            pagination_cooldown_ms: 500,
        }
    }

    #[test]
    fn startup_sequence_is_ordered() {
        let sequence = startup_command_sequence(&sample_config());
        assert_eq!(sequence.len(), 2);
        assert!(matches!(sequence[0], BackendCommand::Init { .. }));
        assert!(matches!(sequence[1], BackendCommand::RestoreSession));
    }

    #[test]
    fn post_auth_sequence_starts_sync_then_lists_rooms() {
        let sequence = post_auth_command_sequence();
        assert!(matches!(sequence[0], BackendCommand::StartSync));
        assert!(matches!(sequence[1], BackendCommand::ListRooms));
    }

    #[test]
    fn login_password_command_uses_config_credentials() {
        let command = login_password_command(&sample_config());
        assert!(matches!(
            command,
            BackendCommand::LoginPassword {
                user_id_or_localpart,
                password,
            } if user_id_or_localpart == "@alice:example.org" && password == "secret"
        ));
    }

    #[test]
    fn command_builders_use_expected_payloads() {
        let open = room_open_command("!room:example.org".to_owned());
        assert!(matches!(
            open,
            BackendCommand::OpenRoom { room_id } if room_id == "!room:example.org"
        ));

        let accept =
            accept_room_invite_command("!room:example.org".to_owned(), "txn-accept".to_owned());
        assert!(matches!(
            accept,
            BackendCommand::AcceptRoomInvite {
                room_id,
                client_txn_id,
            } if room_id == "!room:example.org" && client_txn_id == "txn-accept"
        ));

        let reject =
            reject_room_invite_command("!room:example.org".to_owned(), "txn-reject".to_owned());
        assert!(matches!(
            reject,
            BackendCommand::RejectRoomInvite {
                room_id,
                client_txn_id,
            } if room_id == "!room:example.org" && client_txn_id == "txn-reject"
        ));

        let send = send_message_command(
            "!room:example.org".to_owned(),
            "txn-1".to_owned(),
            "hello".to_owned(),
        );
        assert!(matches!(
            send,
            BackendCommand::SendMessage {
                room_id,
                client_txn_id,
                body,
                msgtype: MessageType::Text,
            } if room_id == "!room:example.org" && client_txn_id == "txn-1" && body == "hello"
        ));

        let paginate = paginate_back_command("!room:example.org".to_owned(), 0);
        assert!(matches!(
            paginate,
            BackendCommand::PaginateBack { room_id, limit } if room_id == "!room:example.org" && limit == 1
        ));

        let recovery_status = get_recovery_status_command();
        assert!(matches!(recovery_status, BackendCommand::GetRecoveryStatus));

        let enable_recovery = enable_recovery_command("txn-r".to_owned());
        assert!(matches!(
            enable_recovery,
            BackendCommand::EnableRecovery {
                client_txn_id,
                passphrase: None,
                wait_for_backups_to_upload: true
            } if client_txn_id == "txn-r"
        ));

        let reset_recovery = reset_recovery_command("txn-r2".to_owned());
        assert!(matches!(
            reset_recovery,
            BackendCommand::ResetRecovery {
                client_txn_id,
                passphrase: None,
                wait_for_backups_to_upload: true
            } if client_txn_id == "txn-r2"
        ));

        let recover = recover_secrets_command("txn-restore".to_owned(), "key words".to_owned());
        assert!(matches!(
            recover,
            BackendCommand::RecoverSecrets {
                client_txn_id,
                recovery_key,
            } if client_txn_id == "txn-restore" && recovery_key == "key words"
        ));
    }
}
