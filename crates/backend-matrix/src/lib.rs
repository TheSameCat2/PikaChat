//! Matrix SDK-backed backend runtime and frontend adapter implementation.
//!
//! The primary frontend-facing entry points are:
//! - [`spawn_runtime`]
//! - [`MatrixRuntimeHandle`]
//! - [`MatrixFrontendAdapter`]

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use backend_core::types::{InviteAction, InviteActionAck, RoomMembership};
use backend_core::{
    BackendChannelError, BackendChannels, BackendCommand, BackendError, BackendErrorCategory,
    BackendEvent, BackendInitConfig, BackendLifecycleState, BackendStateMachine, EventStream,
    KeyBackupState, MediaDownloadAck, MediaUploadAck, MessageType, RecoveryEnableAck,
    RecoveryRestoreAck, RecoveryState as CoreRecoveryState, RecoveryStatus, RetryPolicy,
    RoomSummary, SendOutcome, SyncStatus, TimelineBuffer, TimelineItem, TimelineMergeError,
    TimelineOp, classify_http_status, normalize_send_outcome,
};
use backend_platform::{OsKeyringSecretStore, ScopedSecretStore, SecretStoreError};
use matrix_sdk::{
    Client, ClientBuildError, HttpError, RoomState,
    authentication::matrix::MatrixSession,
    config::RequestConfig,
    config::SyncSettings,
    deserialized_responses::TimelineEvent,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings,
        backups::BackupState as MatrixBackupState,
        recovery::{RecoveryError as MatrixRecoveryError, RecoveryState as MatrixRecoveryState},
    },
    media::{MediaFormat, MediaRequestParameters},
    room::{Messages, MessagesOptions, edit::EditedContent},
    ruma::{
        OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, UInt,
        api::client::{
            error::{ErrorKind, RetryAfter},
            session::login,
            uiaa::UserIdentifier,
        },
        events::room::{
            MediaSource,
            message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
        },
    },
    store::RoomLoadSettings,
};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
    time,
};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};
use uuid::Uuid;

const KEYRING_SERVICE: &str = "dev.thesamecat.pikachat";
const STORE_PASSPHRASE_KEY_PREFIX: &str = "store-passphrase";
const SESSION_KEY_PREFIX: &str = "matrix-session";
const DEFAULT_DEVICE_DISPLAY_NAME: &str = "PikaChat Desktop";
const DEFAULT_OPEN_ROOM_LIMIT: u16 = 30;
const DEFAULT_PAGINATION_LIMIT_CAP: u16 = 100;
const DEFAULT_LOGIN_TIMEOUT_SECS: u64 = 12;
const ROOM_MESSAGE_EVENT_TYPE: &str = "m.room.message";
const ROOM_REDACTION_EVENT_TYPE: &str = "m.room.redaction";
const ROOM_ENCRYPTED_EVENT_TYPE: &str = "m.room.encrypted";
const REL_TYPE_REPLACE: &str = "m.replace";
const MSGTYPE_TEXT: &str = "m.text";
const MSGTYPE_NOTICE: &str = "m.notice";
const MSGTYPE_EMOTE: &str = "m.emote";
const ENCRYPTED_TIMELINE_PLACEHOLDER: &str = "[Encrypted message: keys unavailable on this device]";
const TO_DEVICE_ROOM_KEY_EVENT_TYPE: &str = "m.room_key";
const TO_DEVICE_FORWARDED_ROOM_KEY_EVENT_TYPE: &str = "m.forwarded_room_key";
const DEFAULT_TIMELINE_MAX_ITEMS: usize = 500;
const STORE_PASSPHRASE_FILENAME: &str = ".pikachat-store-passphrase";
const DEVICE_ID_HINT_SUFFIX: &str = ".device-id-hint";
const LEGACY_DEVICE_ID_HINT_FILENAME: &str = ".pikachat-device-id";
const ROOM_KEY_REFRESH_ROOM_LIMIT: usize = 5;
const ROOM_KEY_REFRESH_EVENT_LIMIT: u16 = 30;

/// Configuration for constructing a [`MatrixBackend`] instance.
#[derive(Debug, Clone)]
pub struct MatrixBackendConfig {
    /// Homeserver base URL (for example `https://matrix.example.org`).
    pub homeserver: String,
    /// Local data directory for the Matrix SDK sqlite store.
    pub data_dir: PathBuf,
    /// Optional sqlite store passphrase.
    pub store_passphrase: Option<String>,
}

impl MatrixBackendConfig {
    /// Build a new backend config.
    pub fn new(
        homeserver: impl Into<String>,
        data_dir: impl Into<PathBuf>,
        store_passphrase: Option<String>,
    ) -> Self {
        Self {
            homeserver: homeserver.into(),
            data_dir: data_dir.into(),
            store_passphrase,
        }
    }
}

#[derive(Debug)]
struct RunningSyncTask {
    stop: CancellationToken,
    task: JoinHandle<()>,
}

/// Low-level Matrix SDK backend implementation.
///
/// Most frontend integrations should use [`spawn_runtime`] and
/// [`MatrixFrontendAdapter`] rather than calling this type directly.
#[derive(Debug)]
pub struct MatrixBackend {
    client: Client,
    sync_task: Mutex<Option<RunningSyncTask>>,
}

impl MatrixBackend {
    /// Construct a backend client and initialize the persistent store.
    pub async fn new(config: MatrixBackendConfig) -> Result<Self, BackendError> {
        let client = Client::builder()
            .homeserver_url(&config.homeserver)
            .with_encryption_settings(EncryptionSettings {
                auto_enable_cross_signing: false,
                backup_download_strategy: BackupDownloadStrategy::OneShot,
                auto_enable_backups: false,
            })
            .sqlite_store(&config.data_dir, config.store_passphrase.as_deref())
            .build()
            .await
            .map_err(map_client_build_error)?;

        Ok(Self {
            client,
            sync_task: Mutex::new(None),
        })
    }

    /// Access the underlying Matrix SDK client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Return the currently active Matrix session, if authenticated.
    pub fn session(&self) -> Option<MatrixSession> {
        self.client.matrix_auth().session()
    }

    /// Restore a previously serialized Matrix session.
    pub async fn restore_session(&self, session: MatrixSession) -> Result<(), BackendError> {
        self.client
            .restore_session(session)
            .await
            .map_err(map_matrix_error)
    }

    /// Logout the current account.
    pub async fn logout(&self) -> Result<(), BackendError> {
        self.client.logout().await.map_err(map_matrix_error)
    }

    /// Collect room summaries from the current client state.
    pub fn list_rooms(&self) -> Vec<RoomSummary> {
        collect_room_summaries(&self.client)
    }

    /// Login with password using the Matrix client API.
    pub async fn login_password(
        &self,
        user_id_or_localpart: &str,
        password: &str,
        device_display_name: &str,
        preferred_device_id: Option<&str>,
    ) -> Result<(), BackendError> {
        let login_info = login::v3::LoginInfo::Password(login::v3::Password::new(
            UserIdentifier::UserIdOrLocalpart(user_id_or_localpart.into()),
            password.to_owned(),
        ));
        let request = login::v3::Request::new(login_info);
        let mut request = request;
        request.initial_device_display_name = Some(device_display_name.to_owned());
        if let Some(device_id) = preferred_device_id {
            request.device_id = Some(device_id.into());
        }

        let response = self
            .client
            .send(request)
            .with_request_config(RequestConfig::new().disable_retry().skip_auth())
            .await
            .map_err(map_matrix_http_error)?;

        self.client
            .matrix_auth()
            .restore_session((&response).into(), RoomLoadSettings::default())
            .await
            .map_err(map_matrix_error)
    }

    /// Start continuous `/sync` polling and emit events to `event_tx`.
    pub async fn start_sync(
        &self,
        event_tx: broadcast::Sender<BackendEvent>,
        sync_request_timeout: Option<Duration>,
    ) -> Result<(), BackendError> {
        info!(
            timeout_ms = sync_request_timeout.map(|timeout| timeout.as_millis() as u64),
            "starting matrix sync task"
        );
        let mut guard = self.sync_task.lock().await;
        if guard.is_some() {
            return Err(BackendError::new(
                BackendErrorCategory::Internal,
                "sync_already_running",
                "sync task is already running",
            ));
        }

        let stop = CancellationToken::new();
        let stop_child = stop.child_token();
        let event_tx_clone = event_tx.clone();
        let client = self.client.clone();
        let task = tokio::spawn(async move {
            info!("matrix sync task started");
            let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                running: true,
                lag_hint_ms: None,
            }));

            let retry_policy = RetryPolicy::default();
            let mut attempt: u32 = 0;
            let mut room_event_bodies: HashMap<String, HashMap<String, String>> = HashMap::new();
            let mut sync_settings = match sync_request_timeout {
                Some(timeout) => SyncSettings::default().timeout(timeout),
                None => SyncSettings::default(),
            };

            loop {
                tokio::select! {
                    _ = stop_child.cancelled() => break,
                    sync_result = client.sync_once(sync_settings.clone()) => {
                        match sync_result {
                            Ok(sync_response) => {
                                attempt = 0;
                                let deltas =
                                    sync_timeline_deltas(&sync_response, &mut room_event_bodies);
                                trace!(room_delta_count = deltas.len(), "sync response processed");
                                for (room_id, ops) in deltas {
                                    debug!(
                                        room_id = %room_id,
                                        op_count = ops.len(),
                                        "emitting timeline delta from sync"
                                    );
                                    let _ = event_tx_clone.send(BackendEvent::RoomTimelineDelta {
                                        room_id,
                                        ops,
                                    });
                                }
                                let rooms = collect_room_summaries(&client);
                                debug!(room_count = rooms.len(), "emitting room list after sync");
                                let _ = event_tx_clone.send(BackendEvent::RoomListUpdated { rooms });

                                if sync_response_has_room_key_events(&sync_response) {
                                    let refresh_deltas = refresh_room_timeline_updates_after_room_keys(
                                        &client,
                                        &mut room_event_bodies,
                                    )
                                    .await;
                                    for (room_id, ops) in refresh_deltas {
                                        debug!(
                                            room_id = %room_id,
                                            op_count = ops.len(),
                                            "emitting timeline refresh delta after room-key to-device event"
                                        );
                                        let _ = event_tx_clone.send(BackendEvent::RoomTimelineDelta {
                                            room_id,
                                            ops,
                                        });
                                    }
                                }

                                sync_settings = sync_settings.token(sync_response.next_batch);

                                let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                                    running: true,
                                    lag_hint_ms: None,
                                }));
                            }
                            Err(err) => {
                                let mapped = map_matrix_error(err);
                                let recoverable = is_recoverable_sync_error(&mapped);
                                warn!(
                                    code = %mapped.code,
                                    recoverable,
                                    message = %mapped.message,
                                    "sync request failed"
                                );
                                let _ = event_tx_clone.send(BackendEvent::FatalError {
                                    code: mapped.code.clone(),
                                    message: mapped.message.clone(),
                                    recoverable,
                                });

                                if !recoverable {
                                    break;
                                }

                                let delay = retry_policy.delay_for_attempt(attempt, mapped.retry_after_ms);
                                attempt = attempt.saturating_add(1);
                                debug!(
                                    delay_ms = delay.as_millis() as u64,
                                    attempt,
                                    "sync retry scheduled"
                                );
                                let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                                    running: true,
                                    lag_hint_ms: Some(delay.as_millis() as u64),
                                }));

                                tokio::select! {
                                    _ = stop_child.cancelled() => break,
                                    _ = tokio::time::sleep(delay) => {}
                                }
                            }
                        }
                    }
                }
            }

            let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                running: false,
                lag_hint_ms: None,
            }));
            info!("matrix sync task stopped");
        });

        *guard = Some(RunningSyncTask { stop, task });
        Ok(())
    }

    /// Stop the currently running sync loop.
    pub async fn stop_sync(&self) -> Result<(), BackendError> {
        let running = {
            let mut guard = self.sync_task.lock().await;
            guard.take()
        };

        let Some(running) = running else {
            return Err(BackendError::new(
                BackendErrorCategory::Internal,
                "sync_not_running",
                "sync task is not running",
            ));
        };

        running.stop.cancel();
        let _ = running.task.await;
        Ok(())
    }

    /// Execute one `/sync` request.
    pub async fn sync_once(&self) -> Result<(), BackendError> {
        self.client
            .sync_once(SyncSettings::default())
            .await
            .map(|_| ())
            .map_err(map_matrix_error)
    }

    /// Send a room message and return created event ID.
    pub async fn send_message(
        &self,
        room_id: &str,
        body: &str,
        msgtype: MessageType,
    ) -> Result<String, BackendError> {
        let room = self.lookup_room(room_id)?;

        let content = match msgtype {
            MessageType::Text => RoomMessageEventContent::text_plain(body),
            MessageType::Notice => RoomMessageEventContent::notice_plain(body),
            MessageType::Emote => RoomMessageEventContent::emote_plain(body),
        };

        let response = room.send(content).await.map_err(map_matrix_error)?;
        Ok(response.event_id.to_string())
    }

    /// Send an edit for an existing room message.
    pub async fn edit_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        new_body: &str,
    ) -> Result<String, BackendError> {
        let room = self.lookup_room(room_id)?;
        let target_event_id = parse_event_id(target_event_id)?;

        let edit_event = room
            .make_edit_event(
                &target_event_id,
                EditedContent::RoomMessage(RoomMessageEventContentWithoutRelation::text_plain(
                    new_body,
                )),
            )
            .await
            .map_err(map_edit_error)?;

        let response = room.send(edit_event).await.map_err(map_matrix_error)?;
        Ok(response.event_id.to_string())
    }

    /// Redact an existing room event.
    pub async fn redact_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        reason: Option<String>,
    ) -> Result<(), BackendError> {
        let room = self.lookup_room(room_id)?;
        let target_event_id = parse_event_id(target_event_id)?;

        room.redact(&target_event_id, reason.as_deref(), None)
            .await
            .map_err(map_matrix_http_error)?;

        Ok(())
    }

    /// Fetch initial room history and return timeline operations plus next token.
    pub async fn open_room(
        &self,
        room_id: &str,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
        let room = self.lookup_room(room_id)?;
        let messages = room
            .messages(messages_options(None, limit)?)
            .await
            .map_err(map_matrix_error)?;
        let next_token = messages.end.clone();

        Ok((timeline_ops_for_open(messages), next_token))
    }

    /// Fetch older room history and return prepend operations plus next token.
    pub async fn paginate_back(
        &self,
        room_id: &str,
        from_token: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
        let room = self.lookup_room(room_id)?;
        let messages = room
            .messages(messages_options(from_token, limit)?)
            .await
            .map_err(map_matrix_error)?;
        let next_token = messages.end.clone();

        Ok((timeline_ops_for_pagination(messages), next_token))
    }

    /// Send a text DM to a user, creating a DM room if needed.
    pub async fn send_dm_text(&self, user_id: &str, body: &str) -> Result<String, BackendError> {
        let user_id = parse_user_id(user_id)?;

        let room = if let Some(existing_room) = self.client.get_dm_room(&user_id) {
            existing_room
        } else {
            self.client
                .create_dm(&user_id)
                .await
                .map_err(map_matrix_error)?
        };

        let response = room
            .send(RoomMessageEventContent::text_plain(body))
            .await
            .map_err(map_matrix_error)?;
        Ok(response.event_id.to_string())
    }

    /// Accept a pending room invite.
    pub async fn accept_room_invite(&self, room_id: &str) -> Result<(), BackendError> {
        let room = self.lookup_room(room_id)?;
        room.join().await.map_err(map_matrix_error)
    }

    /// Reject a pending room invite.
    pub async fn reject_room_invite(&self, room_id: &str) -> Result<(), BackendError> {
        let room = self.lookup_room(room_id)?;
        room.leave().await.map_err(map_matrix_error)
    }

    /// Upload raw media bytes and return the resulting `mxc://` URI string.
    pub async fn upload_media(
        &self,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<String, BackendError> {
        let content_type = parse_media_content_type(content_type)?;
        let response = self
            .client
            .media()
            .upload(&content_type, data, None)
            .await
            .map_err(map_matrix_error)?;
        Ok(response.content_uri.to_string())
    }

    /// Download media bytes from an `mxc://` source.
    pub async fn download_media(&self, source: &str) -> Result<Vec<u8>, BackendError> {
        let source = parse_mxc_uri(source)?;
        let request = MediaRequestParameters {
            source: MediaSource::Plain(source),
            format: MediaFormat::File,
        };
        self.client
            .media()
            .get_media_content(&request, true)
            .await
            .map_err(map_matrix_error)
    }

    /// Fetch current backup/recovery status from local state and homeserver.
    pub async fn get_recovery_status(&self) -> Result<RecoveryStatus, BackendError> {
        let backups = self.client.encryption().backups();
        let backup_state = map_backup_state(backups.state());
        let backup_enabled = backups.are_enabled().await;
        let backup_exists_on_server = backups
            .fetch_exists_on_server()
            .await
            .map_err(map_matrix_error)?;
        let recovery_state = map_recovery_state(self.client.encryption().recovery().state());

        Ok(RecoveryStatus {
            backup_state,
            backup_enabled,
            backup_exists_on_server,
            recovery_state,
        })
    }

    /// Enable recovery and backups, returning the generated recovery key.
    pub async fn enable_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError> {
        let existing = self.get_recovery_status().await?;
        if existing.backup_enabled
            && existing.backup_exists_on_server
            && matches!(
                existing.recovery_state,
                CoreRecoveryState::Enabled | CoreRecoveryState::Incomplete
            )
        {
            return Err(BackendError::new(
                BackendErrorCategory::Config,
                "recovery_already_enabled",
                "identity backup is already enabled; use status view to inspect current backup",
            ));
        }

        let recovery = self.client.encryption().recovery();
        let enable = recovery.enable();
        let enable = if wait_for_backups_to_upload {
            enable.wait_for_backups_to_upload()
        } else {
            enable
        };

        let recovery_key = if let Some(passphrase) = passphrase {
            enable.with_passphrase(passphrase).await
        } else {
            enable.await
        }
        .map_err(map_recovery_error)?;

        Ok(recovery_key)
    }

    /// Recover secrets from secret storage using a recovery key/passphrase.
    pub async fn recover_secrets(&self, recovery_key: &str) -> Result<(), BackendError> {
        self.client
            .encryption()
            .recovery()
            .recover(recovery_key)
            .await
            .map_err(map_recovery_error)
    }

    /// Reset backup/recovery setup and return a newly generated recovery key.
    pub async fn reset_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError> {
        self.client
            .encryption()
            .backups()
            .disable_and_delete()
            .await
            .map_err(map_matrix_error)?;

        let recovery = self.client.encryption().recovery();
        let enable = recovery.enable();
        let enable = if wait_for_backups_to_upload {
            enable.wait_for_backups_to_upload()
        } else {
            enable
        };

        let recovery_key = if let Some(passphrase) = passphrase {
            enable.with_passphrase(passphrase).await
        } else {
            enable.await
        }
        .map_err(map_recovery_error)?;

        Ok(recovery_key)
    }

    fn lookup_room(&self, room_id: &str) -> Result<matrix_sdk::Room, BackendError> {
        let room_id = parse_room_id(room_id)?;
        self.client.get_room(&room_id).ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Config,
                "room_not_found",
                format!("room not found: {room_id}"),
            )
        })
    }
}

