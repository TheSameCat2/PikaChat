//! Backend bridge that wires Matrix runtime events into UI state snapshots.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use arboard::Clipboard;
use backend_core::{
    BackendCommand, BackendEvent, BackendLifecycleState, MediaDownloadAck, MediaSourceRef,
    MediaUploadAck, MessageType, OutgoingMedia, SendAck,
};
use backend_core::types::InviteAction;
use backend_matrix::MatrixFrontendAdapter;
use rfd::FileDialog;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, trace, warn};
use url::Url;

use crate::{
    auth_profile::{AuthProfile, clear_auth_profile, load_auth_profile, save_auth_profile},
    config::DesktopConfig,
    media_cache::MediaCache,
    state::{DesktopSnapshot, DesktopState, PendingOutgoingMediaStatus},
};

/// Callback used to publish new UI snapshots.
pub type UiUpdateCallback = Arc<dyn Fn(DesktopSnapshot) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
struct AuthSessionIntent {
    homeserver: String,
    user_id: String,
    remember_password: bool,
    data_dir: PathBuf,
}

/// Bridges UI actions and backend events using the shared frontend adapter.
pub struct DesktopBridge {
    command_tx: mpsc::UnboundedSender<BackendCommand>,
    state: Arc<Mutex<DesktopState>>,
    clipboard: Mutex<Option<Clipboard>>,
    ui_update: UiUpdateCallback,
    next_txn_id: AtomicU64,
    media_cache: Arc<Mutex<MediaCache>>,
    media_downloads_in_flight: Arc<Mutex<std::collections::HashMap<String, String>>>,
    media_download_txn_to_source: Arc<Mutex<std::collections::HashMap<String, String>>>,
    media_uploads_in_flight:
        Arc<Mutex<std::collections::HashMap<String, PendingOutgoingUploadContext>>>,
    media_send_txn_to_local_id: Arc<Mutex<std::collections::HashMap<String, String>>>,
    paginate_limit: u16,
    pagination_top_threshold_px: f32,
    pagination_cooldown_ms: u64,
    config: DesktopConfig,
    pending_auth_intent: Arc<Mutex<Option<AuthSessionIntent>>>,
    command_task: tokio::task::JoinHandle<()>,
    event_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone)]
struct PendingOutgoingUploadContext {
    room_id: String,
    local_id: String,
    caption: String,
    local_path: String,
    content_type: String,
}

