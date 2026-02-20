use std::path::PathBuf;

use backend_core::{
    BackendError, BackendErrorCategory, BackendEvent, MessageType, RetryPolicy, RoomSummary,
    SyncStatus, classify_http_status,
};
use matrix_sdk::{
    Client, ClientBuildError,
    config::SyncSettings,
    ruma::{
        OwnedRoomId, OwnedUserId,
        api::client::error::{ErrorKind, RetryAfter},
        events::room::message::RoomMessageEventContent,
    },
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

fn parse_user_id(value: &str) -> Result<OwnedUserId, BackendError> {
    value.parse::<OwnedUserId>().map_err(|err| {
        BackendError::new(
            BackendErrorCategory::Config,
            "invalid_user_id",
            format!("invalid user id '{value}': {err}"),
        )
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

fn map_matrix_error(err: matrix_sdk::Error) -> BackendError {
    use matrix_sdk::Error;

    match err {
        Error::Http(http_err) => {
            if let Some(client_err) = http_err.as_client_api_error() {
                let status = client_err.status_code.as_u16();
                let mut mapped = BackendError::new(
                    classify_http_status(status),
                    "matrix_http_error",
                    client_err.to_string(),
                );

                if let Some(ErrorKind::LimitExceeded { retry_after }) = client_err.error_kind() {
                    if let Some(RetryAfter::Delay(delay)) = retry_after {
                        mapped = mapped.with_retry_after(*delay);
                    }
                }

                return mapped;
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
    use std::{env, time::Duration};

    #[test]
    fn rejects_invalid_room_id() {
        let err = parse_room_id("not-a-room-id").expect_err("invalid room id must fail");
        assert_eq!(err.code, "invalid_room_id");
    }

    #[test]
    fn rejects_invalid_user_id() {
        let err = parse_user_id("not-a-user").expect_err("invalid user id must fail");
        assert_eq!(err.code, "invalid_user_id");
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
