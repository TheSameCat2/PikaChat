use std::{collections::HashMap, path::PathBuf, sync::Arc};

use backend_core::{
    BackendChannelError, BackendChannels, BackendCommand, BackendError, BackendErrorCategory,
    BackendEvent, BackendLifecycleState, BackendStateMachine, EventStream, MessageType,
    RetryPolicy, RoomSummary, SendOutcome, SyncStatus, TimelineBuffer, TimelineItem, TimelineOp,
    classify_http_status, normalize_send_outcome,
};
use backend_platform::{OsKeyringSecretStore, ScopedSecretStore, SecretStoreError};
use matrix_sdk::{
    Client, ClientBuildError, HttpError,
    authentication::matrix::MatrixSession,
    config::SyncSettings,
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
const SERVER_PAGINATION_LIMIT_CAP: u16 = 100;

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
            let mut sync_settings = SyncSettings::default();

            loop {
                tokio::select! {
                    _ = stop_child.cancelled() => break,
                    sync_result = client.sync_once(sync_settings.clone()) => {
                        match sync_result {
                            Ok(sync_response) => {
                                attempt = 0;
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

#[derive(Debug, Clone)]
struct RuntimeInitState {
    session_account: String,
}

struct MatrixRuntime {
    channels: BackendChannels,
    command_rx: mpsc::Receiver<BackendCommand>,
    state_machine: BackendStateMachine,
    backend: Option<Arc<MatrixBackend>>,
    init_state: Option<RuntimeInitState>,
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
            init_state: None,
            pagination_tokens: HashMap::new(),
            keyring: ScopedSecretStore::new(OsKeyringSecretStore, KEYRING_SERVICE),
        }
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
            } => self.handle_init(homeserver, data_dir).await,
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
    ) -> Result<(), BackendError> {
        let (candidate, transition_events) = self.validate_transition(BackendCommand::Init {
            homeserver: String::new(),
            data_dir: PathBuf::new(),
        })?;

        let passphrase_account = store_passphrase_account_for_homeserver(&homeserver);
        let session_account = session_account_for_homeserver(&homeserver);
        let store_passphrase = self.get_or_create_store_passphrase(&passphrase_account)?;

        let backend = Arc::new(
            MatrixBackend::new(MatrixBackendConfig::new(
                homeserver.clone(),
                data_dir.clone(),
                Some(store_passphrase),
            ))
            .await?,
        );

        self.backend = Some(backend);
        self.init_state = Some(RuntimeInitState { session_account });
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
        backend.start_sync(self.channels.event_sender()).await?;
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

        let (ops, next_token) = backend.open_room(&room_id, DEFAULT_OPEN_ROOM_LIMIT).await?;
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
            TimelineBuffer::bounded_paginate_limit(limit, SERVER_PAGINATION_LIMIT_CAP);

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

        if let Ok(init_state) = self.require_init_state() {
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

    fn require_backend(&self) -> Result<Arc<MatrixBackend>, BackendError> {
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
    use std::{env, time::Duration};
    use tokio::time::timeout;

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
