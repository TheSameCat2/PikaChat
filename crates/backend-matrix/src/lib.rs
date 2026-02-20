use std::path::PathBuf;

use backend_core::{
    BackendError, BackendErrorCategory, BackendEvent, MessageType, classify_http_status,
};
use matrix_sdk::{
    Client, ClientBuildError,
    config::SyncSettings,
    ruma::{OwnedRoomId, events::room::message::RoomMessageEventContent},
};
use tokio::{
    sync::{Mutex, broadcast},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

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
        let task = tokio::spawn(async move {
            let _ = event_tx_clone.send(BackendEvent::SyncStatus(backend_core::SyncStatus {
                running: true,
                lag_hint_ms: None,
            }));

            stop_child.cancelled().await;

            let _ = event_tx_clone.send(BackendEvent::SyncStatus(backend_core::SyncStatus {
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
        let room_id = parse_room_id(room_id)?;
        let room = self.client.get_room(&room_id).ok_or_else(|| {
            BackendError::new(
                BackendErrorCategory::Config,
                "room_not_found",
                format!("room not found: {room_id}"),
            )
        })?;

        let content = match msgtype {
            MessageType::Text => RoomMessageEventContent::text_plain(body),
            MessageType::Notice => RoomMessageEventContent::notice_plain(body),
            MessageType::Emote => RoomMessageEventContent::emote_plain(body),
        };

        let response = room.send(content).await.map_err(map_matrix_error)?;
        Ok(response.event_id.to_string())
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

fn map_matrix_error(err: matrix_sdk::Error) -> BackendError {
    use matrix_sdk::Error;

    match err {
        Error::Http(http_err) => {
            if let Some(client_err) = http_err.as_client_api_error() {
                let status = client_err.status_code.as_u16();
                return BackendError::new(
                    classify_http_status(status),
                    "matrix_http_error",
                    client_err.to_string(),
                );
            }

            BackendError::new(
                BackendErrorCategory::Network,
                "matrix_http_error",
                http_err.to_string(),
            )
        }
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
    use backend_core::RetryPolicy;

    #[test]
    fn rejects_invalid_room_id() {
        let err = parse_room_id("not-a-room-id").expect_err("invalid room id must fail");
        assert_eq!(err.code, "invalid_room_id");
    }

    #[test]
    fn retry_policy_defaults_are_sane_for_sync_loop() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.base_delay_ms(), 500);
        assert_eq!(policy.max_delay_ms(), 30_000);
    }
}