#[async_trait]
trait RuntimeBackend: Send + Sync {
    fn session(&self) -> Option<MatrixSession>;
    async fn restore_session(&self, session: MatrixSession) -> Result<(), BackendError>;
    async fn logout(&self) -> Result<(), BackendError>;
    fn list_rooms(&self) -> Vec<RoomSummary>;
    async fn login_password(
        &self,
        user_id_or_localpart: &str,
        password: &str,
        device_display_name: &str,
        preferred_device_id: Option<&str>,
    ) -> Result<(), BackendError>;
    async fn start_sync(
        &self,
        event_tx: broadcast::Sender<BackendEvent>,
        sync_request_timeout: Option<Duration>,
    ) -> Result<(), BackendError>;
    async fn stop_sync(&self) -> Result<(), BackendError>;
    async fn send_message(
        &self,
        room_id: &str,
        body: &str,
        msgtype: MessageType,
    ) -> Result<String, BackendError>;
    async fn edit_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        new_body: &str,
    ) -> Result<String, BackendError>;
    async fn redact_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        reason: Option<String>,
    ) -> Result<(), BackendError>;
    async fn open_room(
        &self,
        room_id: &str,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError>;
    async fn paginate_back(
        &self,
        room_id: &str,
        from_token: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError>;
    async fn send_dm_text(&self, user_id: &str, body: &str) -> Result<String, BackendError>;
    async fn accept_room_invite(&self, room_id: &str) -> Result<(), BackendError>;
    async fn reject_room_invite(&self, room_id: &str) -> Result<(), BackendError>;
    async fn upload_media(&self, content_type: &str, data: Vec<u8>)
    -> Result<String, BackendError>;
    async fn download_media(&self, source: &str) -> Result<Vec<u8>, BackendError>;
    async fn get_recovery_status(&self) -> Result<RecoveryStatus, BackendError>;
    async fn enable_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError>;
    async fn reset_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError>;
    async fn recover_secrets(&self, recovery_key: &str) -> Result<(), BackendError>;
}

#[async_trait]
impl RuntimeBackend for MatrixBackend {
    fn session(&self) -> Option<MatrixSession> {
        MatrixBackend::session(self)
    }

    async fn restore_session(&self, session: MatrixSession) -> Result<(), BackendError> {
        MatrixBackend::restore_session(self, session).await
    }

    async fn logout(&self) -> Result<(), BackendError> {
        MatrixBackend::logout(self).await
    }

    fn list_rooms(&self) -> Vec<RoomSummary> {
        MatrixBackend::list_rooms(self)
    }

    async fn login_password(
        &self,
        user_id_or_localpart: &str,
        password: &str,
        device_display_name: &str,
        preferred_device_id: Option<&str>,
    ) -> Result<(), BackendError> {
        MatrixBackend::login_password(
            self,
            user_id_or_localpart,
            password,
            device_display_name,
            preferred_device_id,
        )
        .await
    }

    async fn start_sync(
        &self,
        event_tx: broadcast::Sender<BackendEvent>,
        sync_request_timeout: Option<Duration>,
    ) -> Result<(), BackendError> {
        MatrixBackend::start_sync(self, event_tx, sync_request_timeout).await
    }

    async fn stop_sync(&self) -> Result<(), BackendError> {
        MatrixBackend::stop_sync(self).await
    }

    async fn send_message(
        &self,
        room_id: &str,
        body: &str,
        msgtype: MessageType,
    ) -> Result<String, BackendError> {
        MatrixBackend::send_message(self, room_id, body, msgtype).await
    }

    async fn edit_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        new_body: &str,
    ) -> Result<String, BackendError> {
        MatrixBackend::edit_message(self, room_id, target_event_id, new_body).await
    }

    async fn redact_message(
        &self,
        room_id: &str,
        target_event_id: &str,
        reason: Option<String>,
    ) -> Result<(), BackendError> {
        MatrixBackend::redact_message(self, room_id, target_event_id, reason).await
    }

    async fn open_room(
        &self,
        room_id: &str,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
        MatrixBackend::open_room(self, room_id, limit).await
    }

    async fn paginate_back(
        &self,
        room_id: &str,
        from_token: Option<&str>,
        limit: u16,
    ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
        MatrixBackend::paginate_back(self, room_id, from_token, limit).await
    }

    async fn send_dm_text(&self, user_id: &str, body: &str) -> Result<String, BackendError> {
        MatrixBackend::send_dm_text(self, user_id, body).await
    }

    async fn accept_room_invite(&self, room_id: &str) -> Result<(), BackendError> {
        MatrixBackend::accept_room_invite(self, room_id).await
    }

    async fn reject_room_invite(&self, room_id: &str) -> Result<(), BackendError> {
        MatrixBackend::reject_room_invite(self, room_id).await
    }

    async fn upload_media(
        &self,
        content_type: &str,
        data: Vec<u8>,
    ) -> Result<String, BackendError> {
        MatrixBackend::upload_media(self, content_type, data).await
    }

    async fn download_media(&self, source: &str) -> Result<Vec<u8>, BackendError> {
        MatrixBackend::download_media(self, source).await
    }

    async fn get_recovery_status(&self) -> Result<RecoveryStatus, BackendError> {
        MatrixBackend::get_recovery_status(self).await
    }

    async fn enable_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError> {
        MatrixBackend::enable_recovery(self, passphrase, wait_for_backups_to_upload).await
    }

    async fn reset_recovery(
        &self,
        passphrase: Option<&str>,
        wait_for_backups_to_upload: bool,
    ) -> Result<String, BackendError> {
        MatrixBackend::reset_recovery(self, passphrase, wait_for_backups_to_upload).await
    }

    async fn recover_secrets(&self, recovery_key: &str) -> Result<(), BackendError> {
        MatrixBackend::recover_secrets(self, recovery_key).await
    }
}

/// Handle for interacting with the spawned backend runtime task.
#[derive(Clone, Debug)]
pub struct MatrixRuntimeHandle {
    channels: BackendChannels,
}

impl MatrixRuntimeHandle {
    /// Send one command to the backend runtime.
    pub async fn send(&self, command: BackendCommand) -> Result<(), BackendChannelError> {
        self.channels.send_command(command).await
    }

    /// Subscribe to backend events.
    pub fn subscribe(&self) -> EventStream {
        self.channels.subscribe()
    }
}

/// Spawn the Matrix runtime task and return a handle.
pub fn spawn_runtime() -> MatrixRuntimeHandle {
    let (channels, command_rx) = BackendChannels::new(128, 512);
    let runtime = MatrixRuntime::new(channels.clone(), command_rx);
    tokio::spawn(async move {
        runtime.run().await;
    });

    MatrixRuntimeHandle { channels }
}

/// Opaque ID returned when queuing commands in [`MatrixFrontendAdapter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrontendCommandId(u64);

impl FrontendCommandId {
    /// Raw numeric value of this command ID.
    pub fn value(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
struct QueuedFrontendCommand {
    id: FrontendCommandId,
    command: BackendCommand,
}

/// Frontend-oriented wrapper around runtime handle with command queueing and
/// timeline snapshot convenience events.
#[derive(Debug)]
pub struct MatrixFrontendAdapter {
    runtime: MatrixRuntimeHandle,
    queue: std::sync::Mutex<VecDeque<QueuedFrontendCommand>>,
    next_command_id: AtomicU64,
    timelines: Arc<std::sync::Mutex<HashMap<String, TimelineBuffer>>>,
    timeline_max_items: usize,
    event_tx: broadcast::Sender<BackendEvent>,
    stop: CancellationToken,
    event_task: JoinHandle<()>,
}

impl MatrixFrontendAdapter {
    /// Create an adapter with default buffer sizes.
    pub fn new(runtime: MatrixRuntimeHandle) -> Self {
        Self::with_config(runtime, 512, DEFAULT_TIMELINE_MAX_ITEMS)
    }

    /// Create an adapter with custom event channel buffer size.
    pub fn with_event_buffer(runtime: MatrixRuntimeHandle, event_buffer: usize) -> Self {
        Self::with_config(runtime, event_buffer, DEFAULT_TIMELINE_MAX_ITEMS)
    }

    /// Create an adapter with full custom configuration.
    ///
    /// `timeline_max_items` is clamped to at least `1`.
    pub fn with_config(
        runtime: MatrixRuntimeHandle,
        event_buffer: usize,
        timeline_max_items: usize,
    ) -> Self {
        debug!(
            event_buffer = event_buffer.max(1),
            timeline_max_items = timeline_max_items.max(1),
            "creating MatrixFrontendAdapter"
        );
        let (event_tx, _) = broadcast::channel(event_buffer.max(1));
        let stop = CancellationToken::new();
        let timelines = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mut runtime_events = runtime.subscribe();
        let event_tx_clone = event_tx.clone();
        let stop_clone = stop.clone();
        let timelines_clone = timelines.clone();
        let timeline_max_items = timeline_max_items.max(1);
        let event_task = tokio::spawn(async move {
            debug!("frontend adapter event relay task started");
            loop {
                tokio::select! {
                    _ = stop_clone.cancelled() => break,
                    recv = runtime_events.recv() => {
                        match recv {
                            Ok(event) => {
                                if let BackendEvent::RoomTimelineDelta { room_id, ops } = &event {
                                    trace!(
                                        room_id = %room_id,
                                        op_count = ops.len(),
                                        "adapter received room timeline delta"
                                    );
                                    let snapshot = {
                                        let mut timelines = timelines_clone
                                            .lock()
                                            .expect("frontend timeline map lock poisoned");
                                        let buffer = timelines
                                            .entry(room_id.clone())
                                            .or_insert_with(|| TimelineBuffer::new(timeline_max_items));
                                        apply_timeline_ops_lenient(buffer, ops);
                                        buffer.items().to_vec()
                                    };
                                    // Snapshot emission is an adapter-level concern; runtime stays delta-only.
                                    let _ = event_tx_clone.send(event.clone());
                                    let _ = event_tx_clone.send(BackendEvent::RoomTimelineSnapshot {
                                        room_id: room_id.clone(),
                                        items: snapshot,
                                    });
                                    continue;
                                }
                                let _ = event_tx_clone.send(event);
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                warn!(skipped, "adapter runtime event stream lagged");
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                warn!("adapter runtime event stream closed");
                                break
                            }
                        }
                    }
                }
            }
            debug!("frontend adapter event relay task stopped");
        });

        Self {
            runtime,
            queue: std::sync::Mutex::new(VecDeque::new()),
            next_command_id: AtomicU64::new(1),
            timelines,
            timeline_max_items,
            event_tx,
            stop,
            event_task,
        }
    }

    /// Subscribe to adapter events.
    ///
    /// This stream includes runtime events and adapter-generated
    /// `RoomTimelineSnapshot` events.
    pub fn subscribe(&self) -> EventStream {
        self.event_tx.subscribe()
    }

    /// Queue a command for later flush to runtime.
    pub fn enqueue(&self, command: BackendCommand) -> FrontendCommandId {
        let id = FrontendCommandId(self.next_command_id.fetch_add(1, Ordering::Relaxed));
        trace!(
            command_id = id.value(),
            command = command_kind(&command),
            "adapter enqueue"
        );
        self.queue
            .lock()
            .expect("frontend command queue lock poisoned")
            .push_back(QueuedFrontendCommand { id, command });
        id
    }

    /// Cancel a queued command by ID.
    ///
    /// Returns `true` if a queued item was removed.
    pub fn cancel(&self, command_id: FrontendCommandId) -> bool {
        let mut queue = self
            .queue
            .lock()
            .expect("frontend command queue lock poisoned");
        if let Some(index) = queue.iter().position(|item| item.id == command_id) {
            queue.remove(index);
            true
        } else {
            false
        }
    }

    /// Number of currently queued commands.
    pub fn queued_len(&self) -> usize {
        self.queue
            .lock()
            .expect("frontend command queue lock poisoned")
            .len()
    }

    /// Return current in-memory snapshot for a room.
    pub fn timeline_snapshot(&self, room_id: &str) -> Vec<TimelineItem> {
        self.timelines
            .lock()
            .expect("frontend timeline map lock poisoned")
            .get(room_id)
            .map(|buffer| buffer.items().to_vec())
            .unwrap_or_default()
    }

    /// Maximum retained timeline items per room snapshot.
    pub fn timeline_max_items(&self) -> usize {
        self.timeline_max_items
    }

    /// Flush one queued command to runtime.
    ///
    /// Returns `Ok(None)` when queue is empty.
    pub async fn flush_one(&self) -> Result<Option<FrontendCommandId>, BackendChannelError> {
        let maybe_command = self
            .queue
            .lock()
            .expect("frontend command queue lock poisoned")
            .pop_front();

        let Some(queued) = maybe_command else {
            return Ok(None);
        };

        if let Err(err) = self.runtime.send(queued.command.clone()).await {
            warn!(
                command_id = queued.id.value(),
                command = command_kind(&queued.command),
                error = %err,
                "adapter flush_one failed, command re-queued"
            );
            self.queue
                .lock()
                .expect("frontend command queue lock poisoned")
                .push_front(queued);
            return Err(err);
        }

        trace!(
            command_id = queued.id.value(),
            command = command_kind(&queued.command),
            "adapter flush_one sent command"
        );

        Ok(Some(queued.id))
    }

    /// Flush all currently queued commands.
    ///
    /// Returns number of commands successfully sent.
    pub async fn flush_all(&self) -> Result<usize, BackendChannelError> {
        let mut count = 0;
        while self.flush_one().await?.is_some() {
            count += 1;
        }
        Ok(count)
    }

    /// Stop adapter background tasks and consume the adapter.
    pub async fn shutdown(self) {
        info!("shutting down frontend adapter");
        self.stop.cancel();
        let _ = self.event_task.await;
    }
}

#[cfg(test)]
fn spawn_runtime_with_backend(backend: Arc<dyn RuntimeBackend>) -> MatrixRuntimeHandle {
    let (channels, command_rx) = BackendChannels::new(128, 512);
    let runtime = MatrixRuntime::new(channels.clone(), command_rx)
        .with_backend_override(backend)
        .with_login_timeout(Duration::from_millis(250));
    tokio::spawn(async move {
        runtime.run().await;
    });

    MatrixRuntimeHandle { channels }
}

#[derive(Debug, Clone)]
struct RuntimeInitState {
    homeserver: String,
    data_dir: PathBuf,
    session_account: String,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeTuning {
    sync_request_timeout: Option<Duration>,
    open_room_limit: u16,
    pagination_limit_cap: u16,
}

impl Default for RuntimeTuning {
    fn default() -> Self {
        Self {
            sync_request_timeout: None,
            open_room_limit: DEFAULT_OPEN_ROOM_LIMIT,
            pagination_limit_cap: DEFAULT_PAGINATION_LIMIT_CAP,
        }
    }
}

struct MatrixRuntime {
    channels: BackendChannels,
    command_rx: mpsc::Receiver<BackendCommand>,
    state_machine: BackendStateMachine,
    backend: Option<Arc<dyn RuntimeBackend>>,
    backend_override: Option<Arc<dyn RuntimeBackend>>,
    disable_session_persistence: bool,
    init_state: Option<RuntimeInitState>,
    runtime_tuning: RuntimeTuning,
    login_timeout: Duration,
    pagination_tokens: HashMap<String, Option<String>>,
    keyring: ScopedSecretStore<OsKeyringSecretStore>,
}

impl MatrixRuntime {
    fn new(channels: BackendChannels, command_rx: mpsc::Receiver<BackendCommand>) -> Self {
        Self {
            channels,
            command_rx,
            state_machine: BackendStateMachine::default(),
            backend: None,
            backend_override: None,
            disable_session_persistence: false,
            init_state: None,
            runtime_tuning: RuntimeTuning::default(),
            login_timeout: Duration::from_secs(DEFAULT_LOGIN_TIMEOUT_SECS),
            pagination_tokens: HashMap::new(),
            keyring: ScopedSecretStore::new(OsKeyringSecretStore, KEYRING_SERVICE),
        }
    }

    #[cfg(test)]
    fn with_backend_override(mut self, backend: Arc<dyn RuntimeBackend>) -> Self {
        self.backend_override = Some(backend);
        self.disable_session_persistence = true;
        self
    }

    #[cfg(test)]
    fn with_login_timeout(mut self, login_timeout: Duration) -> Self {
        self.login_timeout = login_timeout;
        self
    }

    async fn run(mut self) {
        info!("matrix runtime loop started");
        while let Some(command) = self.command_rx.recv().await {
            debug!(
                command = command_kind(&command),
                "matrix runtime received command"
            );
            if let Err(err) = self.handle_command(command).await {
                let recoverable = matches!(
                    err.category,
                    BackendErrorCategory::Network | BackendErrorCategory::RateLimited
                );
                error!(
                    code = %err.code,
                    message = %err.message,
                    recoverable,
                    "matrix runtime command handling failed"
                );
                self.channels.emit(BackendEvent::FatalError {
                    code: err.code,
                    message: err.message,
                    recoverable,
                });
            }
        }
        warn!("matrix runtime loop exiting: command channel closed");
    }

    async fn handle_command(&mut self, command: BackendCommand) -> Result<(), BackendError> {
        match command {
            BackendCommand::Init {
                homeserver,
                data_dir,
                config,
            } => self.handle_init(homeserver, data_dir, config).await,
            BackendCommand::LoginPassword {
                user_id_or_localpart,
                password,
                persist_session,
            } => {
                self.handle_login_password(user_id_or_localpart, password, persist_session)
                    .await;
                Ok(())
            }
            BackendCommand::RestoreSession => {
                self.handle_restore_session().await;
                Ok(())
            }
            BackendCommand::StartSync => self.handle_start_sync().await,
            BackendCommand::StopSync => self.handle_stop_sync().await,
            BackendCommand::ListRooms => self.handle_list_rooms(),
            BackendCommand::AcceptRoomInvite {
                room_id,
                client_txn_id,
            } => {
                self.handle_invite_action(room_id, client_txn_id, InviteAction::Accept)
                    .await;
                Ok(())
            }
            BackendCommand::RejectRoomInvite {
                room_id,
                client_txn_id,
            } => {
                self.handle_invite_action(room_id, client_txn_id, InviteAction::Reject)
                    .await;
                Ok(())
            }
            BackendCommand::OpenRoom { room_id } => self.handle_open_room(room_id).await,
            BackendCommand::PaginateBack { room_id, limit } => {
                self.handle_paginate_back(room_id, limit).await
            }
            BackendCommand::SendDmText {
                user_id,
                client_txn_id,
                body,
            } => {
                self.handle_send_dm_text(user_id, client_txn_id, body).await;
                Ok(())
            }
            BackendCommand::SendMessage {
                room_id,
                client_txn_id,
                body,
                msgtype,
            } => {
                self.handle_send_message(room_id, client_txn_id, body, msgtype)
                    .await;
                Ok(())
            }
            BackendCommand::EditMessage {
                room_id,
                target_event_id,
                new_body,
                client_txn_id,
            } => {
                self.handle_edit_message(room_id, target_event_id, new_body, client_txn_id)
                    .await;
                Ok(())
            }
            BackendCommand::RedactMessage {
                room_id,
                target_event_id,
                reason,
            } => {
                self.handle_redact_message(room_id, target_event_id, reason)
                    .await
            }
            BackendCommand::UploadMedia {
                client_txn_id,
                content_type,
                data,
            } => {
                self.handle_upload_media(client_txn_id, content_type, data)
                    .await;
                Ok(())
            }
            BackendCommand::DownloadMedia {
                client_txn_id,
                source,
            } => {
                self.handle_download_media(client_txn_id, source).await;
                Ok(())
            }
            BackendCommand::GetRecoveryStatus => self.handle_get_recovery_status().await,
            BackendCommand::EnableRecovery {
                client_txn_id,
                passphrase,
                wait_for_backups_to_upload,
            } => {
                self.handle_enable_recovery(client_txn_id, passphrase, wait_for_backups_to_upload)
                    .await;
                Ok(())
            }
            BackendCommand::ResetRecovery {
                client_txn_id,
                passphrase,
                wait_for_backups_to_upload,
            } => {
                self.handle_reset_recovery(client_txn_id, passphrase, wait_for_backups_to_upload)
                    .await;
                Ok(())
            }
            BackendCommand::RecoverSecrets {
                client_txn_id,
                recovery_key,
            } => {
                self.handle_recover_secrets(client_txn_id, recovery_key)
                    .await;
                Ok(())
            }
            BackendCommand::Logout => self.handle_logout().await,
        }
    }

    async fn handle_init(
        &mut self,
        homeserver: String,
        data_dir: PathBuf,
        config: Option<BackendInitConfig>,
    ) -> Result<(), BackendError> {
        info!(
            homeserver = %homeserver,
            data_dir = %data_dir.display(),
            has_init_config = config.is_some(),
            "handling Init command"
        );
        let (candidate, transition_events) = self.validate_transition(BackendCommand::Init {
            homeserver: String::new(),
            data_dir: PathBuf::new(),
            config: None,
        })?;

        let runtime_tuning = runtime_tuning_from_init_config(config)?;
        let session_account = session_account_for_homeserver(&homeserver);
        let backend = if let Some(backend_override) = self.backend_override.clone() {
            backend_override
        } else {
            let passphrase_account = store_passphrase_account_for_homeserver(&homeserver);
            let store_passphrase =
                self.get_or_create_store_passphrase(&passphrase_account, &data_dir)?;
            let backend: Arc<dyn RuntimeBackend> = Arc::new(
                MatrixBackend::new(MatrixBackendConfig::new(
                    homeserver.clone(),
                    data_dir.clone(),
                    Some(store_passphrase),
                ))
                .await?,
            );
            backend
        };

        self.backend = Some(backend);
        self.init_state = Some(RuntimeInitState {
            homeserver,
            data_dir,
            session_account,
        });
        self.runtime_tuning = runtime_tuning;
        self.pagination_tokens.clear();

        self.commit_transition(candidate, transition_events);
        info!("Init completed");
        Ok(())
    }

    async fn handle_login_password(
        &mut self,
        user_id_or_localpart: String,
        password: String,
        persist_session: bool,
    ) {
        info!(user = %user_id_or_localpart, "handling LoginPassword command");
        let transition = self.validate_transition(BackendCommand::LoginPassword {
            user_id_or_localpart: String::new(),
            password: String::new(),
            persist_session,
        });

        let Ok((candidate, transition_events)) = transition else {
            if let Err(err) = transition {
                self.emit_auth_failure(err);
            }
            return;
        };

        self.commit_transition(candidate, transition_events);

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.finish_auth(false, Some(err));
                return;
            }
        };
        let preferred_device_id = self.preferred_login_device_id();

        let login_result = self
            .login_password_with_retry(
                &backend,
                &user_id_or_localpart,
                &password,
                preferred_device_id.as_deref(),
            )
            .await;

        match login_result {
            Ok(()) => {
                if let Err(err) =
                    self.finalize_successful_password_login(&user_id_or_localpart, persist_session)
                {
                    error!(code = %err.code, message = %err.message, "failed to persist session after login");
                    self.finish_auth(false, Some(err));
                }
            }
            Err(err) => {
                if self.backend_override.is_none()
                    && is_store_account_mismatch_message(&err.message)
                {
                    let retry_preferred_device_id =
                        expected_device_id_from_store_mismatch_message(&err.message)
                            .or_else(|| preferred_device_id.clone());
                    if let Some(device_id) = retry_preferred_device_id.as_deref()
                        && let Err(hint_err) = self.persist_device_id_hint(device_id)
                    {
                        warn!(
                            code = %hint_err.code,
                            message = %hint_err.message,
                            device_id,
                            "failed to persist mismatch-derived preferred device-id hint"
                        );
                    }
                    warn!(
                        code = %err.code,
                        message = %err.message,
                        "password login failed due store/account mismatch; attempting store reset and retry"
                    );
                    drop(backend);
                    self.backend = None;
                    match self.recover_from_store_account_mismatch().await {
                        Ok(()) => {
                            let backend = match self.require_backend() {
                                Ok(backend) => backend,
                                Err(recovery_err) => {
                                    self.finish_auth(false, Some(recovery_err));
                                    return;
                                }
                            };
                            let retry_result = self
                                .login_password_with_retry(
                                    &backend,
                                    &user_id_or_localpart,
                                    &password,
                                    retry_preferred_device_id.as_deref(),
                                )
                                .await;
                            match retry_result {
                                Ok(()) => {
                                    if let Err(retry_err) = self.finalize_successful_password_login(
                                        &user_id_or_localpart,
                                        persist_session,
                                    ) {
                                        error!(code = %retry_err.code, message = %retry_err.message, "failed to persist session after login retry");
                                        self.finish_auth(false, Some(retry_err));
                                    }
                                }
                                Err(retry_err) => {
                                    warn!(
                                        code = %retry_err.code,
                                        message = %retry_err.message,
                                        "password login failed after store mismatch recovery"
                                    );
                                    self.finish_auth(false, Some(retry_err));
                                }
                            }
                        }
                        Err(recovery_err) => {
                            error!(
                                code = %recovery_err.code,
                                message = %recovery_err.message,
                                "store/account mismatch recovery failed"
                            );
                            self.finish_auth(false, Some(recovery_err));
                        }
                    }
                    return;
                }

                // Some stores may already contain an authenticated session. In that case, a
                // fresh password login can fail (already-logged-in, rate limit, timeout) even
                // though sync-capable session state is present.
                if backend.session().is_some() && is_session_login_fallback_error(&err) {
                    warn!(
                        code = %err.code,
                        message = %err.message,
                        "password login failed but existing session is present; continuing as authenticated"
                    );
                    self.finish_auth(true, None);
                } else {
                    warn!(
                        code = %err.code,
                        message = %err.message,
                        "password login failed"
                    );
                    self.finish_auth(false, Some(err));
                }
            }
        }
    }

    async fn handle_restore_session(&mut self) {
        let transition = self.validate_transition(BackendCommand::RestoreSession);
        let Ok((candidate, transition_events)) = transition else {
            if let Err(err) = transition {
                self.emit_auth_failure(err);
            }
            return;
        };

        self.commit_transition(candidate, transition_events);

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.finish_auth(false, Some(err));
                return;
            }
        };

