//! Backend bridge that wires Matrix runtime events into UI state snapshots.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

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

                let snapshot = {
                    let mut state = state_for_events
                        .lock()
                        .expect("desktop state lock poisoned while handling backend event");
                    state.handle_backend_event(event);
                    state.snapshot()
                };
                (ui_update_for_events)(snapshot);
            }
            warn!("desktop event worker exiting: backend event stream closed");
        });

        let bridge = Arc::new(Self {
            command_tx,
            state,
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
        BackendCommand::OpenRoom { .. } => "OpenRoom",
        BackendCommand::PaginateBack { .. } => "PaginateBack",
        BackendCommand::SendDmText { .. } => "SendDmText",
        BackendCommand::SendMessage { .. } => "SendMessage",
        BackendCommand::EditMessage { .. } => "EditMessage",
        BackendCommand::RedactMessage { .. } => "RedactMessage",
        BackendCommand::UploadMedia { .. } => "UploadMedia",
        BackendCommand::DownloadMedia { .. } => "DownloadMedia",
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
        BackendEvent::MediaUploadAck(_) => "MediaUploadAck",
        BackendEvent::MediaDownloadAck(_) => "MediaDownloadAck",
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
    }
}