enum SendPlan {
    Text {
        room_id: String,
        client_txn_id: String,
        body: String,
    },
    Attachment {
        room_id: String,
        local_id: String,
        caption: String,
        local_path: String,
        content_type: String,
    },
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
            timeline_max_items = config.timeline_max_items,
            paginate_limit = config.paginate_limit,
            "spawning desktop bridge"
        );
        let login_profile_path = config.auth_profile_path();
        let saved_profile = match load_auth_profile(&login_profile_path) {
            Ok(profile) => profile,
            Err(err) => {
                warn!(error = %err, "failed loading auth profile; ignoring persisted profile");
                None
            }
        };

        let prefill_homeserver_raw = saved_profile
            .as_ref()
            .map(|profile| profile.homeserver.clone())
            .or_else(|| config.prefill_homeserver.clone())
            .unwrap_or_default();
        let prefill_homeserver = display_homeserver_host(&prefill_homeserver_raw);
        let prefill_user_id = saved_profile
            .as_ref()
            .map(|profile| profile.user_id.clone())
            .or_else(|| config.prefill_user_id.clone())
            .unwrap_or_default();
        let prefill_password = config.prefill_password.clone().unwrap_or_default();
        let remember_password = saved_profile
            .as_ref()
            .map(|profile| profile.remember_password)
            .unwrap_or(false);

        let startup_restore_intent = saved_profile.as_ref().and_then(|profile| {
            if !profile.remember_password || profile.user_id.trim().is_empty() {
                return None;
            }

            let homeserver = match normalize_homeserver(profile.homeserver.clone()) {
                Ok(value) => value,
                Err(err) => {
                    warn!(
                        homeserver = %profile.homeserver,
                        error = %err,
                        "skipping persisted session restore due invalid homeserver"
                    );
                    return None;
                }
            };

            Some(AuthSessionIntent {
                homeserver: homeserver.clone(),
                user_id: profile.user_id.clone(),
                remember_password: true,
                data_dir: config.data_dir_for_account(&homeserver, &profile.user_id),
            })
        });

        let mut initial_state =
            DesktopState::new(prefill_user_id.clone(), config.timeline_max_items);
        initial_state.set_login_form(
            prefill_homeserver,
            prefill_user_id,
            prefill_password,
            remember_password,
        );
        if startup_restore_intent.is_some() {
            initial_state.begin_restore_attempt();
        } else {
            initial_state.show_login_screen();
        }
        let state = Arc::new(Mutex::new(initial_state));
        let pending_auth_intent = Arc::new(Mutex::new(startup_restore_intent.clone()));
        let media_cache_root = login_profile_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let media_cache = Arc::new(Mutex::new(
            create_media_cache(&media_cache_root).expect("failed to initialize media cache"),
        ));
        let media_downloads_in_flight = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let media_download_txn_to_source = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let media_uploads_in_flight = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let media_send_txn_to_local_id = Arc::new(Mutex::new(std::collections::HashMap::new()));

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
        let media_cache_for_events = Arc::clone(&media_cache);
        let media_downloads_in_flight_for_events = Arc::clone(&media_downloads_in_flight);
        let media_download_txn_to_source_for_events = Arc::clone(&media_download_txn_to_source);
        let media_uploads_in_flight_for_events = Arc::clone(&media_uploads_in_flight);
        let media_send_txn_to_local_id_for_events = Arc::clone(&media_send_txn_to_local_id);
        let ui_update_for_events = Arc::clone(&ui_update);
        let command_tx_for_events = command_tx.clone();
        let pending_auth_intent_for_events = Arc::clone(&pending_auth_intent);
        let login_profile_path_for_events = login_profile_path.clone();
        let config_for_events = config.clone();
        let mut events = adapter.subscribe();
        let event_task = runtime_handle.spawn(async move {
            debug!("desktop event worker started");
            loop {
                let event = match recv_event(&mut events).await {
                    Ok(event) => event,
                    Err(()) => break,
                };
                debug!(event = event_kind(&event), "received backend event");
                if let BackendEvent::AuthResult { success, .. } = &event {
                    if *success {
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
                        let maybe_intent = pending_auth_intent_for_events
                            .lock()
                            .expect("pending auth intent lock poisoned")
                            .take();
                        if let Some(intent) = maybe_intent {
                            if intent.remember_password {
                                let profile = AuthProfile {
                                    homeserver: intent.homeserver,
                                    user_id: intent.user_id,
                                    remember_password: true,
                                };
                                if let Err(err) = save_auth_profile(&login_profile_path_for_events, &profile)
                                {
                                    warn!(error = %err, "failed persisting auth profile after successful login");
                                }
                            } else if let Err(err) = clear_auth_profile(&login_profile_path_for_events) {
                                warn!(error = %err, "failed clearing auth profile after non-remembered login");
                            }
                        }
                    } else {
                        let _ = pending_auth_intent_for_events
                            .lock()
                            .expect("pending auth intent lock poisoned")
                            .take();
                    }
                }

                if let BackendEvent::StateChanged {
                    state: BackendLifecycleState::LoggedOut,
                } = &event
                {
                    if let Err(err) = clear_auth_profile(&login_profile_path_for_events) {
                        warn!(error = %err, "failed clearing auth profile on logout");
                    }
                    for target in config_for_events.logout_wipe_targets() {
                        if is_dangerous_wipe_target(&target) {
                            warn!(path = %target.display(), "skipping dangerous logout wipe target");
                            continue;
                        }
                        if let Err(err) = wipe_path_recursive(&target) {
                            warn!(path = %target.display(), error = %err, "failed wiping logout target");
                        }
                    }
                    let _ = pending_auth_intent_for_events
                        .lock()
                        .expect("pending auth intent lock poisoned")
                        .take();
                }

                let accepted_invite_room_id = match &event {
                    BackendEvent::InviteActionAck(ack)
                        if ack.error_code.is_none() && ack.action == InviteAction::Accept =>
                    {
                        Some(ack.room_id.clone())
                    }
                    _ => None,
                };

                match &event {
                    BackendEvent::MediaDownloadAck(ack) => {
                        handle_media_download_ack(
                            &state_for_events,
                            &media_cache_for_events,
                            &media_downloads_in_flight_for_events,
                            &media_download_txn_to_source_for_events,
                            ack,
                        );
                    }
                    BackendEvent::MediaUploadAck(ack) => {
                        handle_media_upload_ack(
                            &command_tx_for_events,
                            &state_for_events,
                            &media_uploads_in_flight_for_events,
                            &media_send_txn_to_local_id_for_events,
                            ack,
                        );
                    }
                    BackendEvent::SendAck(ack) => {
                        handle_media_send_ack(
                            &state_for_events,
                            &media_send_txn_to_local_id_for_events,
                            ack,
                        );
                    }
                    _ => {}
                }

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

                trigger_lazy_media_downloads(
                    &state_for_events,
                    &command_tx_for_events,
                    &media_cache_for_events,
                    &media_downloads_in_flight_for_events,
                    &media_download_txn_to_source_for_events,
                );
            }
            warn!("desktop event worker exiting: backend event stream closed");
        });

        let bridge = Arc::new(Self {
            command_tx,
            state,
            clipboard: Mutex::new(None),
            ui_update,
            next_txn_id: AtomicU64::new(1),
            media_cache,
            media_downloads_in_flight,
            media_download_txn_to_source,
            media_uploads_in_flight,
            media_send_txn_to_local_id,
            paginate_limit: config.paginate_limit.max(1),
            pagination_top_threshold_px: config.pagination_top_threshold_px,
            pagination_cooldown_ms: config.pagination_cooldown_ms,
            config: config.clone(),
            pending_auth_intent,
            command_task,
            event_task,
        });

        bridge.publish_snapshot();
        if let Some(intent) = startup_restore_intent {
            for command in startup_restore_command_sequence(&config, &intent) {
                debug!(
                    command = command_kind(&command),
                    "enqueue startup restore command"
                );
                bridge.enqueue_command(command);
            }
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

    /// Submit login request from UI login pane.
    pub fn submit_login(
        &self,
        homeserver: String,
        user_id: String,
        password: String,
        remember_password: bool,
    ) {
        let user_id = user_id.trim().to_owned();
        let password = password.to_owned();
        let homeserver = match normalize_homeserver(homeserver) {
            Ok(value) => value,
            Err(err) => {
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while validating homeserver");
                state.set_error_text(err);
                state.show_login_screen();
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
        };

        if user_id.is_empty() || password.is_empty() {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while validating login");
            state.set_error_text("User ID and password are required.");
            state.show_login_screen();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
            return;
        }

        let intent = AuthSessionIntent {
            data_dir: self.config.data_dir_for_account(&homeserver, &user_id),
            homeserver: homeserver.clone(),
            user_id: user_id.clone(),
            remember_password,
        };
        *self
            .pending_auth_intent
            .lock()
            .expect("pending auth intent lock poisoned") = Some(intent.clone());

        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while starting manual login");
            state.begin_manual_login(
                display_homeserver_host(&homeserver),
                user_id.clone(),
                password.clone(),
                remember_password,
            );
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }

        for command in login_command_sequence(&self.config, &intent, password) {
            debug!(command = command_kind(&command), "enqueue login command");
            self.enqueue_command(command);
        }
    }

    /// Request logout confirmation from UI.
    pub fn request_logout_confirmation(&self) {
        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while requesting logout confirmation");
        if state.snapshot().show_login_screen {
            return;
        }
        state.set_logout_confirm_visible(true);
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
    }

    /// Cancel logout confirmation dialog.
    pub fn cancel_logout_confirmation(&self) {
        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while cancelling logout confirmation");
        state.set_logout_confirm_visible(false);
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
    }

    /// Confirm logout and enqueue backend logout command.
    pub fn confirm_logout(&self) {
        let should_logout = {
            let state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while checking logout eligibility");
            !state.snapshot().show_login_screen
        };
        if !should_logout {
            return;
        }

        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while confirming logout");
            state.set_logout_confirm_visible(false);
            state.clear_error();
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        self.enqueue_command(BackendCommand::Logout);
    }

    /// Attempt to send message text to the currently selected room.
    ///
    /// Returns `true` when a send command was queued.
    pub fn send_message(&self, body: String) -> bool {
        let body = body.trim().to_owned();

        let send_plan = {
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

            if let Some(attachment) = state.take_pending_attachment() {
                let local_id = format!(
                    "desktop-media-local-{}",
                    self.next_txn_id.fetch_add(1, Ordering::Relaxed)
                );
                let caption = if body.is_empty() {
                    attachment.file_name.clone()
                } else {
                    body.clone()
                };
                state.upsert_pending_outgoing_media(
                    local_id.clone(),
                    room_id.clone(),
                    caption.clone(),
                    attachment.local_path.clone(),
                    PendingOutgoingMediaStatus::Uploading,
                );
                state.clear_error();
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                SendPlan::Attachment {
                    room_id,
                    local_id,
                    caption,
                    local_path: attachment.local_path,
                    content_type: attachment.content_type,
                }
            } else {
                if body.is_empty() {
                    debug!("ignoring empty send request");
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

                SendPlan::Text {
                    room_id,
                    client_txn_id,
                    body: body.clone(),
                }
            }
        };

        match send_plan {
            SendPlan::Text {
                room_id,
                client_txn_id,
                body,
            } => {
                info!(
                    room_id = %room_id,
                    client_txn_id = %client_txn_id,
                    body_len = body.len(),
                    "queueing room send"
                );
                self.enqueue_command(send_message_command(room_id, client_txn_id, body));
                true
            }
            SendPlan::Attachment {
                room_id,
                local_id,
                caption,
                local_path,
                content_type,
            } => {
                let data = match fs::read(&local_path) {
                    Ok(bytes) => bytes,
                    Err(err) => {
                        error!(path = %local_path, error = %err, "failed to read attachment");
                        let mut state = self.state.lock().expect(
                            "desktop state lock poisoned while handling attachment read failure",
                        );
                        state.set_pending_outgoing_upload_failed(&local_id);
                        state.set_error_text("Failed to read attachment file.");
                        let snapshot = state.snapshot();
                        (self.ui_update)(snapshot);
                        return false;
                    }
                };

                let upload_txn_id = format!(
                    "desktop-upload-{}",
                    self.next_txn_id.fetch_add(1, Ordering::Relaxed)
                );
                self.media_uploads_in_flight
                    .lock()
                    .expect("media uploads lock poisoned")
                    .insert(
                        upload_txn_id.clone(),
                        PendingOutgoingUploadContext {
                            room_id,
                            local_id,
                            caption,
                            local_path,
                            content_type: content_type.clone(),
                        },
                    );
                self.enqueue_command(BackendCommand::UploadMedia {
                    client_txn_id: upload_txn_id,
                    content_type,
                    data,
                });
                true
            }
        }
    }

    /// Open a file picker and stage one image attachment for the composer.
    pub fn pick_attachment(&self) {
        let Some(path) = FileDialog::new()
            .add_filter("Images", &["jpg", "jpeg", "png", "gif"])
            .pick_file()
        else {
            return;
        };

        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("attachment")
            .to_owned();
        let local_path = path.to_string_lossy().to_string();
        let metadata = match fs::metadata(&path) {
            Ok(value) => value,
            Err(err) => {
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while reading attachment metadata");
                state.set_error_text(format!("Failed to read attachment metadata: {err}"));
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
        };
        let content_type = content_type_for_path(&path);

        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while setting attachment");
        match DesktopState::validate_attachment(local_path, file_name, content_type, metadata.len())
        {
            Ok(attachment) => {
                state.set_pending_attachment(attachment);
                state.clear_error();
            }
            Err(err) => state.set_error_text(err),
        }
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
    }

    /// Clear the staged composer attachment.
    pub fn clear_attachment(&self) {
        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while clearing attachment");
        state.clear_pending_attachment();
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
    }

    /// Retry downloading one media source for timeline rendering.
    pub fn retry_media_download(&self, source: String) {
        if source.trim().is_empty() {
            return;
        }
        let source_ref = self
            .state
            .lock()
            .expect("desktop state lock poisoned while resolving media source for retry")
            .media_source_ref_for_key(&source)
            .unwrap_or_else(|| MediaSourceRef::PlainMxc {
                uri: source.clone(),
            });
        {
            let mut state = self
                .state
                .lock()
                .expect("desktop state lock poisoned while retrying media download");
            state.mark_media_download_started(source.clone());
            let snapshot = state.snapshot();
            (self.ui_update)(snapshot);
        }
        enqueue_media_download(
            &self.command_tx,
            source_ref,
            source,
            &self.media_downloads_in_flight,
            &self.media_download_txn_to_source,
        );
    }

    /// Retry a failed outgoing media item by local ID.
    pub fn retry_outgoing_media(&self, local_id: String) {
        let Some(pending) = self
            .state
            .lock()
            .expect("desktop state lock poisoned while loading pending outgoing media")
            .pending_outgoing_media(&local_id)
        else {
            return;
        };

        if let Some(source) = pending.media_source.clone() {
            let send_txn_id = format!(
                "desktop-send-media-{}",
                self.next_txn_id.fetch_add(1, Ordering::Relaxed)
            );
            self.media_send_txn_to_local_id
                .lock()
                .expect("media send txn map lock poisoned")
                .insert(send_txn_id.clone(), local_id.clone());
            {
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while retrying outgoing media send");
                state.set_pending_outgoing_sending(&local_id, source.clone());
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
            }
            self.enqueue_command(BackendCommand::SendMediaMessage {
                room_id: pending.room_id,
                client_txn_id: send_txn_id,
                media: OutgoingMedia::Image {
                    body: pending.caption,
                    source: MediaSourceRef::PlainMxc { uri: source },
                    metadata: None,
                },
            });
            return;
        }

        let data = match fs::read(&pending.local_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                error!(error = %err, path = %pending.local_path, "failed to read pending media");
                let mut state = self
                    .state
                    .lock()
                    .expect("desktop state lock poisoned while handling pending media read error");
                state.set_error_text("Failed to read pending media for retry.");
                let snapshot = state.snapshot();
                (self.ui_update)(snapshot);
                return;
            }
        };

        let upload_txn_id = format!(
            "desktop-upload-{}",
            self.next_txn_id.fetch_add(1, Ordering::Relaxed)
        );
        self.media_uploads_in_flight
            .lock()
            .expect("media uploads lock poisoned")
            .insert(
                upload_txn_id.clone(),
                PendingOutgoingUploadContext {
                    room_id: pending.room_id,
                    local_id,
                    caption: pending.caption,
                    local_path: pending.local_path.clone(),
                    content_type: content_type_for_path(Path::new(&pending.local_path)),
                },
            );
        self.enqueue_command(BackendCommand::UploadMedia {
            client_txn_id: upload_txn_id,
            content_type: content_type_for_path(Path::new(&pending.local_path)),
            data,
        });
    }

    /// Remove one pending outgoing media item from the timeline.
    pub fn remove_outgoing_media(&self, local_id: String) {
        let mut state = self
            .state
            .lock()
            .expect("desktop state lock poisoned while removing outgoing media");
        state.remove_pending_outgoing_media(&local_id);
        let snapshot = state.snapshot();
        (self.ui_update)(snapshot);
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
fn startup_restore_command_sequence(
    config: &DesktopConfig,
    intent: &AuthSessionIntent,
) -> Vec<BackendCommand> {
    vec![
        BackendCommand::Init {
            homeserver: intent.homeserver.clone(),
            data_dir: intent.data_dir.clone(),
            config: config.init_config.clone(),
        },
        BackendCommand::RestoreSession,
    ]
}

fn login_command_sequence(
    config: &DesktopConfig,
    intent: &AuthSessionIntent,
    password: String,
) -> [BackendCommand; 2] {
    [
        BackendCommand::Init {
            homeserver: intent.homeserver.clone(),
            data_dir: intent.data_dir.clone(),
            config: config.init_config.clone(),
        },
        BackendCommand::LoginPassword {
            user_id_or_localpart: intent.user_id.clone(),
            password,
            persist_session: intent.remember_password,
        },
    ]
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

fn normalize_homeserver(raw: String) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Homeserver is required.".to_owned());
    }

    let candidate = if let Some(rest) = raw.strip_prefix("https://") {
        format!("https://{}", rest.trim())
    } else if let Some(rest) = raw.strip_prefix("http://") {
        format!("https://{}", rest.trim())
    } else if raw.contains("://") {
        return Err("Only secure https homeservers are supported.".to_owned());
    } else {
        format!("https://{}", raw)
    };

    let parsed = Url::parse(&candidate).map_err(|err| format!("Invalid homeserver URL: {err}"))?;
    if parsed.scheme() != "https" {
        return Err("Only secure https homeservers are supported.".to_owned());
    }
    if parsed.host_str().is_none() {
        return Err("Homeserver must include a host, for example matrix.example.org.".to_owned());
    }

    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

fn display_homeserver_host(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if let Ok(parsed) = Url::parse(trimmed)
        && let Some(host) = parsed.host_str()
    {
        let mut rendered = host.to_owned();
        if let Some(port) = parsed.port() {
            rendered.push(':');
            rendered.push_str(&port.to_string());
        }
        return rendered;
    }

    let without_scheme = trimmed
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    without_scheme.trim_end_matches('/').to_owned()
}

fn wipe_path_recursive(path: &PathBuf) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!(
            "failed removing path {} during logout wipe: {err}",
            path.display()
        )),
    }
}