        let init_state = match self.require_init_state() {
            Ok(state) => state.clone(),
            Err(err) => {
                self.finish_auth(false, Some(err));
                return;
            }
        };

        let session = match self.load_session(&init_state.session_account) {
            Ok(session) => session,
            Err(err) => {
                self.finish_auth(false, Some(err));
                return;
            }
        };

        match backend.restore_session(session).await {
            Ok(()) => {
                if let Err(err) = self.persist_device_id_hint_from_current_session() {
                    warn!(
                        code = %err.code,
                        message = %err.message,
                        "failed to persist device-id hint after session restore"
                    );
                }
                self.finish_auth(true, None)
            }
            Err(err) => self.finish_auth(false, Some(err)),
        }
    }

    async fn login_password_with_timeout(
        &self,
        backend: &Arc<dyn RuntimeBackend>,
        user_id_or_localpart: &str,
        password: &str,
        preferred_device_id: Option<&str>,
    ) -> Result<(), BackendError> {
        let timeout_duration = self.login_timeout;
        match time::timeout(
            timeout_duration,
            backend.login_password(
                user_id_or_localpart,
                password,
                DEFAULT_DEVICE_DISPLAY_NAME,
                preferred_device_id,
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(BackendError::new(
                BackendErrorCategory::Network,
                "auth_login_timeout",
                format!(
                    "password login timed out after {} ms",
                    timeout_duration.as_millis()
                ),
            )),
        }
    }

    async fn login_password_with_retry(
        &self,
        backend: &Arc<dyn RuntimeBackend>,
        user_id_or_localpart: &str,
        password: &str,
        preferred_device_id: Option<&str>,
    ) -> Result<(), BackendError> {
        let mut attempt = 0u8;
        loop {
            let result = self
                .login_password_with_timeout(
                    backend,
                    user_id_or_localpart,
                    password,
                    preferred_device_id,
                )
                .await;
            let Err(err) = &result else {
                return result;
            };

            if err.category != BackendErrorCategory::RateLimited || attempt >= 3 {
                return result;
            }

            let retry_after_ms = err.retry_after_ms.unwrap_or(2_000).max(250);
            attempt = attempt.saturating_add(1);
            warn!(
                retry_after_ms,
                attempt, "password login rate-limited; retrying"
            );
            time::sleep(Duration::from_millis(retry_after_ms)).await;
        }
    }

    fn finalize_successful_password_login(
        &mut self,
        user_id_or_localpart: &str,
        persist_session: bool,
    ) -> Result<(), BackendError> {
        if persist_session {
            self.persist_current_session()?;
        } else {
            self.clear_persisted_session()?;
        }
        self.persist_device_id_hint_from_current_session()?;
        info!(user = %user_id_or_localpart, "password login succeeded");
        self.finish_auth(true, None);
        Ok(())
    }

    fn preferred_login_device_id(&self) -> Option<String> {
        let init_state = self.require_init_state().ok()?;

        if let Ok(session) = self.load_session(&init_state.session_account) {
            return Some(session.meta.device_id.to_string());
        }

        let hint_path = device_id_hint_file_path(&init_state.data_dir);
        match load_device_id_hint_from_file(&hint_path) {
            Ok(Some(value)) => Some(value),
            Ok(None) => {
                let legacy_hint_path = legacy_device_id_hint_file_path(&init_state.data_dir);
                match load_device_id_hint_from_file(&legacy_hint_path) {
                    Ok(Some(value)) => {
                        if let Err(err) = persist_device_id_hint_to_file(&hint_path, &value) {
                            warn!(
                                code = %err.code,
                                message = %err.message,
                                path = %hint_path.display(),
                                "failed to migrate legacy device-id hint to new path"
                            );
                        }
                        Some(value)
                    }
                    Ok(None) => None,
                    Err(err) => {
                        warn!(
                            code = %err.code,
                            message = %err.message,
                            path = %legacy_hint_path.display(),
                            "failed to load legacy preferred device-id hint"
                        );
                        None
                    }
                }
            }
            Err(err) => {
                warn!(
                    code = %err.code,
                    message = %err.message,
                    "failed to load preferred device-id hint"
                );
                None
            }
        }
    }

    fn persist_device_id_hint_from_current_session(&self) -> Result<(), BackendError> {
        let backend = self.require_backend()?;
        let session = backend.session().ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Auth,
                "session_unavailable",
                "matrix session is unavailable after login",
            )
        })?;
        self.persist_device_id_hint(session.meta.device_id.as_str())
    }

    fn persist_device_id_hint(&self, device_id: &str) -> Result<(), BackendError> {
        let init_state = self.require_init_state()?;
        let hint_path = device_id_hint_file_path(&init_state.data_dir);
        persist_device_id_hint_to_file(&hint_path, device_id)
    }

    async fn recover_from_store_account_mismatch(&mut self) -> Result<(), BackendError> {
        let init_state = self.require_init_state()?.clone();
        let passphrase_account = store_passphrase_account_for_homeserver(&init_state.homeserver);
        self.wipe_local_persistence(&init_state.data_dir)?;
        info!(
            data_dir = %init_state.data_dir.display(),
            "removed mismatched matrix data directory"
        );

        let store_passphrase =
            self.get_or_create_store_passphrase(&passphrase_account, &init_state.data_dir)?;
        let backend: Arc<dyn RuntimeBackend> = Arc::new(
            MatrixBackend::new(MatrixBackendConfig::new(
                init_state.homeserver.clone(),
                init_state.data_dir.clone(),
                Some(store_passphrase),
            ))
            .await?,
        );
        self.backend = Some(backend);

        info!(
            homeserver = %init_state.homeserver,
            data_dir = %init_state.data_dir.display(),
            "recovered backend after store/account mismatch"
        );
        Ok(())
    }

    async fn handle_start_sync(&mut self) -> Result<(), BackendError> {
        info!("handling StartSync command");
        let (candidate, transition_events) = self.validate_transition(BackendCommand::StartSync)?;
        let backend = self.require_backend()?;
        backend
            .start_sync(
                self.channels.event_sender(),
                self.runtime_tuning.sync_request_timeout,
            )
            .await?;
        self.commit_transition(candidate, transition_events);
        info!("StartSync completed");
        Ok(())
    }

    async fn handle_stop_sync(&mut self) -> Result<(), BackendError> {
        let (candidate, transition_events) = self.validate_transition(BackendCommand::StopSync)?;
        let backend = self.require_backend()?;
        backend.stop_sync().await?;
        self.commit_transition(candidate, transition_events);
        Ok(())
    }

    fn handle_list_rooms(&mut self) -> Result<(), BackendError> {
        debug!("handling ListRooms command");
        let (_candidate, _events) = self.validate_transition(BackendCommand::ListRooms)?;
        let backend = self.require_backend()?;
        let rooms = backend.list_rooms();
        debug!(room_count = rooms.len(), "emitting RoomListUpdated");
        self.channels.emit(BackendEvent::RoomListUpdated { rooms });
        Ok(())
    }

    async fn handle_invite_action(
        &mut self,
        room_id: String,
        client_txn_id: String,
        action: InviteAction,
    ) {
        debug!(
            room_id = %room_id,
            client_txn_id = %client_txn_id,
            action = ?action,
            "handling room invite action command"
        );

        let validation = match action {
            InviteAction::Accept => self.validate_transition(BackendCommand::AcceptRoomInvite {
                room_id: String::new(),
                client_txn_id: String::new(),
            }),
            InviteAction::Reject => self.validate_transition(BackendCommand::RejectRoomInvite {
                room_id: String::new(),
                client_txn_id: String::new(),
            }),
        };

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::InviteActionAck(InviteActionAck {
                    client_txn_id,
                    room_id,
                    action,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::InviteActionAck(InviteActionAck {
                        client_txn_id,
                        room_id,
                        action,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        let result = match action {
            InviteAction::Accept => backend.accept_room_invite(&room_id).await,
            InviteAction::Reject => backend.reject_room_invite(&room_id).await,
        };

        let error_code = result.err().map(|err| err.code);
        self.channels
            .emit(BackendEvent::InviteActionAck(InviteActionAck {
                client_txn_id,
                room_id: room_id.clone(),
                action,
                error_code: error_code.clone(),
            }));

        if error_code.is_none() {
            let rooms = backend.list_rooms();
            self.channels.emit(BackendEvent::RoomListUpdated { rooms });
        }
    }

    async fn handle_open_room(&mut self, room_id: String) -> Result<(), BackendError> {
        info!(room_id = %room_id, "handling OpenRoom command");
        let (_candidate, _events) = self.validate_transition(BackendCommand::OpenRoom {
            room_id: String::new(),
        })?;
        let backend = self.require_backend()?;

        let (ops, next_token) = backend
            .open_room(&room_id, self.runtime_tuning.open_room_limit)
            .await?;
        debug!(
            room_id = %room_id,
            op_count = ops.len(),
            has_next_token = next_token.is_some(),
            limit = self.runtime_tuning.open_room_limit,
            "OpenRoom loaded timeline ops"
        );
        self.pagination_tokens.insert(room_id.clone(), next_token);
        self.channels
            .emit(BackendEvent::RoomTimelineDelta { room_id, ops });

        Ok(())
    }

    async fn handle_paginate_back(
        &mut self,
        room_id: String,
        limit: u16,
    ) -> Result<(), BackendError> {
        debug!(room_id = %room_id, requested_limit = limit, "handling PaginateBack command");
        let (_candidate, _events) = self.validate_transition(BackendCommand::PaginateBack {
            room_id: String::new(),
            limit,
        })?;

        let backend = self.require_backend()?;
        let from_token = self
            .pagination_tokens
            .get(&room_id)
            .and_then(|token| token.as_deref());
        let bounded_limit =
            TimelineBuffer::bounded_paginate_limit(limit, self.runtime_tuning.pagination_limit_cap);

        let (ops, next_token) = backend
            .paginate_back(&room_id, from_token, bounded_limit)
            .await?;
        debug!(
            room_id = %room_id,
            bounded_limit,
            op_count = ops.len(),
            has_next_token = next_token.is_some(),
            "PaginateBack loaded timeline ops"
        );

        self.pagination_tokens.insert(room_id.clone(), next_token);
        self.channels
            .emit(BackendEvent::RoomTimelineDelta { room_id, ops });

        Ok(())
    }

    async fn handle_send_dm_text(&mut self, user_id: String, client_txn_id: String, body: String) {
        debug!(
            user_id = %user_id,
            client_txn_id = %client_txn_id,
            body_len = body.len(),
            "handling SendDmText command"
        );
        let validation = self.validate_transition(BackendCommand::SendDmText {
            user_id: String::new(),
            client_txn_id: String::new(),
            body: String::new(),
        });

        if let Err(err) = validation {
            self.channels.emit(normalize_send_outcome(
                client_txn_id,
                SendOutcome::Failure { error: err },
            ));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels.emit(normalize_send_outcome(
                    client_txn_id,
                    SendOutcome::Failure { error: err },
                ));
                return;
            }
        };

        let outcome = match backend.send_dm_text(&user_id, &body).await {
            Ok(event_id) => SendOutcome::Success { event_id },
            Err(error) => SendOutcome::Failure { error },
        };

        self.channels
            .emit(normalize_send_outcome(client_txn_id, outcome));
    }

    async fn handle_send_message(
        &mut self,
        room_id: String,
        client_txn_id: String,
        body: String,
        msgtype: MessageType,
    ) {
        debug!(
            room_id = %room_id,
            client_txn_id = %client_txn_id,
            body_len = body.len(),
            msgtype = ?msgtype,
            "handling SendMessage command"
        );
        let validation = self.validate_transition(BackendCommand::SendMessage {
            room_id: String::new(),
            client_txn_id: String::new(),
            body: String::new(),
            msgtype: MessageType::Text,
        });

        if let Err(err) = validation {
            self.channels.emit(normalize_send_outcome(
                client_txn_id,
                SendOutcome::Failure { error: err },
            ));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels.emit(normalize_send_outcome(
                    client_txn_id,
                    SendOutcome::Failure { error: err },
                ));
                return;
            }
        };

        let outcome = match backend.send_message(&room_id, &body, msgtype).await {
            Ok(event_id) => SendOutcome::Success { event_id },
            Err(error) => SendOutcome::Failure { error },
        };

        self.channels
            .emit(normalize_send_outcome(client_txn_id, outcome));
    }

    async fn handle_edit_message(
        &mut self,
        room_id: String,
        target_event_id: String,
        new_body: String,
        client_txn_id: String,
    ) {
        debug!(
            room_id = %room_id,
            target_event_id = %target_event_id,
            client_txn_id = %client_txn_id,
            new_body_len = new_body.len(),
            "handling EditMessage command"
        );
        let validation = self.validate_transition(BackendCommand::EditMessage {
            room_id: String::new(),
            target_event_id: String::new(),
            new_body: String::new(),
            client_txn_id: String::new(),
        });

        if let Err(err) = validation {
            self.channels.emit(normalize_send_outcome(
                client_txn_id,
                SendOutcome::Failure { error: err },
            ));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels.emit(normalize_send_outcome(
                    client_txn_id,
                    SendOutcome::Failure { error: err },
                ));
                return;
            }
        };

        match backend
            .edit_message(&room_id, &target_event_id, &new_body)
            .await
        {
            Ok(event_id) => {
                self.channels.emit(normalize_send_outcome(
                    client_txn_id,
                    SendOutcome::Success {
                        event_id: event_id.clone(),
                    },
                ));
                self.channels.emit(BackendEvent::RoomTimelineDelta {
                    room_id,
                    ops: vec![TimelineOp::UpdateBody {
                        event_id: target_event_id,
                        new_body,
                    }],
                });
            }
            Err(err) => {
                self.channels.emit(normalize_send_outcome(
                    client_txn_id,
                    SendOutcome::Failure { error: err },
                ));
            }
        }
    }

    async fn handle_redact_message(
        &mut self,
        room_id: String,
        target_event_id: String,
        reason: Option<String>,
    ) -> Result<(), BackendError> {
        debug!(
            room_id = %room_id,
            target_event_id = %target_event_id,
            has_reason = reason.is_some(),
            "handling RedactMessage command"
        );
        let (_candidate, _events) = self.validate_transition(BackendCommand::RedactMessage {
            room_id: String::new(),
            target_event_id: String::new(),
            reason: None,
        })?;

        let backend = self.require_backend()?;
        backend
            .redact_message(&room_id, &target_event_id, reason)
            .await?;

        self.channels.emit(BackendEvent::RoomTimelineDelta {
            room_id,
            ops: vec![TimelineOp::Remove {
                event_id: target_event_id,
            }],
        });

        Ok(())
    }

    async fn handle_upload_media(
        &mut self,
        client_txn_id: String,
        content_type: String,
        data: Vec<u8>,
    ) {
        debug!(
            client_txn_id = %client_txn_id,
            content_type = %content_type,
            data_len = data.len(),
            "handling UploadMedia command"
        );
        let validation = self.validate_transition(BackendCommand::UploadMedia {
            client_txn_id: String::new(),
            content_type: String::new(),
            data: Vec::new(),
        });

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::MediaUploadAck(MediaUploadAck {
                    client_txn_id,
                    content_uri: None,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::MediaUploadAck(MediaUploadAck {
                        client_txn_id,
                        content_uri: None,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        let outcome = backend.upload_media(&content_type, data).await;
        match outcome {
            Ok(content_uri) => self
                .channels
                .emit(BackendEvent::MediaUploadAck(MediaUploadAck {
                    client_txn_id,
                    content_uri: Some(content_uri),
                    error_code: None,
                })),
            Err(err) => self
                .channels
                .emit(BackendEvent::MediaUploadAck(MediaUploadAck {
                    client_txn_id,
                    content_uri: None,
                    error_code: Some(err.code),
                })),
        }
    }

    async fn handle_download_media(&mut self, client_txn_id: String, source: String) {
        debug!(
            client_txn_id = %client_txn_id,
            source = %source,
            "handling DownloadMedia command"
        );
        let validation = self.validate_transition(BackendCommand::DownloadMedia {
            client_txn_id: String::new(),
            source: String::new(),
        });

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::MediaDownloadAck(MediaDownloadAck {
                    client_txn_id,
                    source,
                    data: None,
                    content_type: None,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::MediaDownloadAck(MediaDownloadAck {
                        client_txn_id,
                        source,
                        data: None,
                        content_type: None,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        let outcome = backend.download_media(&source).await;
        match outcome {
            Ok(data) => self
                .channels
                .emit(BackendEvent::MediaDownloadAck(MediaDownloadAck {
                    client_txn_id,
                    source,
                    data: Some(data),
                    content_type: None,
                    error_code: None,
                })),
            Err(err) => self
                .channels
                .emit(BackendEvent::MediaDownloadAck(MediaDownloadAck {
                    client_txn_id,
                    source,
                    data: None,
                    content_type: None,
                    error_code: Some(err.code),
                })),
        }
    }

    async fn handle_get_recovery_status(&mut self) -> Result<(), BackendError> {
        debug!("handling GetRecoveryStatus command");
        let (_candidate, _events) = self.validate_transition(BackendCommand::GetRecoveryStatus)?;
        let backend = self.require_backend()?;
        let status = backend.get_recovery_status().await?;
        self.channels.emit(BackendEvent::RecoveryStatus(status));
        Ok(())
    }

    async fn handle_enable_recovery(
        &mut self,
        client_txn_id: String,
        passphrase: Option<String>,
        wait_for_backups_to_upload: bool,
    ) {
        debug!(
            client_txn_id = %client_txn_id,
            has_passphrase = passphrase.is_some(),
            wait_for_backups_to_upload,
            "handling EnableRecovery command"
        );
        let validation = self.validate_transition(BackendCommand::EnableRecovery {
            client_txn_id: String::new(),
            passphrase: None,
            wait_for_backups_to_upload,
        });

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                    client_txn_id,
                    recovery_key: None,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id,
                        recovery_key: None,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        match backend
            .enable_recovery(passphrase.as_deref(), wait_for_backups_to_upload)
            .await
        {
            Ok(recovery_key) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id: client_txn_id.clone(),
                        recovery_key: Some(recovery_key),
                        error_code: None,
                    }));
                match backend.get_recovery_status().await {
                    Ok(status) => self.channels.emit(BackendEvent::RecoveryStatus(status)),
                    Err(err) => warn!(
                        code = %err.code,
                        message = %err.message,
                        "failed to fetch recovery status after enable"
                    ),
                }
            }
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id,
                        recovery_key: None,
                        error_code: Some(err.code),
                    }));
            }
        }
    }

    async fn handle_recover_secrets(&mut self, client_txn_id: String, recovery_key: String) {
        debug!(
            client_txn_id = %client_txn_id,
            recovery_key_len = recovery_key.len(),
            "handling RecoverSecrets command"
        );
        let validation = self.validate_transition(BackendCommand::RecoverSecrets {
            client_txn_id: String::new(),
            recovery_key: String::new(),
        });

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::RecoveryRestoreAck(RecoveryRestoreAck {
                    client_txn_id,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryRestoreAck(RecoveryRestoreAck {
                        client_txn_id,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        match backend.recover_secrets(&recovery_key).await {
            Ok(()) => {
                self.channels
                    .emit(BackendEvent::RecoveryRestoreAck(RecoveryRestoreAck {
                        client_txn_id: client_txn_id.clone(),
                        error_code: None,
                    }));
                match backend.get_recovery_status().await {
                    Ok(status) => self.channels.emit(BackendEvent::RecoveryStatus(status)),
                    Err(err) => warn!(
                        code = %err.code,
                        message = %err.message,
                        "failed to fetch recovery status after recover"
                    ),
                }
            }
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryRestoreAck(RecoveryRestoreAck {
                        client_txn_id,
                        error_code: Some(err.code),
                    }));
            }
        }
    }

    async fn handle_reset_recovery(
        &mut self,
        client_txn_id: String,
        passphrase: Option<String>,
        wait_for_backups_to_upload: bool,
    ) {
        debug!(
            client_txn_id = %client_txn_id,
            has_passphrase = passphrase.is_some(),
            wait_for_backups_to_upload,
            "handling ResetRecovery command"
        );
        let validation = self.validate_transition(BackendCommand::ResetRecovery {
            client_txn_id: String::new(),
            passphrase: None,
            wait_for_backups_to_upload,
        });

        if let Err(err) = validation {
            self.channels
                .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                    client_txn_id,
                    recovery_key: None,
                    error_code: Some(err.code),
                }));
            return;
        }

        let backend = match self.require_backend() {
            Ok(backend) => backend,
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id,
                        recovery_key: None,
                        error_code: Some(err.code),
                    }));
                return;
            }
        };

        match backend
            .reset_recovery(passphrase.as_deref(), wait_for_backups_to_upload)
            .await
        {
            Ok(recovery_key) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id: client_txn_id.clone(),
                        recovery_key: Some(recovery_key),
                        error_code: None,
                    }));
                match backend.get_recovery_status().await {
                    Ok(status) => self.channels.emit(BackendEvent::RecoveryStatus(status)),
                    Err(err) => warn!(
                        code = %err.code,
                        message = %err.message,
                        "failed to fetch recovery status after reset"
                    ),
                }
            }
            Err(err) => {
                self.channels
                    .emit(BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                        client_txn_id,
                        recovery_key: None,
                        error_code: Some(err.code),
                    }));
            }
        }
    }

    async fn handle_logout(&mut self) -> Result<(), BackendError> {
        let (candidate, transition_events) = self.validate_transition(BackendCommand::Logout)?;
        let backend = self.require_backend()?;
        let init_state = self.require_init_state()?.clone();

        if matches!(self.state_machine.state(), BackendLifecycleState::Syncing) {
            let _ = backend.stop_sync().await;
        }

        let remote_logout_error = backend.logout().await.err();
        drop(backend);
        self.backend = None;
        self.wipe_local_persistence(&init_state.data_dir)?;
        self.init_state = None;
        self.runtime_tuning = RuntimeTuning::default();
        self.pagination_tokens.clear();
        self.commit_transition(candidate, transition_events);

        if let Some(err) = remote_logout_error {
            warn!(
                code = %err.code,
                message = %err.message,
                "remote logout failed; local session and store were still cleared"
            );
        }

        Ok(())
    }

    fn validate_transition(
        &self,
        command: BackendCommand,
    ) -> Result<(BackendStateMachine, Vec<BackendEvent>), BackendError> {
        let mut candidate = self.state_machine.clone();
        let events = candidate.apply(&command)?;
        Ok((candidate, events))
    }

    fn commit_transition(&mut self, candidate: BackendStateMachine, events: Vec<BackendEvent>) {
        self.state_machine = candidate;
        for event in events {
            self.channels.emit(event);
        }
    }

    fn require_backend(&self) -> Result<Arc<dyn RuntimeBackend>, BackendError> {
        self.backend.clone().ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Config,
                "backend_not_initialized",
                "backend is not initialized; send Init first",
            )
        })
    }

    fn require_init_state(&self) -> Result<&RuntimeInitState, BackendError> {
        self.init_state.as_ref().ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Config,
                "backend_not_initialized",
                "runtime init state is not available; send Init first",
            )
        })
    }

    fn get_or_create_store_passphrase(
        &self,
        account: &str,
        data_dir: &Path,
    ) -> Result<String, BackendError> {
        let file_path = store_passphrase_file_path(data_dir);
        if let Some(from_file) = load_store_passphrase_from_file(&file_path)? {
            return Ok(from_file);
        }

        match self.keyring.get(account) {
            Ok(passphrase) => {
                persist_store_passphrase_to_file(&file_path, &passphrase)?;
                Ok(passphrase)
            }
            Err(SecretStoreError::NotFound)
            | Err(SecretStoreError::Unavailable(_))
            | Err(SecretStoreError::Backend(_)) => {
                let generated = format!("pikachat-store-{}", Uuid::new_v4());
                let _ = self.keyring.set(account, &generated);
                persist_store_passphrase_to_file(&file_path, &generated)?;
                Ok(generated)
            }
        }
    }

    fn persist_current_session(&self) -> Result<(), BackendError> {
        if self.disable_session_persistence {
            return Ok(());
        }

        let backend = self.require_backend()?;
        let init_state = self.require_init_state()?;

        let session = backend.session().ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Auth,
                "session_unavailable",
                "matrix session is unavailable after login",
            )
        })?;

        let encoded = serde_json::to_string(&session).map_err(|err| {
            BackendError::new(
                BackendErrorCategory::Serialization,
                "session_serialize_error",
                err.to_string(),
            )
        })?;

        self.keyring
            .set(&init_state.session_account, &encoded)
            .map_err(|err| map_secret_store_error("set_session", &init_state.session_account, err))
    }

    fn clear_persisted_session(&self) -> Result<(), BackendError> {
        if self.disable_session_persistence {
            return Ok(());
        }

        let init_state = self.require_init_state()?;
        match self.keyring.delete(&init_state.session_account) {
            Ok(()) | Err(SecretStoreError::NotFound) => Ok(()),
            Err(err) => Err(map_secret_store_error(
                "delete_session",
                &init_state.session_account,
                err,
            )),
        }
    }

    fn clear_store_passphrase_secret(&self) -> Result<(), BackendError> {
        if self.disable_session_persistence {
            return Ok(());
        }

        let init_state = self.require_init_state()?;
        let passphrase_account = store_passphrase_account_for_homeserver(&init_state.homeserver);
        match self.keyring.delete(&passphrase_account) {
            Ok(()) | Err(SecretStoreError::NotFound) => Ok(()),
            Err(err) => Err(map_secret_store_error(
                "delete_store_passphrase",
                &passphrase_account,
                err,
            )),
        }
    }

    fn clear_device_id_hint_files(&self, data_dir: &Path) -> Result<(), BackendError> {
        let hint_path = device_id_hint_file_path(data_dir);
        match fs::remove_file(&hint_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(BackendError::new(
                    BackendErrorCategory::Storage,
                    "device_id_hint_file_delete_error",
                    format!(
                        "failed deleting device-id hint file {}: {err}",
                        hint_path.display()
                    ),
                ));
            }
        }

        let legacy_hint_path = legacy_device_id_hint_file_path(data_dir);
        match fs::remove_file(&legacy_hint_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(BackendError::new(
                    BackendErrorCategory::Storage,
                    "device_id_hint_file_delete_error",
                    format!(
                        "failed deleting legacy device-id hint file {}: {err}",
                        legacy_hint_path.display()
                    ),
                ));
            }
        }

        Ok(())
    }

    fn clear_local_data_dir(&self, data_dir: &Path) -> Result<(), BackendError> {
        match fs::remove_dir_all(data_dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(BackendError::new(
                BackendErrorCategory::Storage,
                "store_reset_failed",
                format!(
                    "failed clearing matrix data dir {}: {err}",
                    data_dir.display()
                ),
            )),
        }
    }

    fn wipe_local_persistence(&self, data_dir: &Path) -> Result<(), BackendError> {
        self.clear_persisted_session()?;
        self.clear_store_passphrase_secret()?;
        self.clear_device_id_hint_files(data_dir)?;
        self.clear_local_data_dir(data_dir)
    }

    fn load_session(&self, session_account: &str) -> Result<MatrixSession, BackendError> {
        if self.disable_session_persistence {
            return Err(BackendError::new(
                BackendErrorCategory::Auth,
                "session_not_found",
                "session restore is unavailable in this runtime configuration",
            ));
        }

        let raw = self.keyring.get(session_account).map_err(|err| match err {
            SecretStoreError::NotFound => BackendError::new(
                BackendErrorCategory::Auth,
                "session_not_found",
                "no persisted session was found for restore",
            ),
            other => map_secret_store_error("get_session", session_account, other),
        })?;

        serde_json::from_str::<MatrixSession>(&raw).map_err(|err| {
            BackendError::new(
                BackendErrorCategory::Serialization,
                "session_deserialize_error",
                err.to_string(),
            )
        })
    }

    fn finish_auth(&mut self, success: bool, error: Option<BackendError>) {
        if let Ok(state_event) = self.state_machine.on_auth_result(success) {
            self.channels.emit(state_event);
        }

        self.channels.emit(BackendEvent::AuthResult {
            success,
            error_code: error.as_ref().map(|err| err.code.clone()),
        });
    }

    fn emit_auth_failure(&self, error: BackendError) {
        self.channels.emit(BackendEvent::AuthResult {
            success: false,
            error_code: Some(error.code),
        });
    }
}

