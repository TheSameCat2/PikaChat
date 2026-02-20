use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use backend_core::{
    BackendChannelError, BackendChannels, BackendCommand, BackendError, BackendErrorCategory,
    BackendEvent, BackendInitConfig, BackendLifecycleState, BackendStateMachine, EventStream,
    MessageType, RetryPolicy, RoomSummary, SendOutcome, SyncStatus, TimelineBuffer, TimelineItem,
    TimelineOp, classify_http_status, normalize_send_outcome,
};
use backend_platform::{OsKeyringSecretStore, ScopedSecretStore, SecretStoreError};
use matrix_sdk::{
    Client, ClientBuildError, HttpError,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    deserialized_responses::TimelineEvent,
    room::{Messages, MessagesOptions, edit::EditedContent},
    ruma::{
        OwnedEventId, OwnedRoomId, OwnedUserId, UInt,
        api::client::error::{ErrorKind, RetryAfter},
        events::room::message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
    },
};
use tokio::{
    sync::{Mutex, broadcast, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const KEYRING_SERVICE: &str = "dev.thesamecat.pikachat";
const STORE_PASSPHRASE_KEY_PREFIX: &str = "store-passphrase";
const SESSION_KEY_PREFIX: &str = "matrix-session";
const DEFAULT_DEVICE_DISPLAY_NAME: &str = "PikaChat Desktop";
const DEFAULT_OPEN_ROOM_LIMIT: u16 = 30;
const DEFAULT_PAGINATION_LIMIT_CAP: u16 = 100;

#[derive(Debug, Clone)]
pub struct MatrixBackendConfig {
    pub homeserver: String,
    pub data_dir: PathBuf,
    pub store_passphrase: Option<String>,
}

impl MatrixBackendConfig {
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

#[derive(Debug)]
pub struct MatrixBackend {
    client: Client,
    sync_task: Mutex<Option<RunningSyncTask>>,
}

impl MatrixBackend {
    pub async fn new(config: MatrixBackendConfig) -> Result<Self, BackendError> {
        let client = Client::builder()
            .homeserver_url(&config.homeserver)
            .sqlite_store(&config.data_dir, config.store_passphrase.as_deref())
            .build()
            .await
            .map_err(map_client_build_error)?;

        Ok(Self {
            client,
            sync_task: Mutex::new(None),
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn session(&self) -> Option<MatrixSession> {
        self.client.matrix_auth().session()
    }

    pub async fn restore_session(&self, session: MatrixSession) -> Result<(), BackendError> {
        self.client
            .restore_session(session)
            .await
            .map_err(map_matrix_error)
    }

    pub async fn logout(&self) -> Result<(), BackendError> {
        self.client.logout().await.map_err(map_matrix_error)
    }

    pub fn list_rooms(&self) -> Vec<RoomSummary> {
        collect_room_summaries(&self.client)
    }

    pub async fn login_password(
        &self,
        user_id_or_localpart: &str,
        password: &str,
        device_display_name: &str,
    ) -> Result<(), BackendError> {
        self.client
            .matrix_auth()
            .login_username(user_id_or_localpart, password)
            .initial_device_display_name(device_display_name)
            .send()
            .await
            .map(|_| ())
            .map_err(map_matrix_error)
    }

    pub async fn start_sync(
        &self,
        event_tx: broadcast::Sender<BackendEvent>,
        sync_request_timeout: Option<Duration>,
    ) -> Result<(), BackendError> {
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
            let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                running: true,
                lag_hint_ms: None,
            }));

            let retry_policy = RetryPolicy::default();
            let mut attempt: u32 = 0;
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
                                for (room_id, ops) in sync_timeline_deltas(&sync_response) {
                                    let _ = event_tx_clone.send(BackendEvent::RoomTimelineDelta {
                                        room_id,
                                        ops,
                                    });
                                }
                                sync_settings = sync_settings.token(sync_response.next_batch);
                                let rooms = collect_room_summaries(&client);
                                let _ = event_tx_clone.send(BackendEvent::RoomListUpdated { rooms });
                                let _ = event_tx_clone.send(BackendEvent::SyncStatus(SyncStatus {
                                    running: true,
                                    lag_hint_ms: None,
                                }));
                            }
                            Err(err) => {
                                let mapped = map_matrix_error(err);
                                let recoverable = is_recoverable_sync_error(&mapped);
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
        });

        *guard = Some(RunningSyncTask { stop, task });
        Ok(())
    }

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

    pub async fn sync_once(&self) -> Result<(), BackendError> {
        self.client
            .sync_once(SyncSettings::default())
            .await
            .map(|_| ())
            .map_err(map_matrix_error)
    }

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
    ) -> Result<(), BackendError> {
        MatrixBackend::login_password(self, user_id_or_localpart, password, device_display_name)
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
}

#[derive(Clone, Debug)]
pub struct MatrixRuntimeHandle {
    channels: BackendChannels,
}

impl MatrixRuntimeHandle {
    pub async fn send(&self, command: BackendCommand) -> Result<(), BackendChannelError> {
        self.channels.send_command(command).await
    }

    pub fn subscribe(&self) -> EventStream {
        self.channels.subscribe()
    }
}

pub fn spawn_runtime() -> MatrixRuntimeHandle {
    let (channels, command_rx) = BackendChannels::new(128, 512);
    let runtime = MatrixRuntime::new(channels.clone(), command_rx);
    tokio::spawn(async move {
        runtime.run().await;
    });

    MatrixRuntimeHandle { channels }
}

#[cfg(test)]
fn spawn_runtime_with_backend(backend: Arc<dyn RuntimeBackend>) -> MatrixRuntimeHandle {
    let (channels, command_rx) = BackendChannels::new(128, 512);
    let runtime = MatrixRuntime::new(channels.clone(), command_rx).with_backend_override(backend);
    tokio::spawn(async move {
        runtime.run().await;
    });

    MatrixRuntimeHandle { channels }
}

#[derive(Debug, Clone)]
struct RuntimeInitState {
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

    async fn run(mut self) {
        while let Some(command) = self.command_rx.recv().await {
            if let Err(err) = self.handle_command(command).await {
                let recoverable = matches!(
                    err.category,
                    BackendErrorCategory::Network | BackendErrorCategory::RateLimited
                );
                self.channels.emit(BackendEvent::FatalError {
                    code: err.code,
                    message: err.message,
                    recoverable,
                });
            }
        }
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
            } => {
                self.handle_login_password(user_id_or_localpart, password)
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
            BackendCommand::Logout => self.handle_logout().await,
        }
    }

    async fn handle_init(
        &mut self,
        homeserver: String,
        data_dir: PathBuf,
        config: Option<BackendInitConfig>,
    ) -> Result<(), BackendError> {
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
            let store_passphrase = self.get_or_create_store_passphrase(&passphrase_account)?;
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
        self.init_state = Some(RuntimeInitState { session_account });
        self.runtime_tuning = runtime_tuning;
        self.pagination_tokens.clear();

        self.commit_transition(candidate, transition_events);
        Ok(())
    }

    async fn handle_login_password(&mut self, user_id_or_localpart: String, password: String) {
        let transition = self.validate_transition(BackendCommand::LoginPassword {
            user_id_or_localpart: String::new(),
            password: String::new(),
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

        let login_result = backend
            .login_password(
                &user_id_or_localpart,
                &password,
                DEFAULT_DEVICE_DISPLAY_NAME,
            )
            .await;

        match login_result {
            Ok(()) => {
                let persist_result = self.persist_current_session();
                if let Err(err) = persist_result {
                    self.finish_auth(false, Some(err));
                    return;
                }
                self.finish_auth(true, None);
            }
            Err(err) => {
                self.finish_auth(false, Some(err));
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
            Ok(()) => self.finish_auth(true, None),
            Err(err) => self.finish_auth(false, Some(err)),
        }
    }

    async fn handle_start_sync(&mut self) -> Result<(), BackendError> {
        let (candidate, transition_events) = self.validate_transition(BackendCommand::StartSync)?;
        let backend = self.require_backend()?;
        backend
            .start_sync(
                self.channels.event_sender(),
                self.runtime_tuning.sync_request_timeout,
            )
            .await?;
        self.commit_transition(candidate, transition_events);
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
        let (_candidate, _events) = self.validate_transition(BackendCommand::ListRooms)?;
        let backend = self.require_backend()?;
        let rooms = backend.list_rooms();
        self.channels.emit(BackendEvent::RoomListUpdated { rooms });
        Ok(())
    }

    async fn handle_open_room(&mut self, room_id: String) -> Result<(), BackendError> {
        let (_candidate, _events) = self.validate_transition(BackendCommand::OpenRoom {
            room_id: String::new(),
        })?;
        let backend = self.require_backend()?;

        let (ops, next_token) = backend
            .open_room(&room_id, self.runtime_tuning.open_room_limit)
            .await?;
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

        self.pagination_tokens.insert(room_id.clone(), next_token);
        self.channels
            .emit(BackendEvent::RoomTimelineDelta { room_id, ops });

        Ok(())
    }

    async fn handle_send_dm_text(&mut self, user_id: String, client_txn_id: String, body: String) {
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

    async fn handle_logout(&mut self) -> Result<(), BackendError> {
        let (candidate, transition_events) = self.validate_transition(BackendCommand::Logout)?;
        let backend = self.require_backend()?;

        if matches!(self.state_machine.state(), BackendLifecycleState::Syncing) {
            let _ = backend.stop_sync().await;
        }

        backend.logout().await?;

        if !self.disable_session_persistence
            && let Ok(init_state) = self.require_init_state()
        {
            match self.keyring.delete(&init_state.session_account) {
                Ok(()) | Err(SecretStoreError::NotFound) => {}
                Err(err) => {
                    return Err(map_secret_store_error(
                        "delete_session",
                        &init_state.session_account,
                        err,
                    ));
                }
            }
        }

        self.pagination_tokens.clear();
        self.commit_transition(candidate, transition_events);
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

    fn get_or_create_store_passphrase(&self, account: &str) -> Result<String, BackendError> {
        match self.keyring.get(account) {
            Ok(passphrase) => Ok(passphrase),
            Err(SecretStoreError::NotFound) => {
                let generated = format!("pikachat-store-{}", Uuid::new_v4());
                self.keyring
                    .set(account, &generated)
                    .map_err(|err| map_secret_store_error("set_store_passphrase", account, err))?;
                Ok(generated)
            }
            Err(err) => Err(map_secret_store_error("get_store_passphrase", account, err)),
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

fn store_passphrase_account_for_homeserver(homeserver: &str) -> String {
    format!("{STORE_PASSPHRASE_KEY_PREFIX}:{homeserver}")
}

fn session_account_for_homeserver(homeserver: &str) -> String {
    format!("{SESSION_KEY_PREFIX}:{homeserver}")
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

fn timeline_ops_for_open(messages: Messages) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    for event in messages.chunk.iter().rev() {
        if let Some(item) = timeline_item_from_event(event) {
            ops.push(TimelineOp::Append(item));
        }
    }
    ops
}

fn timeline_ops_for_pagination(messages: Messages) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    for event in &messages.chunk {
        if let Some(item) = timeline_item_from_event(event) {
            ops.push(TimelineOp::Prepend(item));
        }
    }
    ops
}

fn timeline_ops_for_sync_events(events: &[TimelineEvent], limited: bool) -> Vec<TimelineOp> {
    let mut ops = Vec::new();
    if limited {
        ops.push(TimelineOp::Clear);
    }
    for event in events {
        if let Some(item) = timeline_item_from_event(event) {
            ops.push(TimelineOp::Append(item));
        }
    }
    ops
}

fn sync_timeline_deltas(
    sync_response: &matrix_sdk::sync::SyncResponse,
) -> Vec<(String, Vec<TimelineOp>)> {
    let mut deltas = Vec::new();

    for (room_id, update) in &sync_response.rooms.joined {
        let ops = timeline_ops_for_sync_events(&update.timeline.events, update.timeline.limited);
        if !ops.is_empty() {
            deltas.push((room_id.to_string(), ops));
        }
    }

    deltas
}

fn timeline_item_from_event(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
) -> Option<TimelineItem> {
    let raw = event.raw();

    let sender = raw.get_field::<String>("sender").ok().flatten()?;
    let body = raw
        .get_field::<serde_json::Value>("content")
        .ok()
        .flatten()
        .and_then(|content| {
            content
                .get("body")
                .and_then(|body| body.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "[non-text event]".to_owned());
    let timestamp_ms = event
        .timestamp_raw()
        .map(|ts| u64::from(ts.get()))
        .unwrap_or(0);

    Some(TimelineItem {
        event_id: event.event_id().map(|event_id| event_id.to_string()),
        sender,
        body,
        timestamp_ms,
    })
}

fn collect_room_summaries(client: &Client) -> Vec<RoomSummary> {
    let mut rooms: Vec<RoomSummary> = client
        .rooms()
        .into_iter()
        .map(|room| {
            let unread = room.unread_notification_counts();
            RoomSummary {
                room_id: room.room_id().to_string(),
                name: room.name(),
                unread_notifications: unread.notification_count,
                highlight_count: unread.highlight_count,
                is_direct: room.direct_targets_length() > 0,
            }
        })
        .collect();

    rooms.sort_by(|a, b| a.room_id.cmp(&b.room_id));
    rooms
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

fn map_matrix_error(err: matrix_sdk::Error) -> BackendError {
    use matrix_sdk::Error;

    match err {
        Error::Http(http_err) => map_matrix_http_error(*http_err),
        Error::AuthenticationRequired => {
            BackendError::new(BackendErrorCategory::Auth, "auth_required", err.to_string())
        }
        Error::StateStore(_) | Error::EventCacheStore(_) | Error::MediaStore(_) | Error::Io(_) => {
            BackendError::new(
                BackendErrorCategory::Storage,
                "storage_error",
                err.to_string(),
            )
        }
        Error::SerdeJson(_) => BackendError::new(
            BackendErrorCategory::Serialization,
            "serde_json_error",
            err.to_string(),
        ),
        _ => BackendError::new(
            BackendErrorCategory::Internal,
            "matrix_error",
            err.to_string(),
        ),
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
    use std::{env, path::PathBuf, sync::Mutex, time::Duration};
    use tokio::sync::broadcast;
    use tokio::time::timeout;

    fn make_sync_timeline_event(
        event_id: &str,
        sender: &str,
        body: &str,
        origin_server_ts: u64,
    ) -> TimelineEvent {
        let json = serde_json::json!({
            "type": "m.room.message",
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": origin_server_ts,
            "content": {
                "msgtype": "m.text",
                "body": body
            }
        });

        let raw = Raw::<AnySyncTimelineEvent>::from_json_string(json.to_string())
            .expect("timeline event json should parse");
        TimelineEvent::from_plaintext(raw)
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

    struct MockRuntimeBackend {
        calls: Mutex<Vec<String>>,
        rooms: Vec<RoomSummary>,
        login_result: Result<(), BackendError>,
        send_message_result: Result<String, BackendError>,
        session: Option<MatrixSession>,
    }

    impl MockRuntimeBackend {
        fn new(rooms: Vec<RoomSummary>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                rooms,
                login_result: Ok(()),
                send_message_result: Ok("$mock-event:example.org".to_owned()),
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

        fn with_login_result(mut self, login_result: Result<(), BackendError>) -> Self {
            self.login_result = login_result;
            self
        }

        fn with_send_message_result(
            mut self,
            send_message_result: Result<String, BackendError>,
        ) -> Self {
            self.send_message_result = send_message_result;
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
            Ok(())
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
        ) -> Result<(), BackendError> {
            self.record_call("login_password");
            self.login_result.clone()
        }

        async fn start_sync(
            &self,
            _event_tx: broadcast::Sender<BackendEvent>,
            _sync_request_timeout: Option<Duration>,
        ) -> Result<(), BackendError> {
            self.record_call("start_sync");
            Ok(())
        }

        async fn stop_sync(&self) -> Result<(), BackendError> {
            self.record_call("stop_sync");
            Ok(())
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
            _limit: u16,
        ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
            self.record_call("open_room");
            Ok((Vec::new(), None))
        }

        async fn paginate_back(
            &self,
            _room_id: &str,
            _from_token: Option<&str>,
            _limit: u16,
        ) -> Result<(Vec<TimelineOp>, Option<String>), BackendError> {
            self.record_call("paginate_back");
            Ok((Vec::new(), None))
        }

        async fn send_dm_text(&self, _user_id: &str, _body: &str) -> Result<String, BackendError> {
            self.record_call("send_dm_text");
            Ok("$mock-dm:example.org".to_owned())
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
    fn sync_timeline_ops_append_events_in_order() {
        let first = make_sync_timeline_event("$e1:example.org", "@alice:example.org", "first", 1);
        let second = make_sync_timeline_event("$e2:example.org", "@bob:example.org", "second", 2);
        let ops = timeline_ops_for_sync_events(&[first, second], false);

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
        let ops = timeline_ops_for_sync_events(&[event], true);
        assert_eq!(ops.len(), 2);
        assert!(matches!(ops[0], TimelineOp::Clear));
        assert!(matches!(ops[1], TimelineOp::Append(_)));
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
    async fn runtime_happy_path_login_list_and_send_message_is_deterministic() {
        let mock_backend = Arc::new(MockRuntimeBackend::new(vec![RoomSummary {
            room_id: "!room:example.org".to_owned(),
            name: Some("Mock Room".to_owned()),
            unread_notifications: 0,
            highlight_count: 0,
            is_direct: false,
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
    async fn runtime_login_failure_emits_auth_result_error_code() {
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
            .login_password(&user, &password, "PikaChat CI Smoke")
            .await
            .expect("login");
        backend.sync_once().await.expect("sync once");
        backend
            .send_dm_text(&dm_target, "PikaChat backend live smoke test")
            .await
            .expect("dm send");
    }
}