fn is_dangerous_wipe_target(path: &PathBuf) -> bool {
    let value = path.as_os_str().to_string_lossy();
    value.is_empty() || value == "/" || value == "." || value == ".."
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

fn next_auto_txn_id(prefix: &str) -> String {
    static NEXT_AUTO_TXN_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_AUTO_TXN_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{id}")
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
        BackendCommand::SendMediaMessage { .. } => "SendMediaMessage",
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

fn create_media_cache(data_dir: &Path) -> Result<MediaCache, String> {
    MediaCache::new(data_dir).map_err(|err| err.to_string())
}

fn enqueue_media_download(
    command_tx: &mpsc::UnboundedSender<BackendCommand>,
    source: MediaSourceRef,
    source_key: String,
    media_downloads_in_flight: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    media_download_txn_to_source: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let txn_id = next_auto_txn_id("desktop-media-download");
    media_downloads_in_flight
        .lock()
        .expect("media downloads lock poisoned")
        .insert(source_key.clone(), txn_id.clone());
    media_download_txn_to_source
        .lock()
        .expect("media download txn lock poisoned")
        .insert(txn_id.clone(), source_key);

    let _ = command_tx.send(BackendCommand::DownloadMedia {
        client_txn_id: txn_id,
        source,
    });
}

fn trigger_lazy_media_downloads(
    state: &Arc<Mutex<DesktopState>>,
    command_tx: &mpsc::UnboundedSender<BackendCommand>,
    media_cache: &Arc<Mutex<MediaCache>>,
    media_downloads_in_flight: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    media_download_txn_to_source: &Arc<Mutex<std::collections::HashMap<String, String>>>,
) {
    let sources = state
        .lock()
        .expect("desktop state lock poisoned while selecting lazy media")
        .selected_room_uncached_image_sources();

    for source in sources {
        let source_key = source.to_string();
        if media_downloads_in_flight
            .lock()
            .expect("media downloads lock poisoned")
            .contains_key(&source_key)
        {
            continue;
        }

        if let Some(path) = media_cache
            .lock()
            .expect("media cache lock poisoned")
            .get(&source_key)
        {
            let mut state = state
                .lock()
                .expect("desktop state lock poisoned while updating cached media");
            state.mark_media_download_ready(source_key.clone(), path.to_string_lossy().to_string());
            continue;
        }

        {
            let mut state = state
                .lock()
                .expect("desktop state lock poisoned while starting media download");
            state.mark_media_download_started(source_key.clone());
        }
        enqueue_media_download(
            command_tx,
            source,
            source_key,
            media_downloads_in_flight,
            media_download_txn_to_source,
        );
    }
}

fn handle_media_download_ack(
    state: &Arc<Mutex<DesktopState>>,
    media_cache: &Arc<Mutex<MediaCache>>,
    media_downloads_in_flight: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    media_download_txn_to_source: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    ack: &MediaDownloadAck,
) {
    let source = media_download_txn_to_source
        .lock()
        .expect("media download txn lock poisoned")
        .remove(&ack.client_txn_id)
        .unwrap_or_else(|| ack.source.to_string());
    media_downloads_in_flight
        .lock()
        .expect("media downloads lock poisoned")
        .remove(&source);

    if ack.error_code.is_some() || ack.data.is_none() {
        let mut state = state
            .lock()
            .expect("desktop state lock poisoned while handling failed media download");
        state.mark_media_download_failed(source);
        return;
    }

    let bytes = ack.data.as_ref().expect("checked is_some");
    let cached = media_cache
        .lock()
        .expect("media cache lock poisoned")
        .insert(&source, bytes, ack.content_type.as_deref());
    match cached {
        Ok(path) => {
            let mut state = state
                .lock()
                .expect("desktop state lock poisoned while marking media ready");
            state.mark_media_download_ready(source, path.to_string_lossy().to_string());
        }
        Err(err) => {
            error!(
                error = %err,
                source = %source,
                content_type = ?ack.content_type,
                bytes_len = bytes.len(),
                "failed to cache downloaded media"
            );
            let mut state = state
                .lock()
                .expect("desktop state lock poisoned while marking media cache failure");
            state.mark_media_download_failed(source);
        }
    }
}

fn handle_media_upload_ack(
    command_tx: &mpsc::UnboundedSender<BackendCommand>,
    state: &Arc<Mutex<DesktopState>>,
    media_uploads_in_flight: &Arc<
        Mutex<std::collections::HashMap<String, PendingOutgoingUploadContext>>,
    >,
    media_send_txn_to_local_id: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    ack: &MediaUploadAck,
) {
    let Some(context) = media_uploads_in_flight
        .lock()
        .expect("media uploads lock poisoned")
        .remove(&ack.client_txn_id)
    else {
        return;
    };

    if ack.error_code.is_some() {
        let mut state = state
            .lock()
            .expect("desktop state lock poisoned while handling upload failure");
        state.set_pending_outgoing_upload_failed(&context.local_id);
        state.set_error_text(format!(
            "media upload failed ({})",
            ack.error_code.as_deref().unwrap_or("unknown")
        ));
        return;
    }

    let Some(content_uri) = ack.content_uri.clone() else {
        let mut state = state
            .lock()
            .expect("desktop state lock poisoned while handling upload failure");
        state.set_pending_outgoing_upload_failed(&context.local_id);
        state.set_error_text("media upload failed (missing content uri)");
        return;
    };

    {
        let mut state = state
            .lock()
            .expect("desktop state lock poisoned while transitioning to sending");
        state.set_pending_outgoing_sending(&context.local_id, content_uri.clone());
    }

    let send_txn_id = next_auto_txn_id("desktop-send-media");
    media_send_txn_to_local_id
        .lock()
        .expect("media send txn map lock poisoned")
        .insert(send_txn_id.clone(), context.local_id.clone());

    let _ = command_tx.send(BackendCommand::SendMediaMessage {
        room_id: context.room_id,
        client_txn_id: send_txn_id,
        media: OutgoingMedia::Image {
            body: context.caption,
            source: MediaSourceRef::PlainMxc { uri: content_uri },
            metadata: None,
        },
    });
}

fn handle_media_send_ack(
    state: &Arc<Mutex<DesktopState>>,
    media_send_txn_to_local_id: &Arc<Mutex<std::collections::HashMap<String, String>>>,
    ack: &SendAck,
) {
    let Some(local_id) = media_send_txn_to_local_id
        .lock()
        .expect("media send txn map lock poisoned")
        .remove(&ack.client_txn_id)
    else {
        return;
    };

    let mut state = state
        .lock()
        .expect("desktop state lock poisoned while handling send ack");
    if ack.error_code.is_some() {
        state.set_pending_outgoing_send_failed(&local_id);
        state.set_error_text(format!(
            "media send failed ({})",
            ack.error_code.as_deref().unwrap_or("unknown")
        ));
    } else {
        state.remove_pending_outgoing_media(&local_id);
    }
}

fn content_type_for_path(path: &Path) -> String {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg".to_owned(),
        Some("png") => "image/png".to_owned(),
        Some("gif") => "image/gif".to_owned(),
        _ => "application/octet-stream".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use backend_core::BackendInitConfig;

    use super::*;

    fn sample_config() -> DesktopConfig {
        DesktopConfig {
            prefill_homeserver: Some("https://matrix.example.org".to_owned()),
            prefill_user_id: Some("@alice:example.org".to_owned()),
            prefill_password: Some("secret".to_owned()),
            data_dir_override: Some(PathBuf::from("/tmp/pika")),
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
    fn startup_restore_sequence_is_ordered() {
        let intent = AuthSessionIntent {
            homeserver: "https://matrix.example.org".to_owned(),
            user_id: "@alice:example.org".to_owned(),
            remember_password: true,
            data_dir: PathBuf::from("/tmp/pika"),
        };
        let sequence = startup_restore_command_sequence(&sample_config(), &intent);
        assert_eq!(sequence.len(), 2);
        assert!(matches!(
            &sequence[0],
            BackendCommand::Init {
                homeserver,
                data_dir,
                ..
            } if homeserver == "https://matrix.example.org" && data_dir == &PathBuf::from("/tmp/pika")
        ));
        assert!(matches!(sequence[1], BackendCommand::RestoreSession));
    }

    #[test]
    fn post_auth_sequence_starts_sync_then_lists_rooms() {
        let sequence = post_auth_command_sequence();
        assert!(matches!(sequence[0], BackendCommand::StartSync));
        assert!(matches!(sequence[1], BackendCommand::ListRooms));
    }

    #[test]
    fn login_command_sequence_uses_intent_and_remember_policy() {
        let config = sample_config();
        let intent = AuthSessionIntent {
            homeserver: "https://matrix.example.org".to_owned(),
            user_id: "@alice:example.org".to_owned(),
            remember_password: false,
            data_dir: PathBuf::from("/tmp/pika"),
        };
        let sequence = login_command_sequence(&config, &intent, "secret".to_owned());
        assert!(matches!(
            &sequence[0],
            BackendCommand::Init {
                homeserver,
                data_dir,
                ..
            } if homeserver == "https://matrix.example.org" && data_dir == &PathBuf::from("/tmp/pika")
        ));
        assert!(matches!(
            &sequence[1],
            BackendCommand::LoginPassword {
                user_id_or_localpart,
                password,
                persist_session: false,
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

    #[test]
    fn normalize_homeserver_accepts_host_and_upgrades_http() {
        assert_eq!(
            normalize_homeserver("matrix.example.org".to_owned()).expect("host should normalize"),
            "https://matrix.example.org"
        );
        assert_eq!(
            normalize_homeserver("http://matrix.example.org".to_owned())
                .expect("http should be upgraded"),
            "https://matrix.example.org"
        );
        assert_eq!(
            normalize_homeserver("https://matrix.example.org/".to_owned())
                .expect("https should normalize"),
            "https://matrix.example.org"
        );
    }

    #[test]
    fn normalize_homeserver_rejects_non_https_scheme() {
        let err = normalize_homeserver("ftp://matrix.example.org".to_owned())
            .expect_err("non-https scheme must be rejected");
        assert!(err.contains("https"));
    }

    #[test]
    fn display_homeserver_host_strips_scheme_for_ui() {
        assert_eq!(
            display_homeserver_host("https://matrix.example.org"),
            "matrix.example.org"
        );
        assert_eq!(
            display_homeserver_host("http://matrix.example.org:8448/"),
            "matrix.example.org:8448"
        );
    }
}