fn parse_room_id(value: &str) -> Result<OwnedRoomId, BackendError> {
    value.parse::<OwnedRoomId>().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_room_id",
            format!("invalid room id '{value}': {err}"),
        )
    })
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

fn is_session_login_fallback_error(err: &BackendError) -> bool {
    let msg = err.message.to_ascii_lowercase();
    matches!(err.category, BackendErrorCategory::RateLimited)
        || err.code == "auth_login_timeout"
        || msg.contains("already logged in")
        || msg.contains("session already")
        || msg.contains("an existing session")
}

fn is_store_account_mismatch_message(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("account in the store doesn't match")
        || msg.contains("account in the store does not match")
        || msg.contains("belongs to a different account")
}

fn expected_device_id_from_store_mismatch_message(message: &str) -> Option<String> {
    let expected_segment = message
        .split("expected ")
        .nth(1)?
        .split(", got")
        .next()?
        .trim();
    let device_id = expected_segment.rsplit(':').next()?.trim();
    if device_id.is_empty() {
        None
    } else {
        Some(device_id.to_owned())
    }
}

fn parse_event_id(value: &str) -> Result<OwnedEventId, BackendError> {
    value.parse::<OwnedEventId>().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_event_id",
            format!("invalid event id '{value}': {err}"),
        )
    })
}

fn parse_user_id(value: &str) -> Result<OwnedUserId, BackendError> {
    value.parse::<OwnedUserId>().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_user_id",
            format!("invalid user id '{value}': {err}"),
        )
    })
}

fn parse_mxc_uri(value: &str) -> Result<OwnedMxcUri, BackendError> {
    let uri = OwnedMxcUri::from(value.to_owned());
    uri.validate().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_mxc_uri",
            format!("invalid mxc uri '{value}': {err}"),
        )
    })?;
    Ok(uri)
}

fn parse_media_content_type(value: &str) -> Result<mime::Mime, BackendError> {
    value.parse::<mime::Mime>().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_media_content_type",
            format!("invalid media content type '{value}': {err}"),
        )
    })
}

fn store_passphrase_account_for_homeserver(homeserver: &str) -> String {
    format!("{STORE_PASSPHRASE_KEY_PREFIX}:{homeserver}")
}

fn session_account_for_homeserver(homeserver: &str) -> String {
    format!("{SESSION_KEY_PREFIX}:{homeserver}")
}

fn store_passphrase_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STORE_PASSPHRASE_FILENAME)
}

fn device_id_hint_file_path(data_dir: &Path) -> PathBuf {
    let data_dir_name = data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("pikachat-store");
    let hint_filename = format!(".{data_dir_name}{DEVICE_ID_HINT_SUFFIX}");
    match data_dir.parent() {
        Some(parent) => parent.join(hint_filename),
        None => PathBuf::from(hint_filename),
    }
}

fn legacy_device_id_hint_file_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LEGACY_DEVICE_ID_HINT_FILENAME)
}

fn load_store_passphrase_from_file(path: &Path) -> Result<Option<String>, BackendError> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                return Err(BackendError::new(
                    BackendErrorCategory::Storage,
                    "store_passphrase_file_empty",
                    format!("store passphrase file is empty: {}", path.display()),
                ));
            }
            Ok(Some(trimmed))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(BackendError::new(
            BackendErrorCategory::Storage,
            "store_passphrase_file_read_error",
            format!(
                "failed reading store passphrase file {}: {err}",
                path.display()
            ),
        )),
    }
}

fn persist_store_passphrase_to_file(path: &Path, passphrase: &str) -> Result<(), BackendError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            BackendError::new(
                BackendErrorCategory::Storage,
                "store_passphrase_file_write_error",
                format!("failed creating data dir {}: {err}", parent.display()),
            )
        })?;
    }

    fs::write(path, passphrase).map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Storage,
            "store_passphrase_file_write_error",
            format!(
                "failed writing store passphrase file {}: {err}",
                path.display()
            ),
        )
    })
}

fn load_device_id_hint_from_file(path: &Path) -> Result<Option<String>, BackendError> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim().to_owned();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(BackendError::new(
            BackendErrorCategory::Storage,
            "device_id_hint_file_read_error",
            format!(
                "failed reading device-id hint file {}: {err}",
                path.display()
            ),
        )),
    }
}

fn persist_device_id_hint_to_file(path: &Path, device_id: &str) -> Result<(), BackendError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            BackendError::new(
                BackendErrorCategory::Storage,
                "device_id_hint_file_write_error",
                format!("failed creating data dir {}: {err}", parent.display()),
            )
        })?;
    }

    fs::write(path, device_id).map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Storage,
            "device_id_hint_file_write_error",
            format!(
                "failed writing device-id hint file {}: {err}",
                path.display()
            ),
        )
    })
}

fn runtime_tuning_from_init_config(
    config: Option<BackendInitConfig>,
) -> Result<RuntimeTuning, BackendError> {
    let config = config.unwrap_or_default();

    let sync_request_timeout = match config.sync_request_timeout_ms {
        Some(0) => {
            return Err(BackendError::new(
                BackendErrorCategory::Config,
                "invalid_init_config",
                "sync_request_timeout_ms must be greater than zero when provided",
            ));
        }
        Some(timeout_ms) => Some(Duration::from_millis(timeout_ms)),
        None => None,
    };

    let open_room_limit = config
        .default_open_room_limit
        .unwrap_or(DEFAULT_OPEN_ROOM_LIMIT);
    if open_room_limit == 0 {
        return Err(BackendError::new(
            BackendErrorCategory::Config,
            "invalid_init_config",
            "default_open_room_limit must be greater than zero when provided",
        ));
    }

    let pagination_limit_cap = config
        .pagination_limit_cap
        .unwrap_or(DEFAULT_PAGINATION_LIMIT_CAP);
    if pagination_limit_cap == 0 {
        return Err(BackendError::new(
            BackendErrorCategory::Config,
            "invalid_init_config",
            "pagination_limit_cap must be greater than zero when provided",
        ));
    }

    Ok(RuntimeTuning {
        sync_request_timeout,
        open_room_limit,
        pagination_limit_cap,
    })
}

fn messages_options(from_token: Option<&str>, limit: u16) -> Result<MessagesOptions, BackendError> {
    let mut options = MessagesOptions::backward();
    options.from = from_token.map(ToOwned::to_owned);
    options.limit = UInt::new(u64::from(limit)).ok_or_else(|| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_pagination_limit",
            format!("invalid pagination limit: {limit}"),
        )
    })?;
    Ok(options)
}

#[derive(Clone, Copy)]
enum TimelineInsertMode {
    Append,
    Prepend,
}

fn timeline_ops_for_open(messages: Messages) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    for event in messages.chunk.iter().rev() {
        if let Some(op) = timeline_op_from_event(event, TimelineInsertMode::Append) {
            ops.push(op);
        }
    }
    ops
}

fn timeline_ops_for_pagination(messages: Messages) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    for event in &messages.chunk {
        if let Some(op) = timeline_op_from_event(event, TimelineInsertMode::Prepend) {
            ops.push(op);
        }
    }
    ops
}

fn timeline_ops_for_sync_events(
    events: &[TimelineEvent],
    limited: bool,
    known_event_bodies: &mut HashMap<String, String>,
) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    if limited {
        ops.push(TimelineOp::Clear);
        known_event_bodies.clear();
    }
    for event in events {
        if let Some(op) = timeline_op_from_event(event, TimelineInsertMode::Append) {
            match op {
                TimelineOp::Append(item) => {
                    if let Some(event_id) = item.event_id.as_ref() {
                        if let Some(existing_body) = known_event_bodies.get(event_id) {
                            if existing_body == &item.body {
                                continue;
                            }
                            known_event_bodies.insert(event_id.clone(), item.body.clone());
                            ops.push(TimelineOp::UpdateBody {
                                event_id: event_id.clone(),
                                new_body: item.body,
                            });
                            continue;
                        }
                        known_event_bodies.insert(event_id.clone(), item.body.clone());
                    }
                    ops.push(TimelineOp::Append(item));
                }
                TimelineOp::UpdateBody { event_id, new_body } => {
                    known_event_bodies.insert(event_id.clone(), new_body.clone());
                    ops.push(TimelineOp::UpdateBody { event_id, new_body });
                }
                TimelineOp::Remove { event_id } => {
                    known_event_bodies.remove(&event_id);
                    ops.push(TimelineOp::Remove { event_id });
                }
                other => ops.push(other),
            }
        }
    }
    ops
}

fn timeline_op_from_event(
    event: &TimelineEvent,
    insert_mode: TimelineInsertMode,
) -> Option<TimelineOp> {
    if let Some(redaction_op) = redaction_op_from_event(event) {
        return Some(redaction_op);
    }

    if let Some(edit_op) = edit_op_from_event(event) {
        return Some(edit_op);
    }

    let mapped = timeline_item_from_event(event).map(|item| match insert_mode {
        TimelineInsertMode::Append => TimelineOp::Append(item),
        TimelineInsertMode::Prepend => TimelineOp::Prepend(item),
    });
    if mapped.is_none() {
        trace!(
            event_type = event_type(event).as_deref().unwrap_or("unknown"),
            "timeline event not mapped to timeline op"
        );
    }
    mapped
}

fn redaction_op_from_event(event: &TimelineEvent) -> Option<TimelineOp> {
    if event_type(event).as_deref() != Some(ROOM_REDACTION_EVENT_TYPE) {
        return None;
    }

    let redacts = event.raw().get_field::<String>("redacts").ok().flatten()?;
    Some(TimelineOp::Remove { event_id: redacts })
}

fn edit_op_from_event(event: &TimelineEvent) -> Option<TimelineOp> {
    if event_type(event).as_deref() != Some(ROOM_MESSAGE_EVENT_TYPE) {
        return None;
    }

    let content = event
        .raw()
        .get_field::<serde_json::Value>("content")
        .ok()
        .flatten()?;

    let relates_to = content.get("m.relates_to")?;
    if relates_to.get("rel_type").and_then(|value| value.as_str()) != Some(REL_TYPE_REPLACE) {
        return None;
    }

    let target_event_id = relates_to
        .get("event_id")
        .and_then(|value| value.as_str())?;
    let new_body = content
        .get("m.new_content")
        .and_then(extract_basic_message_body)
        .or_else(|| extract_basic_message_body(&content))?;

    Some(TimelineOp::UpdateBody {
        event_id: target_event_id.to_owned(),
        new_body,
    })
}

fn event_type(event: &TimelineEvent) -> Option<String> {
    event.raw().get_field::<String>("type").ok().flatten()
}

fn sync_timeline_deltas(
    sync_response: &matrix_sdk::sync::SyncResponse,
    room_event_bodies: &mut HashMap<String, HashMap<String, String>>,
) -> Vec<(String, Vec<TimelineOp>)> {
    let mut deltas = Vec::new();

    for (room_id, update) in &sync_response.rooms.joined {
        let room_id = room_id.to_string();
        let known_event_bodies = room_event_bodies.entry(room_id.clone()).or_default();
        let ops = timeline_ops_for_sync_events(
            &update.timeline.events,
            update.timeline.limited,
            known_event_bodies,
        );
        if !ops.is_empty() {
            deltas.push((room_id, ops));
        }
    }

    deltas
}

fn sync_response_has_room_key_events(sync_response: &matrix_sdk::sync::SyncResponse) -> bool {
    sync_response.to_device.iter().any(|event| {
        matches!(
            event
                .as_raw()
                .get_field::<String>("type")
                .ok()
                .flatten()
                .as_deref(),
            Some(TO_DEVICE_ROOM_KEY_EVENT_TYPE | TO_DEVICE_FORWARDED_ROOM_KEY_EVENT_TYPE)
        )
    })
}

async fn refresh_room_timeline_updates_after_room_keys(
    client: &Client,
    room_event_bodies: &mut HashMap<String, HashMap<String, String>>,
) -> Vec<(String, Vec<TimelineOp>)> {
    let candidate_room_ids: Vec<String> = room_event_bodies
        .iter()
        .filter(|(_, known_event_bodies)| !known_event_bodies.is_empty())
        .map(|(room_id, _)| room_id.clone())
        .take(ROOM_KEY_REFRESH_ROOM_LIMIT)
        .collect();

    let mut deltas = Vec::new();
    for room_id in candidate_room_ids {
        let Ok(parsed_room_id) = parse_room_id(&room_id) else {
            continue;
        };
        let Some(room) = client.get_room(parsed_room_id.as_ref()) else {
            continue;
        };

        let options = match messages_options(None, ROOM_KEY_REFRESH_EVENT_LIMIT) {
            Ok(options) => options,
            Err(err) => {
                warn!(
                    code = %err.code,
                    message = %err.message,
                    room_id = %room_id,
                    "unable to build room-key refresh pagination options"
                );
                continue;
            }
        };

        let messages = match room.messages(options).await {
            Ok(messages) => messages,
            Err(err) => {
                let mapped = map_matrix_error(err);
                debug!(
                    code = %mapped.code,
                    message = %mapped.message,
                    room_id = %room_id,
                    "room-key refresh fetch failed"
                );
                continue;
            }
        };

        let Some(known_event_bodies) = room_event_bodies.get_mut(&room_id) else {
            continue;
        };
        let ops = timeline_update_ops_for_refresh_events(&messages.chunk, known_event_bodies);
        if !ops.is_empty() {
            deltas.push((room_id, ops));
        }
    }

    deltas
}

fn timeline_update_ops_for_refresh_events(
    events: &[TimelineEvent],
    known_event_bodies: &mut HashMap<String, String>,
) -> Vec<TimelineOp> {
    let mut ops = Vec::new();

    for event in events {
        let Some(op) = timeline_op_from_event(event, TimelineInsertMode::Append) else {
            continue;
        };

        match op {
            TimelineOp::Append(item) => {
                let Some(event_id) = item.event_id else {
                    continue;
                };
                let Some(existing_body) = known_event_bodies.get(&event_id) else {
                    continue;
                };
                if existing_body == &item.body {
                    continue;
                }
                known_event_bodies.insert(event_id.clone(), item.body.clone());
                ops.push(TimelineOp::UpdateBody {
                    event_id,
                    new_body: item.body,
                });
            }
            TimelineOp::UpdateBody { event_id, new_body } => {
                let Some(existing_body) = known_event_bodies.get(&event_id) else {
                    continue;
                };
                if existing_body == &new_body {
                    continue;
                }
                known_event_bodies.insert(event_id.clone(), new_body.clone());
                ops.push(TimelineOp::UpdateBody { event_id, new_body });
            }
            TimelineOp::Remove { event_id } => {
                if known_event_bodies.remove(&event_id).is_some() {
                    ops.push(TimelineOp::Remove { event_id });
                }
            }
            TimelineOp::Prepend(_) | TimelineOp::Clear => {}
        }
    }

    ops
}

fn timeline_item_from_event(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
) -> Option<TimelineItem> {
    let event_type = event_type(event)?;
    let raw = event.raw();

    let sender = raw.get_field::<String>("sender").ok().flatten()?;
    let timestamp_ms = event
        .timestamp_raw()
        .map(|ts| u64::from(ts.get()))
        .unwrap_or(0);

    if event_type == ROOM_ENCRYPTED_EVENT_TYPE {
        return Some(TimelineItem {
            event_id: event.event_id().map(|event_id| event_id.to_string()),
            sender,
            body: ENCRYPTED_TIMELINE_PLACEHOLDER.to_owned(),
            timestamp_ms,
        });
    }

    if event_type != ROOM_MESSAGE_EVENT_TYPE {
        return None;
    }

    let content = raw
        .get_field::<serde_json::Value>("content")
        .ok()
        .flatten()?;

    if content
        .get("m.relates_to")
        .and_then(|value| value.get("rel_type"))
        .and_then(|value| value.as_str())
        == Some(REL_TYPE_REPLACE)
    {
        return None;
    }

    let body = extract_basic_message_body(&content)?;
    Some(TimelineItem {
        event_id: event.event_id().map(|event_id| event_id.to_string()),
        sender,
        body,
        timestamp_ms,
    })
}

fn extract_basic_message_body(content: &serde_json::Value) -> Option<String> {
    let body = content.get("body").and_then(|value| value.as_str())?;
    let msgtype = content
        .get("msgtype")
        .and_then(|value| value.as_str())
        .unwrap_or(MSGTYPE_TEXT);

    match msgtype {
        MSGTYPE_TEXT | MSGTYPE_NOTICE | MSGTYPE_EMOTE => Some(body.to_owned()),
        _ => None,
    }
}

fn apply_timeline_ops_lenient(buffer: &mut TimelineBuffer, ops: &[TimelineOp]) {
    for op in ops {
        let result = buffer.apply_ops(std::slice::from_ref(op));
        if let Err(TimelineMergeError::MissingEvent(_)) = result {
            if matches!(
                op,
                TimelineOp::UpdateBody { .. } | TimelineOp::Remove { .. }
            ) {
                continue;
            }
        }
    }
}

fn collect_room_summaries(client: &Client) -> Vec<RoomSummary> {
    let mut rooms: Vec<RoomSummary> = client
        .rooms()
        .into_iter()
        .filter_map(|room| {
            let membership = match room.state() {
                RoomState::Joined => RoomMembership::Joined,
                RoomState::Invited => RoomMembership::Invited,
                _ => return None,
            };
            let unread = room.unread_notification_counts();
            Some(RoomSummary {
                room_id: room.room_id().to_string(),
                name: room.name(),
                unread_notifications: unread.notification_count,
                highlight_count: unread.highlight_count,
                is_direct: room.direct_targets_length() > 0,
                membership,
            })
        })
        .collect();

    rooms.sort_by(|a, b| a.room_id.cmp(&b.room_id));
    rooms
}

fn map_backup_state(state: MatrixBackupState) -> KeyBackupState {
    match state {
        MatrixBackupState::Unknown => KeyBackupState::Unknown,
        MatrixBackupState::Creating => KeyBackupState::Creating,
        MatrixBackupState::Enabling => KeyBackupState::Enabling,
        MatrixBackupState::Resuming => KeyBackupState::Resuming,
        MatrixBackupState::Enabled => KeyBackupState::Enabled,
        MatrixBackupState::Downloading => KeyBackupState::Downloading,
        MatrixBackupState::Disabling => KeyBackupState::Disabling,
    }
}

fn map_recovery_state(state: MatrixRecoveryState) -> CoreRecoveryState {
    match state {
        MatrixRecoveryState::Unknown => CoreRecoveryState::Unknown,
        MatrixRecoveryState::Enabled => CoreRecoveryState::Enabled,
        MatrixRecoveryState::Disabled => CoreRecoveryState::Disabled,
        MatrixRecoveryState::Incomplete => CoreRecoveryState::Incomplete,
    }
}

fn is_recoverable_sync_error(err: &BackendError) -> bool {
    matches!(
        err.category,
        BackendErrorCategory::Network | BackendErrorCategory::RateLimited
    )
}

fn map_secret_store_error(operation: &str, account: &str, err: SecretStoreError) -> BackendError {
    match err {
        SecretStoreError::NotFound => BackendError::new(
            BackendErrorCategory::Config,
            "secret_not_found",
            format!("secret missing for '{account}' during {operation}"),
        ),
        SecretStoreError::Unavailable(message) => BackendError::new(
            BackendErrorCategory::Storage,
            "secret_store_unavailable",
            format!("secret store unavailable during {operation}: {message}"),
        ),
        SecretStoreError::Backend(message) => BackendError::new(
            BackendErrorCategory::Storage,
            "secret_store_error",
            format!("secret store backend error during {operation}: {message}"),
        ),
    }
}

fn map_matrix_http_error(err: HttpError) -> BackendError {
    if let Some(client_err) = err.as_client_api_error() {
        let status = client_err.status_code.as_u16();
        let mut mapped = BackendError::new(
            classify_http_status(status),
            "matrix_http_error",
            client_err.to_string(),
        );

        if let Some(ErrorKind::LimitExceeded { retry_after }) = client_err.error_kind()
            && let Some(RetryAfter::Delay(delay)) = retry_after
        {
            mapped = mapped.with_retry_after(*delay);
        }

        mapped
    } else {
        BackendError::new(
            BackendErrorCategory::Network,
            "matrix_http_error",
            err.to_string(),
        )
    }
}

fn map_edit_error(err: matrix_sdk::room::edit::EditError) -> BackendError {
    use matrix_sdk::room::edit::EditError;

    match err {
        EditError::StateEvent | EditError::NotAuthor | EditError::IncompatibleEditType { .. } => {
            BackendError::new(
                BackendErrorCategory::Config,
                "edit_invalid",
                err.to_string(),
            )
        }
        EditError::Deserialize(_) => BackendError::new(
            BackendErrorCategory::Serialization,
            "edit_deserialize_error",
            err.to_string(),
        ),
        EditError::Fetch(_) => BackendError::new(
            BackendErrorCategory::Network,
            "edit_fetch_error",
            err.to_string(),
        ),
    }
}

fn map_recovery_error(err: MatrixRecoveryError) -> BackendError {
    match err {
        MatrixRecoveryError::BackupExistsOnServer => BackendError::new(
            BackendErrorCategory::Config,
            "recovery_backup_exists",
            "a backup already exists on the homeserver",
        ),
        MatrixRecoveryError::Sdk(err) => map_matrix_error(err),
        MatrixRecoveryError::SecretStorage(err) => BackendError::new(
            BackendErrorCategory::Crypto,
            "secret_storage_error",
            err.to_string(),
        ),
    }
}

fn map_matrix_error(err: matrix_sdk::Error) -> BackendError {
    use matrix_sdk::Error;

    let message = err.to_string();
    match err {
        Error::Http(http_err) => map_matrix_http_error(*http_err),
        Error::AuthenticationRequired => {
            BackendError::new(BackendErrorCategory::Auth, "auth_required", message)
        }
        Error::NoOlmMachine => BackendError::new(
            BackendErrorCategory::Crypto,
            "crypto_machine_unavailable",
            format!(
                "{message}. Local crypto state is unavailable; reset the data dir for this account."
            ),
        ),
        Error::BadCryptoStoreState => BackendError::new(
            BackendErrorCategory::Crypto,
            "crypto_store_state_error",
            format!("{message}. Local crypto store state is invalid for the current session."),
        ),
        Error::CryptoStoreError(_)
        | Error::StateStore(_)
        | Error::EventCacheStore(_)
        | Error::MediaStore(_)
        | Error::Io(_)
        | Error::CrossProcessLockError(_) => {
            if is_store_account_mismatch_message(&message) {
                BackendError::new(
                    BackendErrorCategory::Storage,
                    "crypto_store_account_mismatch",
                    format!(
                        "{message}. The configured data dir appears to belong to a different Matrix account."
                    ),
                )
            } else {
                BackendError::new(BackendErrorCategory::Storage, "storage_error", message)
            }
        }
        Error::SerdeJson(_) => BackendError::new(
            BackendErrorCategory::Serialization,
            "serde_json_error",
            message,
        ),
        _ => BackendError::new(BackendErrorCategory::Internal, "matrix_error", message),
    }
}

fn map_client_build_error(err: ClientBuildError) -> BackendError {
    BackendError::new(
        BackendErrorCategory::Config,
        "client_build_error",
        err.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use matrix_sdk::{
        SessionMeta, SessionTokens,
        ruma::{device_id, events::AnySyncTimelineEvent, serde::Raw, user_id},
    };
    use std::{env, fs, path::PathBuf, sync::Mutex, time::Duration};
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    fn make_sync_timeline_event(
        event_id: &str,
        sender: &str,
        body: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        make_sync_timeline_event_with_msgtype(
            event_id,
            sender,
            MSGTYPE_TEXT,
            body,
            origin_server_ts,
        )
    }

    fn make_sync_timeline_event_with_msgtype(
        event_id: &str,
        sender: &str,
        msgtype: &str,
        body: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "content": {
                "msgtype": msgtype,
                "body": body
            }
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("timeline event json should parse");
        TimelineEvent::from_plaintext(raw)
    }

    fn make_sync_state_event(
        event_id: &str,
        sender: &str,
        event_type: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": event_type,
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "state_key": "",
            "content": {}
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("state event json should parse");
        TimelineEvent::from_plaintext(raw)
    }

    fn make_sync_encrypted_event(
        event_id: &str,
        sender: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": "m.room.encrypted",
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "placeholder"
            }
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("encrypted event json should parse");
        TimelineEvent::from_plaintext(raw)
    }

    fn make_sync_edit_event(
        edit_event_id: &str,
        sender: &str,
        target_event_id: &str,
        new_body: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": "m.room.message",
            "event_id": edit_event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "content": {
                "msgtype": "m.text",
                "body": format!("* {}", new_body),
                "m.new_content": {
                    "msgtype": "m.text",
                    "body": new_body
                },
                "m.relates_to": {
                    "rel_type": "m.replace",
                    "event_id": target_event_id
                }
            }
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("edit event json should parse");
        TimelineEvent::from_plaintext(raw)
    }

    fn make_sync_redaction_event(
        redaction_event_id: &str,
        sender: &str,
        target_event_id: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": "m.room.redaction",
            "event_id": redaction_event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "redacts": target_event_id,
            "content": {
                "reason": "cleanup"
            }
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("redaction event json should parse");
        TimelineEvent::from_plaintext(raw)
    }

    fn make_timeline_item(event_id: &str, body: &str) -> TimelineItem {
        TimelineItem {
            event_id: Some(event_id.to_owned()),
            sender: "@alice:example.org".to_owned(),
            body: body.to_owned(),
            timestamp_ms: 1,
        }
    }

    fn mock_session() -> MatrixSession {
        MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@mock:example.org").to_owned(),
                device_id: device_id!("MOCKDEVICE").to_owned(),
            },
            tokens: SessionTokens {
                access_token: "mock-access-token".to_owned(),
                refresh_token: None,
            },
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!("pikachat-{prefix}-{}", Uuid::new_v4()))
    }

    struct MockRuntimeBackend {
        calls: Mutex<Vec<String>>,
        login_device_ids: Mutex<Vec<Option<String>>>,
        start_sync_timeouts: Mutex<Vec<Option<Duration>>>,
        open_room_limits: Mutex<Vec<u16>>,
        paginate_back_limits: Mutex<Vec<u16>>,
        enable_recovery_inputs: Mutex<Vec<(Option<String>, bool)>>,
        reset_recovery_inputs: Mutex<Vec<(Option<String>, bool)>>,
        recover_secret_inputs: Mutex<Vec<String>>,
        rooms: Vec<RoomSummary>,
        login_result: Result<(), BackendError>,
        login_delay: Option<Duration>,
        logout_result: Result<(), BackendError>,
        start_sync_result: Result<(), BackendError>,
        stop_sync_result: Result<(), BackendError>,
        open_room_result: Result<(Vec<TimelineOp>, Option<String>), BackendError>,
        send_message_result: Result<String, BackendError>,
        upload_media_result: Result<String, BackendError>,
        download_media_result: Result<Vec<u8>, BackendError>,
        recovery_status_result: Result<RecoveryStatus, BackendError>,
        enable_recovery_result: Result<String, BackendError>,
        reset_recovery_result: Result<String, BackendError>,
        recover_secrets_result: Result<(), BackendError>,
        accept_room_invite_result: Result<(), BackendError>,
        reject_room_invite_result: Result<(), BackendError>,
        session: Option<MatrixSession>,
    }

    impl MockRuntimeBackend {
        fn new(rooms: Vec<RoomSummary>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                login_device_ids: Mutex::new(Vec::new()),
                start_sync_timeouts: Mutex::new(Vec::new()),
                open_room_limits: Mutex::new(Vec::new()),
                paginate_back_limits: Mutex::new(Vec::new()),
                enable_recovery_inputs: Mutex::new(Vec::new()),
                reset_recovery_inputs: Mutex::new(Vec::new()),
                recover_secret_inputs: Mutex::new(Vec::new()),
                rooms,
                login_result: Ok(()),
                login_delay: None,
                logout_result: Ok(()),
                start_sync_result: Ok(()),
                stop_sync_result: Ok(()),
                open_room_result: Ok((Vec::new(), None)),
                send_message_result: Ok("$mock-event:example.org".to_owned()),
                upload_media_result: Ok("mxc://example.org/mock-media".to_owned()),
                download_media_result: Ok(b"mock-media".to_vec()),
                recovery_status_result: Ok(RecoveryStatus {
                    backup_state: KeyBackupState::Unknown,
                    backup_enabled: false,
                    backup_exists_on_server: false,
                    recovery_state: CoreRecoveryState::Unknown,
                }),
                enable_recovery_result: Ok("mock recovery key".to_owned()),
                reset_recovery_result: Ok("mock reset recovery key".to_owned()),
                recover_secrets_result: Ok(()),
                accept_room_invite_result: Ok(()),
                reject_room_invite_result: Ok(()),
                session: Some(mock_session()),
            }
        }

        fn record_call(&self, call: &str) {
            self.calls
                .lock()
                .expect("call log lock")
                .push(call.to_owned());
        }

        fn calls_snapshot(&self) -> Vec<String> {
            self.calls.lock().expect("call log lock").clone()
        }

        fn login_device_ids_snapshot(&self) -> Vec<Option<String>> {
            self.login_device_ids
                .lock()
                .expect("login-device-ids lock")
                .clone()
        }

        fn start_sync_timeouts_snapshot(&self) -> Vec<Option<Duration>> {
            self.start_sync_timeouts
                .lock()
                .expect("start-sync-timeouts lock")
                .clone()
        }

        fn open_room_limits_snapshot(&self) -> Vec<u16> {
            self.open_room_limits
                .lock()
                .expect("open-room-limits lock")
                .clone()
        }

        fn paginate_back_limits_snapshot(&self) -> Vec<u16> {
            self.paginate_back_limits
                .lock()
                .expect("paginate-back-limits lock")
                .clone()
        }

        fn enable_recovery_inputs_snapshot(&self) -> Vec<(Option<String>, bool)> {
            self.enable_recovery_inputs
                .lock()
                .expect("enable-recovery-inputs lock")
                .clone()
        }

        fn reset_recovery_inputs_snapshot(&self) -> Vec<(Option<String>, bool)> {
            self.reset_recovery_inputs
                .lock()
                .expect("reset-recovery-inputs lock")
                .clone()
        }

        fn recover_secret_inputs_snapshot(&self) -> Vec<String> {
            self.recover_secret_inputs
                .lock()
                .expect("recover-secret-inputs lock")
                .clone()
        }

        fn with_login_result(mut self, login_result: Result<(), BackendError>) -> Self {
            self.login_result = login_result;
            self
        }

        fn with_login_delay(mut self, login_delay: Duration) -> Self {
            self.login_delay = Some(login_delay);
            self
        }

        fn with_session(mut self, session: Option<MatrixSession>) -> Self {
            self.session = session;
            self
        }

        fn with_logout_result(mut self, logout_result: Result<(), BackendError>) -> Self {
            self.logout_result = logout_result;
            self
        }

        fn with_send_message_result(
            mut self,
            send_message_result: Result<String, BackendError>,
        ) -> Self {
            self.send_message_result = send_message_result;
            self
        }

        fn with_start_sync_result(mut self, start_sync_result: Result<(), BackendError>) -> Self {
            self.start_sync_result = start_sync_result;
            self
        }

        fn with_open_room_result(
            mut self,
            open_room_result: Result<(Vec<TimelineOp>, Option<String>), BackendError>,
        ) -> Self {
            self.open_room_result = open_room_result;
            self
        }

        fn with_upload_media_result(
            mut self,
            upload_media_result: Result<String, BackendError>,
        ) -> Self {
            self.upload_media_result = upload_media_result;
            self
        }

        fn with_download_media_result(
            mut self,
            download_media_result: Result<Vec<u8>, BackendError>,
        ) -> Self {
            self.download_media_result = download_media_result;
            self
        }

        fn with_recovery_status_result(
            mut self,
            recovery_status_result: Result<RecoveryStatus, BackendError>,
        ) -> Self {
            self.recovery_status_result = recovery_status_result;
            self
        }

        fn with_enable_recovery_result(
            mut self,
            enable_recovery_result: Result<String, BackendError>,
        ) -> Self {
            self.enable_recovery_result = enable_recovery_result;
            self
        }

        fn with_reset_recovery_result(
            mut self,
            reset_recovery_result: Result<String, BackendError>,
        ) -> Self {
            self.reset_recovery_result = reset_recovery_result;
            self
        }

        fn with_recover_secrets_result(
            mut self,
            recover_secrets_result: Result<(), BackendError>,
        ) -> Self {
            self.recover_secrets_result = recover_secrets_result;
            self
        }

        fn with_reject_room_invite_result(
            mut self,
            reject_room_invite_result: Result<(), BackendError>,
        ) -> Self {
            self.reject_room_invite_result = reject_room_invite_result;
            self
        }
    }

    #[async_trait]
    impl RuntimeBackend for MockRuntimeBackend {
        fn session(&self) -> Option<MatrixSession> {
            self.session.clone()
        }

        async fn restore_session(&self, _session: MatrixSession) -> Result<(), BackendError> {
            self.record_call("restore_session");
            Ok(())
        }

        async fn logout(&self) -> Result<(), BackendError> {
            self.record_call("logout");
            self.logout_result.clone()
        }

        fn list_rooms(&self) -> Vec<RoomSummary> {
            self.record_call("list_rooms");
            self.rooms.clone()
        }

        async fn login_password(
            &self,
            _user_id_or_localpart: &str,
            _password: &str,
            _device_display_name: &str,
            preferred_device_id: Option<&str>,
        ) -> Result<(), BackendError> {
            self.record_call("login_password");
            self.login_device_ids
                .lock()
                .expect("login-device-ids lock")
                .push(preferred_device_id.map(ToOwned::to_owned));
            if let Some(delay) = self.login_delay {
                tokio::time::sleep(delay).await;
            }
            self.login_result.clone()
        }

        async fn start_sync(
            &self,
            _event_tx: broadcast::Sender<BackendEvent>,
            sync_request_timeout: Option<Duration>,
        ) -> Result<(), BackendError> {
            self.record_call("start_sync");
            self.start_sync_timeouts
                .lock()
                .expect("start-sync-timeouts lock")
                .push(sync_request_timeout);
            self.start_sync_result.clone()
        }

        async fn stop_sync(&self) -> Result<(), BackendError> {
            self.record_call("stop_sync");
            self.stop_sync_result.clone()
        }

        async fn send_message(
            &self,
            _room_id: &str,
            _body: &str,
            _msgtype: MessageType,
        ) -> Result<String, BackendError> {
            self.record_call("send_message");
            self.send_message_result.clone()
        }

        async fn edit_message(
            &self,
            _room_id: &str,
            _target_event_id: &str,
            _new_body: &str,
        ) -> Result<String, BackendError> {
            self.record_call("edit_message");
            Ok("$mock-edit:example.org".to_owned())
        }

        async fn redact_message(
            &self,
            _room_id: &str,
            _target_event_id: &str,
            _reason: Option<String>,
        ) -> Result<(), BackendError> {
            self.record_call("redact_message");
            Ok(())
        }

        async fn open_room(
            &self,
            _room_id: &str,
            limit: u16,
        ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
            self.record_call("open_room");
            self.open_room_limits
                .lock()
                .expect("open-room-limits lock")
                .push(limit);
            self.open_room_result.clone()
        }

        async fn paginate_back(
            &self,
            _room_id: &str,
            _from_token: Option<&str>,
            limit: u16,
        ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
            self.record_call("paginate_back");
            self.paginate_back_limits
                .lock()
                .expect("paginate-back-limits lock")
                .push(limit);
            Ok((Vec::new(), None))
        }

        async fn send_dm_text(&self, _user_id: &str, _body: &str) -> Result<String, BackendError> {
            self.record_call("send_dm_text");
            Ok("$mock-dm:example.org".to_owned())
        }

        async fn accept_room_invite(&self, _room_id: &str) -> Result<(), BackendError> {
            self.record_call("accept_room_invite");
            self.accept_room_invite_result.clone()
        }

        async fn reject_room_invite(&self, _room_id: &str) -> Result<(), BackendError> {
            self.record_call("reject_room_invite");
            self.reject_room_invite_result.clone()
        }

        async fn upload_media(
            &self,
            _content_type: &str,
            _data: Vec<u8>,
        ) -> Result<String, BackendError> {
            self.record_call("upload_media");
            self.upload_media_result.clone()
        }

        async fn download_media(&self, _source: &str) -> Result<Vec<u8>, BackendError> {
            self.record_call("download_media");
            self.download_media_result.clone()
        }

        async fn get_recovery_status(&self) -> Result<RecoveryStatus, BackendError> {
            self.record_call("get_recovery_status");
            self.recovery_status_result.clone()
        }

        async fn enable_recovery(
            &self,
            passphrase: Option<&str>,
            wait_for_backups_to_upload: bool,
        ) -> Result<String, BackendError> {
            self.record_call("enable_recovery");
            self.enable_recovery_inputs
                .lock()
                .expect("enable-recovery-inputs lock")
                .push((
                    passphrase.map(ToOwned::to_owned),
                    wait_for_backups_to_upload,
                ));
            self.enable_recovery_result.clone()
        }

        async fn recover_secrets(&self, recovery_key: &str) -> Result<(), BackendError> {
            self.record_call("recover_secrets");
            self.recover_secret_inputs
                .lock()
                .expect("recover-secret-inputs lock")
                .push(recovery_key.to_owned());
            self.recover_secrets_result.clone()
        }

        async fn reset_recovery(
            &self,
            passphrase: Option<&str>,
            wait_for_backups_to_upload: bool,
        ) -> Result<String, BackendError> {
            self.record_call("reset_recovery");
            self.reset_recovery_inputs
                .lock()
                .expect("reset-recovery-inputs lock")
                .push((
                    passphrase.map(ToOwned::to_owned),
                    wait_for_backups_to_upload,
                ));
            self.reset_recovery_result.clone()
        }
    }

    async fn recv_matching_event<F>(events: &mut EventStream, mut predicate: F) -> BackendEvent
    where
        F: FnMut(&BackendEvent) -> bool,
    {
        loop {
            let event = timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("event timeout")
                .expect("event receive");
            if predicate(&event) {
                return event;
            }
        }
    }

    #[test]
    fn rejects_invalid_room_id() {
        let err = parse_room_id("not-a-room-id").expect_err("invalid room id must fail");
        assert_eq!(err.code, "invalid_room_id");
    }

    #[test]
    fn rejects_invalid_event_id() {
        let err = parse_event_id("not-an-event-id").expect_err("invalid event id must fail");
        assert_eq!(err.code, "invalid_event_id");
    }

    #[test]
    fn rejects_invalid_user_id() {
        let err = parse_user_id("not-a-user").expect_err("invalid user id must fail");
        assert_eq!(err.code, "invalid_user_id");
    }

    #[test]
    fn rejects_invalid_mxc_uri() {
        let err =
            parse_mxc_uri("https://example.org/not-mxc").expect_err("invalid mxc uri must fail");
        assert_eq!(err.code, "invalid_mxc_uri");
    }

    #[test]
    fn rejects_invalid_media_content_type() {
        let err = parse_media_content_type("bad-content-type")
            .expect_err("invalid media content type must fail");
        assert_eq!(err.code, "invalid_media_content_type");
    }

    #[test]
    fn sync_timeline_ops_append_events_in_order() {
        let first = make_sync_timeline_event("$e1:example.org", "@alice:example.org", "first", 1);
        let second = make_sync_timeline_event("$e2:example.org", "@bob:example.org", "second", 2);
        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[first, second], false, &mut known_event_bodies);

        assert_eq!(ops.len(), 2);
        match &ops[0] {
            TimelineOp::Append(item) => {
                assert_eq!(item.event_id.as_deref(), Some("$e1:example.org"));
                assert_eq!(item.sender, "@alice:example.org");
                assert_eq!(item.body, "first");
                assert_eq!(item.timestamp_ms, 1);
            }
            other => panic!("unexpected op: {other:?}"),
        }
        match &ops[1] {
            TimelineOp::Append(item) => {
                assert_eq!(item.event_id.as_deref(), Some("$e2:example.org"));
                assert_eq!(item.sender, "@bob:example.org");
                assert_eq!(item.body, "second");
                assert_eq!(item.timestamp_ms, 2);
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_insert_clear_when_limited() {
        let event = make_sync_timeline_event("$e3:example.org", "@alice:example.org", "hello", 3);
        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[event], true, &mut known_event_bodies);
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], TimelineOp::Clear));
        assert!(matches!(ops[1], TimelineOp::Append(_)));
    }

    #[test]
    fn sync_timeline_ops_map_edit_events_to_update_body() {
        let edit = make_sync_edit_event(
            "$edit1:example.org",
            "@alice:example.org",
            "$target1:example.org",
            "edited body",
            4,
        );

        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[edit], false, &mut known_event_bodies);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::UpdateBody { event_id, new_body } => {
                assert_eq!(event_id, "$target1:example.org");
                assert_eq!(new_body, "edited body");
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_map_redaction_events_to_remove() {
        let redaction = make_sync_redaction_event(
            "$redact1:example.org",
            "@alice:example.org",
            "$target2:example.org",
            5,
        );

        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[redaction], false, &mut known_event_bodies);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::Remove { event_id } => {
                assert_eq!(event_id, "$target2:example.org");
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_ignore_non_message_state_events() {
        let membership = make_sync_state_event(
            "$member1:example.org",
            "@alice:example.org",
            "m.room.member",
            7,
        );
        let text = make_sync_timeline_event("$e4:example.org", "@bob:example.org", "hello", 8);

        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[membership, text], false, &mut known_event_bodies);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::Append(item) => {
                assert_eq!(item.event_id.as_deref(), Some("$e4:example.org"));
                assert_eq!(item.body, "hello");
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_ignore_non_text_message_types() {
        let image = make_sync_timeline_event_with_msgtype(
            "$img1:example.org",
            "@alice:example.org",
            "m.image",
            "screenshot.png",
            9,
        );
        let notice = make_sync_timeline_event_with_msgtype(
            "$notice1:example.org",
            "@alice:example.org",
            MSGTYPE_NOTICE,
            "Server maintenance in 10 minutes.",
            10,
        );

        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[image, notice], false, &mut known_event_bodies);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::Append(item) => {
                assert_eq!(item.event_id.as_deref(), Some("$notice1:example.org"));
                assert_eq!(item.body, "Server maintenance in 10 minutes.");
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_surface_encrypted_events_as_placeholder() {
        let encrypted = make_sync_encrypted_event("$enc1:example.org", "@alice:example.org", 11);
        let text = make_sync_timeline_event("$e5:example.org", "@bob:example.org", "hello", 12);

        let mut known_event_bodies = HashMap::new();
        let ops = timeline_ops_for_sync_events(&[encrypted, text], false, &mut known_event_bodies);
        assert_eq!(ops.len(), 2);
        match &ops[0] {
            TimelineOp::Append(item) => {
                assert_eq!(item.event_id.as_deref(), Some("$enc1:example.org"));
                assert_eq!(item.body, ENCRYPTED_TIMELINE_PLACEHOLDER);
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_convert_placeholder_to_update_when_body_changes() {
        let event_id = "$enc2:example.org";
        let encrypted = make_sync_encrypted_event(event_id, "@alice:example.org", 11);
        let decrypted =
            make_sync_timeline_event(event_id, "@alice:example.org", "decrypted body", 11);

        let mut known_event_bodies = HashMap::new();
        let first_ops = timeline_ops_for_sync_events(&[encrypted], false, &mut known_event_bodies);
        assert_eq!(first_ops.len(), 1);
        assert!(matches!(first_ops[0], TimelineOp::Append(_)));

        let second_ops = timeline_ops_for_sync_events(&[decrypted], false, &mut known_event_bodies);
        assert_eq!(second_ops.len(), 1);
        match &second_ops[0] {
            TimelineOp::UpdateBody { event_id, new_body } => {
                assert_eq!(event_id, "$enc2:example.org");
                assert_eq!(new_body, "decrypted body");
            }
            other => panic!("unexpected op: {other:?}"),
        }
    }

    #[test]
    fn sync_timeline_ops_skip_duplicate_appends_for_same_event_body() {
        let event = make_sync_timeline_event("$dup1:example.org", "@alice:example.org", "hello", 1);
        let mut known_event_bodies = HashMap::new();

        let first_ops = timeline_ops_for_sync_events(
            std::slice::from_ref(&event),
            false,
            &mut known_event_bodies,
        );
        assert_eq!(first_ops.len(), 1);
        assert!(matches!(first_ops[0], TimelineOp::Append(_)));

        let second_ops = timeline_ops_for_sync_events(&[event], false, &mut known_event_bodies);
        assert!(second_ops.is_empty());
    }

    #[test]
    fn refresh_updates_only_known_event_bodies() {
        let mut known_event_bodies = HashMap::from([(
            "$enc3:example.org".to_owned(),
            ENCRYPTED_TIMELINE_PLACEHOLDER.to_owned(),
        )]);
        let unknown = make_sync_timeline_event("$new:example.org", "@alice:example.org", "new", 1);
        let decrypted_known =
            make_sync_timeline_event("$enc3:example.org", "@alice:example.org", "decrypted", 2);

        let ops = timeline_update_ops_for_refresh_events(
            &[unknown, decrypted_known],
            &mut known_event_bodies,
        );
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::UpdateBody { event_id, new_body } => {
                assert_eq!(event_id, "$enc3:example.org");
                assert_eq!(new_body, "decrypted");
            }
            other => panic!("unexpected op: {other:?}"),
        }
        assert_eq!(
            known_event_bodies
                .get("$enc3:example.org")
                .map(String::as_str),
            Some("decrypted")
        );
        assert!(!known_event_bodies.contains_key("$new:example.org"));
    }

    #[test]
    fn refresh_emits_remove_when_known_event_is_redacted() {
        let mut known_event_bodies =
            HashMap::from([("$target:example.org".to_owned(), "hello".to_owned())]);
        let redaction = make_sync_redaction_event(
            "$redact:example.org",
            "@alice:example.org",
            "$target:example.org",
            3,
        );

        let ops = timeline_update_ops_for_refresh_events(&[redaction], &mut known_event_bodies);
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            TimelineOp::Remove { event_id } => {
                assert_eq!(event_id, "$target:example.org");
            }
            other => panic!("unexpected op: {other:?}"),
        }
        assert!(!known_event_bodies.contains_key("$target:example.org"));
    }

    #[test]
    fn keyring_account_keys_are_stable() {
        assert_eq!(
            store_passphrase_account_for_homeserver("https://matrix.example.org"),
            "store-passphrase:https://matrix.example.org"
        );
        assert_eq!(
            session_account_for_homeserver("https://matrix.example.org"),
            "matrix-session:https://matrix.example.org"
        );
    }

    #[test]
    fn store_passphrase_file_roundtrip_trims_trailing_whitespace() {
        let data_dir = unique_temp_dir("passphrase-roundtrip");
        let path = store_passphrase_file_path(&data_dir);

        persist_store_passphrase_to_file(&path, "passphrase-value\n")
            .expect("persist passphrase file should work");
        let loaded =
            load_store_passphrase_from_file(&path).expect("loading passphrase file should work");
        assert_eq!(loaded.as_deref(), Some("passphrase-value"));

        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn store_passphrase_file_missing_returns_none() {
        let data_dir = unique_temp_dir("passphrase-missing");
        let path = store_passphrase_file_path(&data_dir);

        let loaded = load_store_passphrase_from_file(&path)
            .expect("missing passphrase file should not fail");
        assert_eq!(loaded, None);
    }

    #[test]
    fn store_passphrase_file_empty_returns_storage_error() {
        let data_dir = unique_temp_dir("passphrase-empty");
        let path = store_passphrase_file_path(&data_dir);
        fs::create_dir_all(&data_dir).expect("test data dir should be creatable");
        fs::write(&path, "\n").expect("test file should be writable");

        let err = load_store_passphrase_from_file(&path)
            .expect_err("empty passphrase file must fail with storage error");
        assert_eq!(err.code, "store_passphrase_file_empty");

        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn device_id_hint_file_roundtrip_trims_trailing_whitespace() {
        let data_dir = unique_temp_dir("device-id-hint-roundtrip");
        let path = device_id_hint_file_path(&data_dir);

        persist_device_id_hint_to_file(&path, "ABCDEVICE\n")
            .expect("persist device-id hint file should work");
        let loaded =
            load_device_id_hint_from_file(&path).expect("loading device-id hint should work");
        assert_eq!(loaded.as_deref(), Some("ABCDEVICE"));

        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn device_id_hint_file_path_is_sibling_of_store_dir() {
        let data_dir = unique_temp_dir("device-id-hint-path");
        let path = device_id_hint_file_path(&data_dir);

        assert_eq!(path.parent(), data_dir.parent());
        assert_ne!(path, data_dir.join(".pikachat-device-id"));
        assert!(!path.starts_with(&data_dir));

        let _ = fs::remove_dir_all(&data_dir);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn device_id_hint_persists_even_if_store_dir_is_deleted() {
        let data_dir = unique_temp_dir("device-id-hint-survives-store-reset");
        fs::create_dir_all(&data_dir).expect("test store dir should be creatable");
        let path = device_id_hint_file_path(&data_dir);

        persist_device_id_hint_to_file(&path, "STABLEDEVICE")
            .expect("persist device-id hint file should work");
        fs::remove_dir_all(&data_dir).expect("store dir should be removable");

        let loaded =
            load_device_id_hint_from_file(&path).expect("loading device-id hint should work");
        assert_eq!(loaded.as_deref(), Some("STABLEDEVICE"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn device_id_hint_file_missing_returns_none() {
        let data_dir = unique_temp_dir("device-id-hint-missing");
        let path = device_id_hint_file_path(&data_dir);
        let loaded =
            load_device_id_hint_from_file(&path).expect("missing device-id hint should not fail");
        assert_eq!(loaded, None);
    }

    #[test]
    fn legacy_device_id_hint_file_path_stays_inside_store_dir() {
        let data_dir = unique_temp_dir("device-id-legacy-path");
        let path = legacy_device_id_hint_file_path(&data_dir);
        assert_eq!(path, data_dir.join(".pikachat-device-id"));
        let _ = fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn runtime_tuning_defaults_when_init_config_is_absent() {
        let tuning = runtime_tuning_from_init_config(None).expect("defaults should apply");
        assert_eq!(tuning.sync_request_timeout, None);
        assert_eq!(tuning.open_room_limit, DEFAULT_OPEN_ROOM_LIMIT);
        assert_eq!(tuning.pagination_limit_cap, DEFAULT_PAGINATION_LIMIT_CAP);
    }

    #[test]
    fn runtime_tuning_rejects_zero_values() {
        let err = runtime_tuning_from_init_config(Some(BackendInitConfig {
            sync_request_timeout_ms: Some(0),
            default_open_room_limit: None,
            pagination_limit_cap: None,
        }))
        .expect_err("zero timeout must fail");
        assert_eq!(err.code, "invalid_init_config");

        let err = runtime_tuning_from_init_config(Some(BackendInitConfig {
            sync_request_timeout_ms: None,
            default_open_room_limit: Some(0),
            pagination_limit_cap: None,
        }))
        .expect_err("zero open-room limit must fail");
        assert_eq!(err.code, "invalid_init_config");

        let err = runtime_tuning_from_init_config(Some(BackendInitConfig {
            sync_request_timeout_ms: None,
            default_open_room_limit: None,
            pagination_limit_cap: Some(0),
        }))
        .expect_err("zero pagination cap must fail");
        assert_eq!(err.code, "invalid_init_config");
    }

    #[test]
    fn retry_policy_defaults_are_sane_for_sync_loop() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.base_delay_ms(), 500);
        assert_eq!(policy.max_delay_ms(), 30_000);
    }

    #[test]
    fn recoverable_sync_error_categories_are_limited_to_network_and_rate_limit() {
        let network = BackendError::new(BackendErrorCategory::Network, "n", "network");
        let rate = BackendError::new(BackendErrorCategory::RateLimited, "r", "rate");
        let auth = BackendError::new(BackendErrorCategory::Auth, "a", "auth");

        assert!(is_recoverable_sync_error(&network));
        assert!(is_recoverable_sync_error(&rate));
        assert!(!is_recoverable_sync_error(&auth));
    }

    #[test]
    fn sync_loop_retry_hint_uses_error_retry_after() {
        let err = BackendError::new(BackendErrorCategory::RateLimited, "rate", "wait")
            .with_retry_after(Duration::from_secs(7));
        let policy = RetryPolicy::default();
        let delay = policy.delay_for_attempt(0, err.retry_after_ms);
        assert_eq!(delay, Duration::from_secs(7));
    }

    #[test]
    fn store_account_mismatch_message_detection_matches_known_patterns() {
        assert!(is_store_account_mismatch_message(
            "the account in the store doesn't match the account in the constructor"
        ));
        assert!(is_store_account_mismatch_message(
            "data dir belongs to a different account"
        ));
        assert!(!is_store_account_mismatch_message("already logged in"));
    }

    #[test]
    fn store_account_mismatch_message_extracts_expected_device_id() {
        let message = "the account in the store doesn't match the account in the constructor: expected @testbot:matrix.pikipika.com:WFCAJIAVBR, got @testbot:matrix.pikipika.com:VJEMXYZVZZ";
        let parsed = expected_device_id_from_store_mismatch_message(message);
        assert_eq!(parsed.as_deref(), Some("WFCAJIAVBR"));
    }

    #[test]
    fn store_account_mismatch_message_extract_returns_none_for_unexpected_shape() {
        let parsed = expected_device_id_from_store_mismatch_message("already logged in");
        assert_eq!(parsed, None);
    }

    #[test]
    fn map_matrix_error_no_olm_machine_uses_specific_error_code() {
        let mapped = map_matrix_error(matrix_sdk::Error::NoOlmMachine);
        assert_eq!(mapped.category, BackendErrorCategory::Crypto);
        assert_eq!(mapped.code, "crypto_machine_unavailable");
    }

    #[tokio::test]
    async fn runtime_init_config_applies_sync_request_timeout_to_start_sync() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: Some(BackendInitConfig {
                    sync_request_timeout_ms: Some(12_345),
                    default_open_room_limit: None,
                    pagination_limit_cap: None,
                }),
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::StartSync)
            .await
            .expect("start sync should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Syncing
                }
            )
        })
        .await;

        assert_eq!(
            mock_backend.start_sync_timeouts_snapshot(),
            vec![Some(Duration::from_millis(12_345))]
        );
    }

    #[tokio::test]
    async fn runtime_init_config_applies_open_room_limit() {
        let room_id = "!room:example.org".to_owned();
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_open_room_result(Ok((
                vec![TimelineOp::Append(make_timeline_item(
                    "$event:example.org",
                    "hello",
                ))],
                None,
            ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: Some(BackendInitConfig {
                    sync_request_timeout_ms: None,
                    default_open_room_limit: Some(7),
                    pagination_limit_cap: None,
                }),
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::OpenRoom {
                room_id: room_id.clone(),
            })
            .await
            .expect("open room should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RoomTimelineDelta {
                    room_id: candidate_room_id,
                    ..
                } if candidate_room_id == &room_id
            )
        })
        .await;

        assert_eq!(mock_backend.open_room_limits_snapshot(), vec![7]);
    }

    #[tokio::test]
    async fn runtime_init_config_applies_pagination_limit_cap() {
        let room_id = "!room:example.org".to_owned();
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: Some(BackendInitConfig {
                    sync_request_timeout_ms: None,
                    default_open_room_limit: None,
                    pagination_limit_cap: Some(12),
                }),
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::PaginateBack {
                room_id,
                limit: 200,
            })
            .await
            .expect("paginate should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::RoomTimelineDelta { .. })
        })
        .await;

        assert_eq!(mock_backend.paginate_back_limits_snapshot(), vec![12]);
    }

    #[tokio::test]
    async fn runtime_emits_timeline_deltas_without_snapshots() {
        let room_id = "!room:example.org".to_owned();
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_open_room_result(Ok((
                vec![TimelineOp::Append(make_timeline_item(
                    "$event:example.org",
                    "hello",
                ))],
                None,
            ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend);
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::OpenRoom {
                room_id: room_id.clone(),
            })
            .await
            .expect("open room should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RoomTimelineDelta {
                    room_id: candidate_room_id,
                    ..
                } if candidate_room_id == &room_id
            )
        })
        .await;

        loop {
            match timeout(Duration::from_millis(100), events.recv()).await {
                Ok(Ok(event)) => {
                    assert!(
                        !matches!(event, BackendEvent::RoomTimelineSnapshot { .. }),
                        "runtime must remain delta-only; snapshots are adapter-level events"
                    );
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }
    }

    #[tokio::test]
    async fn runtime_emits_fatal_error_for_invalid_transition() {
        let handle = spawn_runtime();
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::StartSync)
            .await
            .expect("command should enqueue");

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event receive");

        match event {
            BackendEvent::FatalError { code, .. } => {
                assert_eq!(code, "invalid_state_transition");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_send_dm_outside_authenticated_context_emits_send_ack_failure() {
        let handle = spawn_runtime();
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::SendDmText {
                user_id: "@alice:example.org".to_owned(),
                client_txn_id: "tx-dm-1".to_owned(),
                body: "hello".to_owned(),
            })
            .await
            .expect("command should enqueue");

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event receive");

        match event {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "tx-dm-1");
                assert_eq!(ack.event_id, None);
                assert_eq!(ack.error_code.as_deref(), Some("invalid_state_transition"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_upload_media_outside_authenticated_context_emits_upload_ack_failure() {
        let handle = spawn_runtime();
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::UploadMedia {
                client_txn_id: "tx-upload-1".to_owned(),
                content_type: "text/plain".to_owned(),
                data: b"hello".to_vec(),
            })
            .await
            .expect("command should enqueue");

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event receive");

        match event {
            BackendEvent::MediaUploadAck(ack) => {
                assert_eq!(ack.client_txn_id, "tx-upload-1");
                assert_eq!(ack.content_uri, None);
                assert_eq!(ack.error_code.as_deref(), Some("invalid_state_transition"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_download_media_outside_authenticated_context_emits_download_ack_failure() {
        let handle = spawn_runtime();
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::DownloadMedia {
                client_txn_id: "tx-download-1".to_owned(),
                source: "mxc://example.org/file".to_owned(),
            })
            .await
            .expect("command should enqueue");

        let event = timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event timeout")
            .expect("event receive");

        match event {
            BackendEvent::MediaDownloadAck(ack) => {
                assert_eq!(ack.client_txn_id, "tx-download-1");
                assert_eq!(ack.source, "mxc://example.org/file");
                assert_eq!(ack.data, None);
                assert_eq!(ack.error_code.as_deref(), Some("invalid_state_transition"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn runtime_upload_and_download_media_happy_path_is_deterministic() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_upload_media_result(Ok("mxc://example.org/uploaded".to_owned()))
                .with_download_media_result(Ok(b"binary-payload".to_vec())),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::UploadMedia {
                client_txn_id: "tx-upload-ok".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                data: b"binary-payload".to_vec(),
            })
            .await
            .expect("upload should enqueue");
        let upload_ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::MediaUploadAck(MediaUploadAck {
                    client_txn_id,
                    content_uri: Some(_),
                    error_code: None,
                }) if client_txn_id == "tx-upload-ok"
            )
        })
        .await;
        match upload_ack {
            BackendEvent::MediaUploadAck(ack) => {
                assert_eq!(
                    ack.content_uri.as_deref(),
                    Some("mxc://example.org/uploaded")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }

        handle
            .send(BackendCommand::DownloadMedia {
                client_txn_id: "tx-download-ok".to_owned(),
                source: "mxc://example.org/uploaded".to_owned(),
            })
            .await
            .expect("download should enqueue");
        let download_ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::MediaDownloadAck(MediaDownloadAck {
                    client_txn_id,
                    source,
                    data: Some(_),
                    error_code: None,
                    ..
                }) if client_txn_id == "tx-download-ok" && source == "mxc://example.org/uploaded"
            )
        })
        .await;
        match download_ack {
            BackendEvent::MediaDownloadAck(ack) => {
                assert_eq!(ack.data.as_deref(), Some(&b"binary-payload"[..]));
                assert_eq!(ack.content_type, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "upload_media", "download_media"]
        );
    }

    #[tokio::test]
    async fn runtime_upload_media_failure_emits_upload_ack_error_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_upload_media_result(Err(BackendError::new(
                BackendErrorCategory::Network,
                "media_upload_failed",
                "upload failed",
            ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::UploadMedia {
                client_txn_id: "tx-upload-fail".to_owned(),
                content_type: "application/octet-stream".to_owned(),
                data: b"payload".to_vec(),
            })
            .await
            .expect("upload should enqueue");
        let upload_ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::MediaUploadAck(MediaUploadAck {
                    client_txn_id,
                    content_uri: None,
                    error_code: Some(code),
                }) if client_txn_id == "tx-upload-fail" && code == "media_upload_failed"
            )
        })
        .await;
        assert!(matches!(upload_ack, BackendEvent::MediaUploadAck(_)));
    }

    #[tokio::test]
    async fn runtime_download_media_failure_emits_download_ack_error_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_download_media_result(Err(BackendError::new(
                BackendErrorCategory::Network,
                "media_download_failed",
                "boom",
            ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::DownloadMedia {
                client_txn_id: "tx-download-fail".to_owned(),
                source: "mxc://example.org/uploaded".to_owned(),
            })
            .await
            .expect("download should enqueue");
        let download_ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::MediaDownloadAck(MediaDownloadAck {
                    client_txn_id,
                    data: None,
                    error_code: Some(code),
                    ..
                }) if client_txn_id == "tx-download-fail" && code == "media_download_failed"
            )
        })
        .await;
        assert!(matches!(download_ack, BackendEvent::MediaDownloadAck(_)));
    }

    #[tokio::test]
    async fn runtime_enable_recovery_outside_authenticated_context_emits_ack_failure() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend);
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::EnableRecovery {
                client_txn_id: "tx-enable-outside-auth".to_owned(),
                passphrase: None,
                wait_for_backups_to_upload: false,
            })
            .await
            .expect("enable recovery should enqueue");

        let ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                    client_txn_id,
                    recovery_key: None,
                    error_code: Some(code),
                }) if client_txn_id == "tx-enable-outside-auth" && code == "invalid_state_transition"
            )
        })
        .await;
        assert!(matches!(ack, BackendEvent::RecoveryEnableAck(_)));
    }

    #[tokio::test]
    async fn runtime_get_recovery_status_and_enable_flow_emit_expected_events() {
        let status = RecoveryStatus {
            backup_state: KeyBackupState::Enabled,
            backup_enabled: true,
            backup_exists_on_server: true,
            recovery_state: CoreRecoveryState::Enabled,
        };
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_recovery_status_result(Ok(status.clone()))
                .with_enable_recovery_result(Ok("word1 word2 word3".to_owned())),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::GetRecoveryStatus)
            .await
            .expect("status should enqueue");
        let status_event = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::RecoveryStatus(_))
        })
        .await;
        match status_event {
            BackendEvent::RecoveryStatus(observed) => {
                assert_eq!(observed, status);
            }
            other => panic!("unexpected status event: {other:?}"),
        }

        handle
            .send(BackendCommand::EnableRecovery {
                client_txn_id: "tx-enable".to_owned(),
                passphrase: Some("opensesame".to_owned()),
                wait_for_backups_to_upload: true,
            })
            .await
            .expect("enable should enqueue");

        let ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                    client_txn_id,
                    recovery_key: Some(key),
                    error_code: None,
                }) if client_txn_id == "tx-enable" && key == "word1 word2 word3"
            )
        })
        .await;
        assert!(matches!(ack, BackendEvent::RecoveryEnableAck(_)));

        assert_eq!(
            mock_backend.enable_recovery_inputs_snapshot(),
            vec![(Some("opensesame".to_owned()), true)]
        );
        assert_eq!(
            mock_backend.calls_snapshot(),
            vec![
                "login_password",
                "get_recovery_status",
                "enable_recovery",
                "get_recovery_status",
            ]
        );
    }

    #[tokio::test]
    async fn runtime_reset_recovery_flow_emits_new_recovery_key_ack() {
        let status = RecoveryStatus {
            backup_state: KeyBackupState::Enabled,
            backup_enabled: true,
            backup_exists_on_server: true,
            recovery_state: CoreRecoveryState::Enabled,
        };
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_recovery_status_result(Ok(status))
                .with_reset_recovery_result(Ok("new reset key".to_owned())),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::ResetRecovery {
                client_txn_id: "tx-reset".to_owned(),
                passphrase: Some("new-passphrase".to_owned()),
                wait_for_backups_to_upload: true,
            })
            .await
            .expect("reset should enqueue");

        let ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RecoveryEnableAck(RecoveryEnableAck {
                    client_txn_id,
                    recovery_key: Some(key),
                    error_code: None,
                }) if client_txn_id == "tx-reset" && key == "new reset key"
            )
        })
        .await;
        assert!(matches!(ack, BackendEvent::RecoveryEnableAck(_)));

        assert_eq!(
            mock_backend.reset_recovery_inputs_snapshot(),
            vec![(Some("new-passphrase".to_owned()), true)]
        );
        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "reset_recovery", "get_recovery_status",]
        );
    }

    #[tokio::test]
    async fn runtime_recover_secrets_failure_emits_ack_error_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_recover_secrets_result(Err(
                BackendError::new(
                    BackendErrorCategory::Crypto,
                    "secret_storage_error",
                    "bad key",
                ),
            )),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::RecoverSecrets {
                client_txn_id: "tx-recover".to_owned(),
                recovery_key: "wrong secret".to_owned(),
            })
            .await
            .expect("recover should enqueue");

        let ack = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::RecoveryRestoreAck(RecoveryRestoreAck {
                    client_txn_id,
                    error_code: Some(code),
                }) if client_txn_id == "tx-recover" && code == "secret_storage_error"
            )
        })
        .await;
        assert!(matches!(ack, BackendEvent::RecoveryRestoreAck(_)));
        assert_eq!(
            mock_backend.recover_secret_inputs_snapshot(),
            vec!["wrong secret".to_owned()]
        );
    }

    #[tokio::test]
    async fn runtime_happy_path_login_list_and_send_message_is_deterministic() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(vec![RoomSummary {
            room_id: "!room:example.org".to_owned(),
            name: Some("Mock Room".to_owned()),
            unread_notifications: 0,
            highlight_count: 0,
            is_direct: false,
            membership: RoomMembership::Joined,
        }]));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");

        let configured = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;
        assert!(matches!(
            configured,
            BackendEvent::StateChanged {
                state: BackendLifecycleState::Configured
            }
        ));

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(success);
                assert_eq!(error_code, None);
            }
            other => panic!("unexpected auth event: {other:?}"),
        }

        handle
            .send(BackendCommand::ListRooms)
            .await
            .expect("list rooms should enqueue");

        let room_list_event = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::RoomListUpdated { .. })
        })
        .await;
        match room_list_event {
            BackendEvent::RoomListUpdated { rooms } => {
                assert_eq!(rooms.len(), 1);
                assert_eq!(rooms[0].room_id, "!room:example.org");
            }
            other => panic!("unexpected room list event: {other:?}"),
        }

        handle
            .send(BackendCommand::SendMessage {
                room_id: "!room:example.org".to_owned(),
                client_txn_id: "txn-1".to_owned(),
                body: "hello from test".to_owned(),
                msgtype: MessageType::Text,
            })
            .await
            .expect("send message should enqueue");

        let send_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::SendAck(_))
        })
        .await;
        match send_ack {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-1");
                assert_eq!(ack.event_id.as_deref(), Some("$mock-event:example.org"));
                assert_eq!(ack.error_code, None);
            }
            other => panic!("unexpected send ack event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "list_rooms", "send_message"]
        );
    }

    #[tokio::test]
    async fn runtime_login_uses_device_id_hint_when_present() {
        let data_dir = unique_temp_dir("runtime-device-id-hint");
        fs::create_dir_all(&data_dir).expect("data dir should be creatable");
        let hint_path = device_id_hint_file_path(&data_dir);
        persist_device_id_hint_to_file(&hint_path, "HINTDEVICE")
            .expect("device hint should be writable");

        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: data_dir.clone(),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::AuthResult {
                    success: true,
                    error_code: None
                }
            )
        })
        .await;

        assert_eq!(
            mock_backend.login_device_ids_snapshot(),
            vec![Some("HINTDEVICE".to_owned())]
        );

        let _ = fs::remove_dir_all(&data_dir);
        let _ = fs::remove_file(&hint_path);
    }

    #[tokio::test]
    async fn runtime_login_uses_legacy_device_id_hint_and_migrates_it() {
        let data_dir = unique_temp_dir("runtime-device-id-legacy-hint");
        fs::create_dir_all(&data_dir).expect("data dir should be creatable");
        let hint_path = device_id_hint_file_path(&data_dir);
        let legacy_hint_path = legacy_device_id_hint_file_path(&data_dir);
        persist_device_id_hint_to_file(&legacy_hint_path, "LEGACYDEVICE")
            .expect("legacy device hint should be writable");

        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_login_result(Err(
            BackendError::new(
                BackendErrorCategory::Auth,
                "auth_invalid_credentials",
                "bad credentials",
            ),
        )));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: data_dir.clone(),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::AuthResult {
                    success: false,
                    error_code: Some(_)
                }
            )
        })
        .await;

        assert_eq!(
            mock_backend.login_device_ids_snapshot(),
            vec![Some("LEGACYDEVICE".to_owned())]
        );

        let migrated = load_device_id_hint_from_file(&hint_path)
            .expect("new device-id hint should be readable");
        assert_eq!(migrated.as_deref(), Some("LEGACYDEVICE"));

        let _ = fs::remove_dir_all(&data_dir);
        let _ = fs::remove_file(&hint_path);
    }

    #[tokio::test]
    async fn runtime_login_persists_device_id_hint() {
        let data_dir = unique_temp_dir("runtime-device-id-persist");
        let hint_path = device_id_hint_file_path(&data_dir);
        let _ = fs::remove_file(&hint_path);

        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend);
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: data_dir.clone(),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::AuthResult {
                    success: true,
                    error_code: None
                }
            )
        })
        .await;

        let persisted =
            load_device_id_hint_from_file(&hint_path).expect("device-id hint should be readable");
        assert_eq!(persisted.as_deref(), Some("MOCKDEVICE"));

        let _ = fs::remove_dir_all(&data_dir);
    }

    #[tokio::test]
    async fn runtime_login_failure_emits_auth_result_error_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_session(None)
                .with_login_result(Err(BackendError::new(
                    BackendErrorCategory::Auth,
                    "auth_invalid_credentials",
                    "bad credentials",
                ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "wrong-password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(!success);
                assert_eq!(error_code.as_deref(), Some("auth_invalid_credentials"));
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        assert_eq!(mock_backend.calls_snapshot(), vec!["login_password"]);
    }

    #[tokio::test]
    async fn runtime_login_failure_with_existing_session_falls_back_to_auth_success() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_login_result(Err(
            BackendError::new(
                BackendErrorCategory::Internal,
                "matrix_error",
                "already logged in",
            ),
        )));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(success);
                assert_eq!(error_code, None);
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        handle
            .send(BackendCommand::ListRooms)
            .await
            .expect("list rooms should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::RoomListUpdated { .. })
        })
        .await;

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "list_rooms"]
        );
    }

    #[tokio::test]
    async fn runtime_login_timeout_emits_auth_timeout_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_login_delay(Duration::from_secs(1))
                .with_session(None),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(!success);
                assert_eq!(error_code.as_deref(), Some("auth_login_timeout"));
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        assert_eq!(mock_backend.calls_snapshot(), vec!["login_password"]);
    }

    #[tokio::test]
    async fn runtime_login_timeout_with_existing_session_falls_back_to_auth_success() {
        let mock_backend =
            Arc::new(MockRuntimeBackend::new(Vec::new()).with_login_delay(Duration::from_secs(1)));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(success);
                assert_eq!(error_code, None);
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        assert_eq!(mock_backend.calls_snapshot(), vec!["login_password"]);
    }

    #[tokio::test]
    async fn runtime_login_failure_with_store_mismatch_does_not_fallback() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_login_result(Err(
            BackendError::new(
                BackendErrorCategory::Internal,
                "matrix_error",
                "the account in the store doesn't match the account in the constructor",
            ),
        )));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(!success);
                assert_eq!(error_code.as_deref(), Some("matrix_error"));
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        assert_eq!(mock_backend.calls_snapshot(), vec!["login_password"]);
    }

    #[tokio::test]
    async fn runtime_send_message_failure_emits_send_ack_error_code() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_send_message_result(Err(BackendError::new(
                BackendErrorCategory::Network,
                "send_network_failure",
                "network outage",
            ))),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::SendMessage {
                room_id: "!room:example.org".to_owned(),
                client_txn_id: "txn-failure".to_owned(),
                body: "hello".to_owned(),
                msgtype: MessageType::Text,
            })
            .await
            .expect("send message should enqueue");

        let send_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::SendAck(_))
        })
        .await;
        match send_ack {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-failure");
                assert_eq!(ack.event_id, None);
                assert_eq!(ack.error_code.as_deref(), Some("send_network_failure"));
            }
            other => panic!("unexpected send ack event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "send_message"]
        );
    }

    #[tokio::test]
    async fn runtime_accept_room_invite_success_emits_ack_and_refreshes_room_list() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(vec![RoomSummary {
            room_id: "!room:example.org".to_owned(),
            name: Some("Mock Room".to_owned()),
            unread_notifications: 0,
            highlight_count: 0,
            is_direct: false,
            membership: RoomMembership::Joined,
        }]));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::AcceptRoomInvite {
                room_id: "!room:example.org".to_owned(),
                client_txn_id: "invite-txn-1".to_owned(),
            })
            .await
            .expect("accept invite should enqueue");

        let invite_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::InviteActionAck(_))
        })
        .await;
        match invite_ack {
            BackendEvent::InviteActionAck(ack) => {
                assert_eq!(ack.client_txn_id, "invite-txn-1");
                assert_eq!(ack.room_id, "!room:example.org");
                assert_eq!(ack.action, InviteAction::Accept);
                assert_eq!(ack.error_code, None);
            }
            other => panic!("unexpected invite ack event: {other:?}"),
        }

        let room_list = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::RoomListUpdated { .. })
        })
        .await;
        match room_list {
            BackendEvent::RoomListUpdated { rooms } => {
                assert_eq!(rooms.len(), 1);
                assert_eq!(rooms[0].room_id, "!room:example.org");
                assert_eq!(rooms[0].membership, RoomMembership::Joined);
            }
            other => panic!("unexpected room list event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "accept_room_invite", "list_rooms"]
        );
    }

    #[tokio::test]
    async fn runtime_reject_room_invite_failure_emits_ack_without_fatal_error() {
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_reject_room_invite_result(Err(
                BackendError::new(
                    BackendErrorCategory::Network,
                    "invite_reject_failed",
                    "temporary network issue",
                ),
            )),
        );
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::RejectRoomInvite {
                room_id: "!room:example.org".to_owned(),
                client_txn_id: "invite-txn-2".to_owned(),
            })
            .await
            .expect("reject invite should enqueue");

        let invite_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::InviteActionAck(_))
        })
        .await;
        match invite_ack {
            BackendEvent::InviteActionAck(ack) => {
                assert_eq!(ack.client_txn_id, "invite-txn-2");
                assert_eq!(ack.room_id, "!room:example.org");
                assert_eq!(ack.action, InviteAction::Reject);
                assert_eq!(ack.error_code.as_deref(), Some("invite_reject_failed"));
            }
            other => panic!("unexpected invite ack event: {other:?}"),
        }

        let fatal = time::timeout(
            Duration::from_millis(200),
            recv_matching_event(&mut events, |event| {
                matches!(event, BackendEvent::FatalError { .. })
            }),
        )
        .await;
        assert!(fatal.is_err(), "invite failures should not emit FatalError");

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "reject_room_invite"]
        );
    }

    #[tokio::test]
    async fn runtime_invite_action_invalid_state_emits_ack_without_fatal_error() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::AcceptRoomInvite {
                room_id: "!room:example.org".to_owned(),
                client_txn_id: "invite-txn-3".to_owned(),
            })
            .await
            .expect("accept invite should enqueue");

        let invite_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::InviteActionAck(_))
        })
        .await;
        match invite_ack {
            BackendEvent::InviteActionAck(ack) => {
                assert_eq!(ack.client_txn_id, "invite-txn-3");
                assert_eq!(ack.action, InviteAction::Accept);
                assert_eq!(ack.error_code.as_deref(), Some("invalid_state_transition"));
            }
            other => panic!("unexpected invite ack event: {other:?}"),
        }

        let fatal = time::timeout(
            Duration::from_millis(200),
            recv_matching_event(&mut events, |event| {
                matches!(event, BackendEvent::FatalError { .. })
            }),
        )
        .await;
        assert!(
            fatal.is_err(),
            "invite validation failures should not emit FatalError"
        );

        assert!(mock_backend.calls_snapshot().is_empty());
    }

    #[tokio::test]
    async fn runtime_restore_session_without_persisted_session_reports_auth_error() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::RestoreSession)
            .await
            .expect("restore should enqueue");

        let auth_result = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { .. })
        })
        .await;
        match auth_result {
            BackendEvent::AuthResult {
                success,
                error_code,
            } => {
                assert!(!success);
                assert_eq!(error_code.as_deref(), Some("session_not_found"));
            }
            other => panic!("unexpected auth result event: {other:?}"),
        }

        assert!(mock_backend.calls_snapshot().is_empty());
    }

    #[tokio::test]
    async fn runtime_logout_still_transitions_when_remote_logout_fails() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_logout_result(Err(
            BackendError::new(
                BackendErrorCategory::Network,
                "logout_network_error",
                "network outage",
            ),
        )));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::Logout)
            .await
            .expect("logout should enqueue");
        let logged_out = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::LoggedOut
                }
            )
        })
        .await;
        assert!(matches!(
            logged_out,
            BackendEvent::StateChanged {
                state: BackendLifecycleState::LoggedOut
            }
        ));

        let fatal = time::timeout(
            Duration::from_millis(200),
            recv_matching_event(&mut events, |event| {
                matches!(event, BackendEvent::FatalError { .. })
            }),
        )
        .await;
        assert!(
            fatal.is_err(),
            "remote logout failures should not emit FatalError"
        );
        assert_eq!(mock_backend.calls_snapshot(), vec!["logout"]);
    }

    #[tokio::test]
    async fn runtime_start_and_stop_sync_happy_path_transitions_state() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::StartSync)
            .await
            .expect("start sync should enqueue");
        let started = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Syncing
                }
            )
        })
        .await;
        assert!(matches!(
            started,
            BackendEvent::StateChanged {
                state: BackendLifecycleState::Syncing
            }
        ));

        handle
            .send(BackendCommand::StopSync)
            .await
            .expect("stop sync should enqueue");
        let stopped = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Authenticated
                }
            )
        })
        .await;
        assert!(matches!(
            stopped,
            BackendEvent::StateChanged {
                state: BackendLifecycleState::Authenticated
            }
        ));

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "start_sync", "stop_sync"]
        );
    }

    #[tokio::test]
    async fn runtime_start_sync_network_failure_emits_recoverable_fatal_error() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_start_sync_result(
            Err(BackendError::new(
                BackendErrorCategory::Network,
                "sync_start_network_error",
                "temporary network issue",
            )),
        ));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::StartSync)
            .await
            .expect("start sync should enqueue");
        let fatal = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::FatalError { .. })
        })
        .await;

        match fatal {
            BackendEvent::FatalError {
                code, recoverable, ..
            } => {
                assert_eq!(code, "sync_start_network_error");
                assert!(recoverable);
            }
            other => panic!("unexpected fatal event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "start_sync"]
        );
    }

    #[tokio::test]
    async fn runtime_start_sync_auth_failure_emits_nonrecoverable_fatal_error() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(Vec::new()).with_start_sync_result(
            Err(BackendError::new(
                BackendErrorCategory::Auth,
                "sync_start_auth_error",
                "unauthorized",
            )),
        ));
        let handle = spawn_runtime_with_backend(mock_backend.clone());
        let mut events = handle.subscribe();

        handle
            .send(BackendCommand::Init {
                homeserver: "https://matrix.example.org".to_owned(),
                data_dir: PathBuf::from("./.unused-test-store"),
                config: None,
            })
            .await
            .expect("init should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(
                event,
                BackendEvent::StateChanged {
                    state: BackendLifecycleState::Configured
                }
            )
        })
        .await;

        handle
            .send(BackendCommand::LoginPassword {
                user_id_or_localpart: "@mock:example.org".to_owned(),
                password: "password".to_owned(),
                persist_session: true,
            })
            .await
            .expect("login should enqueue");
        let _ = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::AuthResult { success: true, .. })
        })
        .await;

        handle
            .send(BackendCommand::StartSync)
            .await
            .expect("start sync should enqueue");
        let fatal = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::FatalError { .. })
        })
        .await;

        match fatal {
            BackendEvent::FatalError {
                code, recoverable, ..
            } => {
                assert_eq!(code, "sync_start_auth_error");
                assert!(!recoverable);
            }
            other => panic!("unexpected fatal event: {other:?}"),
        }

        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "start_sync"]
        );
    }

    #[tokio::test]
    async fn frontend_adapter_supports_queue_cancellation_and_flush() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(vec![RoomSummary {
            room_id: "!room:example.org".to_owned(),
            name: Some("Mock Room".to_owned()),
            unread_notifications: 0,
            highlight_count: 0,
            is_direct: false,
            membership: RoomMembership::Joined,
        }]));
        let runtime = spawn_runtime_with_backend(mock_backend.clone());
        let adapter = MatrixFrontendAdapter::new(runtime);
        let mut events = adapter.subscribe();

        let _init_id = adapter.enqueue(BackendCommand::Init {
            homeserver: "https://matrix.example.org".to_owned(),
            data_dir: PathBuf::from("./.unused-test-store"),
            config: None,
        });
        let _login_id = adapter.enqueue(BackendCommand::LoginPassword {
            user_id_or_localpart: "@mock:example.org".to_owned(),
            password: "password".to_owned(),
            persist_session: true,
        });
        let list_id = adapter.enqueue(BackendCommand::ListRooms);
        let send_id = adapter.enqueue(BackendCommand::SendMessage {
            room_id: "!room:example.org".to_owned(),
            client_txn_id: "txn-adapter".to_owned(),
            body: "hello".to_owned(),
            msgtype: MessageType::Text,
        });

        assert_eq!(adapter.queued_len(), 4);
        assert!(adapter.cancel(list_id));
        assert_eq!(adapter.queued_len(), 3);
        assert!(!adapter.cancel(FrontendCommandId(999_999)));

        let flushed = adapter.flush_all().await.expect("flush should work");
        assert_eq!(flushed, 3);
        assert_eq!(adapter.queued_len(), 0);

        let send_ack = recv_matching_event(&mut events, |event| {
            matches!(event, BackendEvent::SendAck(_))
        })
        .await;
        match send_ack {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-adapter");
                assert_eq!(ack.error_code, None);
            }
            other => panic!("unexpected send ack event: {other:?}"),
        }

        assert!(!adapter.cancel(send_id));
        assert_eq!(
            mock_backend.calls_snapshot(),
            vec!["login_password", "send_message"]
        );

        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn frontend_adapter_forwards_runtime_events() {
        let runtime = spawn_runtime();
        let adapter = MatrixFrontendAdapter::new(runtime);
        let mut events = adapter.subscribe();

        adapter.enqueue(BackendCommand::StartSync);
        adapter.flush_all().await.expect("flush should work");

        let event = recv_matching_event(&mut events, |candidate| {
            matches!(candidate, BackendEvent::FatalError { .. })
        })
        .await;
        match event {
            BackendEvent::FatalError { code, .. } => {
                assert_eq!(code, "invalid_state_transition");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn frontend_adapter_tracks_timeline_snapshots_from_deltas() {
        let room_id = "!room:example.org".to_owned();
        let target_event_id = "$target:example.org".to_owned();
        let initial_item = make_timeline_item(&target_event_id, "first body");
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new())
                .with_open_room_result(Ok((vec![TimelineOp::Append(initial_item.clone())], None))),
        );
        let runtime = spawn_runtime_with_backend(mock_backend);
        let adapter = MatrixFrontendAdapter::with_config(runtime, 64, 32);
        let mut events = adapter.subscribe();

        adapter.enqueue(BackendCommand::Init {
            homeserver: "https://matrix.example.org".to_owned(),
            data_dir: PathBuf::from("./.unused-test-store"),
            config: None,
        });
        adapter.enqueue(BackendCommand::LoginPassword {
            user_id_or_localpart: "@mock:example.org".to_owned(),
            password: "password".to_owned(),
            persist_session: true,
        });
        adapter.enqueue(BackendCommand::OpenRoom {
            room_id: room_id.clone(),
        });
        adapter.enqueue(BackendCommand::EditMessage {
            room_id: room_id.clone(),
            target_event_id: target_event_id.clone(),
            new_body: "edited body".to_owned(),
            client_txn_id: "edit-txn".to_owned(),
        });
        adapter.enqueue(BackendCommand::RedactMessage {
            room_id: room_id.clone(),
            target_event_id: target_event_id.clone(),
            reason: Some("cleanup".to_owned()),
        });
        adapter.flush_all().await.expect("flush should work");

        let first_snapshot = recv_matching_event(&mut events, |event| match event {
            BackendEvent::RoomTimelineSnapshot { room_id, items } => {
                room_id == "!room:example.org" && items.len() == 1 && items[0].body == "first body"
            }
            _ => false,
        })
        .await;
        match first_snapshot {
            BackendEvent::RoomTimelineSnapshot { items, .. } => {
                assert_eq!(items[0].event_id.as_deref(), Some("$target:example.org"));
            }
            other => panic!("unexpected first snapshot event: {other:?}"),
        }

        let edited_snapshot = recv_matching_event(&mut events, |event| match event {
            BackendEvent::RoomTimelineSnapshot { room_id, items } => {
                room_id == "!room:example.org" && items.len() == 1 && items[0].body == "edited body"
            }
            _ => false,
        })
        .await;
        assert!(matches!(
            edited_snapshot,
            BackendEvent::RoomTimelineSnapshot { .. }
        ));

        let empty_snapshot = recv_matching_event(&mut events, |event| match event {
            BackendEvent::RoomTimelineSnapshot { room_id, items } => {
                room_id == "!room:example.org" && items.is_empty()
            }
            _ => false,
        })
        .await;
        assert!(matches!(
            empty_snapshot,
            BackendEvent::RoomTimelineSnapshot { .. }
        ));

        assert!(adapter.timeline_snapshot(&room_id).is_empty());
        assert_eq!(adapter.timeline_max_items(), 32);

        adapter.shutdown().await;
    }

    #[tokio::test]
    async fn frontend_adapter_truncates_snapshot_to_timeline_max_items() {
        let room_id = "!room:example.org".to_owned();
        let open_room_ops = (1..=5)
            .map(|idx| {
                let event_id = format!("$event-{idx}:example.org");
                let body = format!("body {idx}");
                TimelineOp::Append(make_timeline_item(&event_id, &body))
            })
            .collect::<Vec<_>>();
        let mock_backend = Arc::new(
            MockRuntimeBackend::new(Vec::new()).with_open_room_result(Ok((open_room_ops, None))),
        );
        let runtime = spawn_runtime_with_backend(mock_backend);
        let adapter = MatrixFrontendAdapter::with_config(runtime, 64, 2);
        let mut events = adapter.subscribe();

        adapter.enqueue(BackendCommand::Init {
            homeserver: "https://matrix.example.org".to_owned(),
            data_dir: PathBuf::from("./.unused-test-store"),
            config: None,
        });
        adapter.enqueue(BackendCommand::LoginPassword {
            user_id_or_localpart: "@mock:example.org".to_owned(),
            password: "password".to_owned(),
            persist_session: true,
        });
        adapter.enqueue(BackendCommand::OpenRoom {
            room_id: room_id.clone(),
        });
        adapter.flush_all().await.expect("flush should work");

        let truncated_snapshot = recv_matching_event(&mut events, |event| match event {
            BackendEvent::RoomTimelineSnapshot { room_id, items } => {
                room_id == "!room:example.org" && items.len() == 2
            }
            _ => false,
        })
        .await;
        match truncated_snapshot {
            BackendEvent::RoomTimelineSnapshot { items, .. } => {
                assert_eq!(items[0].event_id.as_deref(), Some("$event-4:example.org"));
                assert_eq!(items[1].event_id.as_deref(), Some("$event-5:example.org"));
            }
            other => panic!("unexpected snapshot event: {other:?}"),
        }

        let snapshot = adapter.timeline_snapshot(&room_id);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(
            snapshot[0].event_id.as_deref(),
            Some("$event-4:example.org")
        );
        assert_eq!(
            snapshot[1].event_id.as_deref(),
            Some("$event-5:example.org")
        );

        adapter.shutdown().await;
    }

    #[tokio::test]
    #[ignore = "runs against live homeserver, requires env vars"]
    async fn live_login_sync_and_dm_smoke() {
        let homeserver = env::var("PIKACHAT_HOMESERVER").expect("PIKACHAT_HOMESERVER must be set");
        let user = env::var("PIKACHAT_USER").expect("PIKACHAT_USER must be set");
        let password = env::var("PIKACHAT_PASSWORD").expect("PIKACHAT_PASSWORD must be set");
        let dm_target = env::var("PIKACHAT_DM_TARGET").expect("PIKACHAT_DM_TARGET must be set");

        let unique = format!(
            ".pikachat-live-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_secs()
        );
        let data_dir = std::path::PathBuf::from(unique);

        let backend = MatrixBackend::new(MatrixBackendConfig::new(homeserver, data_dir, None))
            .await
            .expect("backend init");
        backend
            .login_password(&user, &password, "PikaChat CI Smoke", None)
            .await
            .expect("login");
        backend.sync_once().await.expect("sync once");
        backend
            .send_dm_text(&dm_target, "PikaChat backend live smoke test")
            .await
            .expect("dm send");
    }
}
