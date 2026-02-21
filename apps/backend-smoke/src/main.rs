use std::{
    env::{self, VarError},
    fs,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use backend_core::{
    BackendCommand, BackendEvent, BackendInitConfig, BackendLifecycleState, EventStream,
    RecoveryStatus,
};
use backend_matrix::{MatrixFrontendAdapter, spawn_runtime};
use tokio::{sync::broadcast, time::timeout};
use tracing::info;
use tracing_subscriber::EnvFilter;

const EVENT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SYNC_OBSERVE_SECS: u64 = 8;

#[tokio::main]
async fn main() {
    init_logging();
    if let Err(err) = run().await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let homeserver =
        env::var("PIKACHAT_HOMESERVER").unwrap_or_else(|_| "https://matrix.example.org".to_owned());
    let data_dir = env::var("PIKACHAT_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./.pikachat-smoke-store"));
    let restore_session = env_truthy("PIKACHAT_RESTORE_SESSION");
    let init_config = init_config_from_env()?;

    info!("starting backend-smoke harness");
    let runtime = spawn_runtime();
    let adapter = MatrixFrontendAdapter::new(runtime);
    let mut events = adapter.subscribe();

    let result: Result<(), String> = async {
        send_command(
            &adapter,
            BackendCommand::Init {
                homeserver: homeserver.clone(),
                data_dir,
                config: init_config,
            },
            "Init",
        )
        .await?;
        wait_for_state(&mut events, BackendLifecycleState::Configured, "configured").await?;
        println!("Runtime initialized for {homeserver}");

        let maybe_user = env::var("PIKACHAT_USER").ok();
        let maybe_password = env::var("PIKACHAT_PASSWORD").ok();

        if restore_session {
            send_command(&adapter, BackendCommand::RestoreSession, "RestoreSession").await?;
            wait_for_auth_success(&mut events, "restore session").await?;
            println!("Session restored successfully.");
        } else if let (Some(user), Some(password)) = (maybe_user, maybe_password) {
            send_command(
                &adapter,
                BackendCommand::LoginPassword {
                    user_id_or_localpart: user.clone(),
                    password,
                    persist_session: true,
                },
                "LoginPassword",
            )
            .await?;
            wait_for_auth_success(&mut events, "password login").await?;
            println!("Login successful for {user}");
        } else {
            println!("Set PIKACHAT_USER and PIKACHAT_PASSWORD to run live auth smoke.");
            println!("Optional: set PIKACHAT_DM_TARGET to send a DM smoke message.");
            println!("Optional: set PIKACHAT_RESTORE_SESSION=1 to try keyring session restore.");
            println!("Optional: set PIKACHAT_START_SYNC=1 to exercise StartSync/StopSync.");
            println!(
                "Optional sync trace: PIKACHAT_SYNC_VERBOSE=1, PIKACHAT_WATCH_ROOM_ID='!room:example.org'."
            );
            println!(
                "Optional tuning: PIKACHAT_SYNC_REQUEST_TIMEOUT_MS, PIKACHAT_OPEN_ROOM_LIMIT, PIKACHAT_PAGINATION_LIMIT_CAP."
            );
            println!(
                "Optional media smoke: PIKACHAT_MEDIA_UPLOAD_FILE, PIKACHAT_MEDIA_CONTENT_TYPE, PIKACHAT_MEDIA_DOWNLOAD_MXC."
            );
            println!(
                "Optional recovery smoke: PIKACHAT_GET_RECOVERY_STATUS=1, PIKACHAT_ENABLE_RECOVERY=1, PIKACHAT_RESET_RECOVERY=1, PIKACHAT_RECOVERY_PASSPHRASE, PIKACHAT_WAIT_RECOVERY_UPLOAD=1, PIKACHAT_RECOVERY_KEY."
            );
            return Ok(());
        }

        if env_truthy("PIKACHAT_GET_RECOVERY_STATUS") {
            send_command(&adapter, BackendCommand::GetRecoveryStatus, "GetRecoveryStatus").await?;
            let status = wait_for_recovery_status(&mut events).await?;
            println!(
                "Recovery status: backup_state={:?}, backup_enabled={}, backup_exists_on_server={}, recovery_state={:?}",
                status.backup_state,
                status.backup_enabled,
                status.backup_exists_on_server,
                status.recovery_state
            );
        }

        if env_truthy("PIKACHAT_ENABLE_RECOVERY") {
            let client_txn_id = format!("smoke-enable-recovery-{}", monotonic_nonce());
            let passphrase = env::var("PIKACHAT_RECOVERY_PASSPHRASE").ok();
            let wait_for_backups_to_upload = env_truthy("PIKACHAT_WAIT_RECOVERY_UPLOAD");
            send_command(
                &adapter,
                BackendCommand::EnableRecovery {
                    client_txn_id: client_txn_id.clone(),
                    passphrase,
                    wait_for_backups_to_upload,
                },
                "EnableRecovery",
            )
            .await?;

            let recovery_key = wait_for_recovery_enable_ack(&mut events, &client_txn_id).await?;
            println!("Recovery enabled. Recovery key: {recovery_key}");
        }

        if env_truthy("PIKACHAT_RESET_RECOVERY") {
            let client_txn_id = format!("smoke-reset-recovery-{}", monotonic_nonce());
            let passphrase = env::var("PIKACHAT_RECOVERY_PASSPHRASE").ok();
            let wait_for_backups_to_upload = env_truthy("PIKACHAT_WAIT_RECOVERY_UPLOAD");
            send_command(
                &adapter,
                BackendCommand::ResetRecovery {
                    client_txn_id: client_txn_id.clone(),
                    passphrase,
                    wait_for_backups_to_upload,
                },
                "ResetRecovery",
            )
            .await?;

            let recovery_key = wait_for_recovery_enable_ack(&mut events, &client_txn_id).await?;
            println!("Recovery reset complete. New recovery key: {recovery_key}");
        }

        if let Some(recovery_key) = env::var("PIKACHAT_RECOVERY_KEY").ok() {
            let client_txn_id = format!("smoke-recover-secrets-{}", monotonic_nonce());
            send_command(
                &adapter,
                BackendCommand::RecoverSecrets {
                    client_txn_id: client_txn_id.clone(),
                    recovery_key,
                },
                "RecoverSecrets",
            )
            .await?;
            wait_for_recovery_restore_ack(&mut events, &client_txn_id).await?;
            println!("Recovery restore succeeded.");
        }

        send_command(&adapter, BackendCommand::ListRooms, "ListRooms").await?;
        let rooms = wait_for_room_list(&mut events).await?;
        println!("Loaded {rooms} rooms from backend.");

        if env_truthy("PIKACHAT_START_SYNC") {
            send_command(&adapter, BackendCommand::StartSync, "StartSync").await?;
            wait_for_state(&mut events, BackendLifecycleState::Syncing, "syncing").await?;
            println!("Sync started.");

            let sync_verbose = env_truthy("PIKACHAT_SYNC_VERBOSE");
            let watch_room_id = env::var("PIKACHAT_WATCH_ROOM_ID").ok();
            let observe_secs = env::var("PIKACHAT_SYNC_OBSERVE_SECS")
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(DEFAULT_SYNC_OBSERVE_SECS);
            let stats = observe_sync_events(
                &mut events,
                Duration::from_secs(observe_secs),
                sync_verbose,
                watch_room_id.as_deref(),
            )
            .await?;
            println!(
                "Observed sync for {}s: room_list_updates={}, timeline_deltas={}, append_ops={}, update_ops={}, remove_ops={}, clear_ops={}",
                observe_secs,
                stats.room_list_updates,
                stats.timeline_deltas,
                stats.append_ops,
                stats.update_ops,
                stats.remove_ops,
                stats.clear_ops
            );

            send_command(&adapter, BackendCommand::StopSync, "StopSync").await?;
            wait_for_state(
                &mut events,
                BackendLifecycleState::Authenticated,
                "authenticated after stop sync",
            )
            .await?;
            println!("Sync stopped.");
        }

        if let Some(dm_target) = env::var("PIKACHAT_DM_TARGET").ok() {
            let dm_body =
                env::var("PIKACHAT_DM_BODY").unwrap_or_else(|_| "PikaChat backend smoke test".to_owned());
            let client_txn_id = format!("smoke-dm-{}", monotonic_nonce());

            send_command(
                &adapter,
                BackendCommand::SendDmText {
                    user_id: dm_target.clone(),
                    client_txn_id: client_txn_id.clone(),
                    body: dm_body,
                },
                "SendDmText",
            )
            .await?;

            let event_id = wait_for_send_ack(&mut events, &client_txn_id).await?;
            println!("Sent DM to {dm_target} as event {event_id}");
        }

        let mut uploaded_media: Option<(String, Vec<u8>)> = None;
        if let Some(upload_path) = env::var("PIKACHAT_MEDIA_UPLOAD_FILE").ok() {
            let upload_bytes =
                fs::read(&upload_path).map_err(|err| format!("failed to read media file '{upload_path}': {err}"))?;
            let content_type = env::var("PIKACHAT_MEDIA_CONTENT_TYPE")
                .unwrap_or_else(|_| "application/octet-stream".to_owned());
            let client_txn_id = format!("smoke-upload-{}", monotonic_nonce());

            send_command(
                &adapter,
                BackendCommand::UploadMedia {
                    client_txn_id: client_txn_id.clone(),
                    content_type,
                    data: upload_bytes.clone(),
                },
                "UploadMedia",
            )
            .await?;

            let content_uri = wait_for_media_upload_ack(&mut events, &client_txn_id).await?;
            println!(
                "Uploaded media from {} ({} bytes) -> {}",
                upload_path,
                upload_bytes.len(),
                content_uri
            );
            uploaded_media = Some((content_uri, upload_bytes));
        }

        let explicit_download_uri = env::var("PIKACHAT_MEDIA_DOWNLOAD_MXC").ok();
        let download_uri = explicit_download_uri
            .or_else(|| uploaded_media.as_ref().map(|(uri, _)| uri.clone()));

        if let Some(download_uri) = download_uri {
            let client_txn_id = format!("smoke-download-{}", monotonic_nonce());
            send_command(
                &adapter,
                BackendCommand::DownloadMedia {
                    client_txn_id: client_txn_id.clone(),
                    source: download_uri.clone(),
                },
                "DownloadMedia",
            )
            .await?;

            let downloaded = wait_for_media_download_ack(&mut events, &client_txn_id).await?;
            println!(
                "Downloaded media {} ({} bytes)",
                download_uri,
                downloaded.len()
            );

            if let Some((uploaded_uri, uploaded_bytes)) = uploaded_media
                && uploaded_uri == download_uri
            {
                if uploaded_bytes == downloaded {
                    println!("Media roundtrip verified: upload and download bytes match.");
                } else {
                    println!("Media roundtrip mismatch: uploaded and downloaded bytes differ.");
                }
            }
        }

        Ok(())
    }
    .await;

    adapter.shutdown().await;
    result
}

fn init_logging() {
    let env_filter = if let Ok(filter) = EnvFilter::try_from_default_env() {
        filter
    } else if let Some(value) = env::var("PIKACHAT_SMOKE_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        EnvFilter::try_new(value)
            .unwrap_or_else(|_| EnvFilter::new("info,backend_smoke=debug,backend_matrix=debug"))
    } else if let Some(value) = env::var("PIKACHAT_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        EnvFilter::try_new(value)
            .unwrap_or_else(|_| EnvFilter::new("info,backend_smoke=debug,backend_matrix=debug"))
    } else {
        EnvFilter::new("info,backend_smoke=debug,backend_matrix=debug")
    };

    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_env_filter(env_filter)
        .try_init();
}

async fn send_command(
    adapter: &MatrixFrontendAdapter,
    command: BackendCommand,
    label: &str,
) -> Result<(), String> {
    let command_id = adapter.enqueue(command);
    adapter
        .flush_one()
        .await
        .map_err(|err| format!("failed to flush {label} (id={}): {err}", command_id.value()))?;
    Ok(())
}

async fn wait_for_state(
    events: &mut EventStream,
    expected: BackendLifecycleState,
    label: &str,
) -> Result<(), String> {
    wait_for_event(events, label, |event| match event {
        BackendEvent::StateChanged { state } if state == expected => Some(Ok(())),
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_auth_success(events: &mut EventStream, label: &str) -> Result<(), String> {
    wait_for_event(events, label, |event| match event {
        BackendEvent::AuthResult {
            success,
            error_code,
        } => {
            if success {
                Some(Ok(()))
            } else {
                let code = error_code.unwrap_or_else(|| "unknown".to_owned());
                Some(Err(format!("auth failed with error code: {code}")))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_room_list(events: &mut EventStream) -> Result<usize, String> {
    wait_for_event(events, "room list", |event| match event {
        BackendEvent::RoomListUpdated { rooms } => Some(Ok(rooms.len())),
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_send_ack(
    events: &mut EventStream,
    client_txn_id: &str,
) -> Result<String, String> {
    wait_for_event(events, "send ack", |event| match event {
        BackendEvent::SendAck(ack) if ack.client_txn_id == client_txn_id => {
            if let Some(event_id) = ack.event_id {
                Some(Ok(event_id))
            } else {
                let code = ack.error_code.unwrap_or_else(|| "unknown".to_owned());
                Some(Err(format!("send failed with error code: {code}")))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_media_upload_ack(
    events: &mut EventStream,
    client_txn_id: &str,
) -> Result<String, String> {
    wait_for_event(events, "media upload ack", |event| match event {
        BackendEvent::MediaUploadAck(ack) if ack.client_txn_id == client_txn_id => {
            if let Some(content_uri) = ack.content_uri {
                Some(Ok(content_uri))
            } else {
                let code = ack.error_code.unwrap_or_else(|| "unknown".to_owned());
                Some(Err(format!("media upload failed with error code: {code}")))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_media_download_ack(
    events: &mut EventStream,
    client_txn_id: &str,
) -> Result<Vec<u8>, String> {
    wait_for_event(events, "media download ack", |event| match event {
        BackendEvent::MediaDownloadAck(ack) if ack.client_txn_id == client_txn_id => {
            if let Some(data) = ack.data {
                Some(Ok(data))
            } else {
                let code = ack.error_code.unwrap_or_else(|| "unknown".to_owned());
                Some(Err(format!(
                    "media download failed with error code: {code}"
                )))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_recovery_status(events: &mut EventStream) -> Result<RecoveryStatus, String> {
    wait_for_event(events, "recovery status", |event| match event {
        BackendEvent::RecoveryStatus(status) => Some(Ok(status)),
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_recovery_enable_ack(
    events: &mut EventStream,
    client_txn_id: &str,
) -> Result<String, String> {
    wait_for_event(events, "recovery enable ack", |event| match event {
        BackendEvent::RecoveryEnableAck(ack) if ack.client_txn_id == client_txn_id => {
            if let Some(recovery_key) = ack.recovery_key {
                Some(Ok(recovery_key))
            } else {
                let code = ack.error_code.unwrap_or_else(|| "unknown".to_owned());
                Some(Err(format!(
                    "recovery enable failed with error code: {code}"
                )))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

async fn wait_for_recovery_restore_ack(
    events: &mut EventStream,
    client_txn_id: &str,
) -> Result<(), String> {
    wait_for_event(events, "recovery restore ack", |event| match event {
        BackendEvent::RecoveryRestoreAck(ack) if ack.client_txn_id == client_txn_id => {
            if let Some(code) = ack.error_code {
                Some(Err(format!(
                    "recovery restore failed with error code: {code}"
                )))
            } else {
                Some(Ok(()))
            }
        }
        BackendEvent::FatalError { code, message, .. } => {
            Some(Err(format!("fatal backend error ({code}): {message}")))
        }
        _ => None,
    })
    .await
}

#[derive(Debug, Default)]
struct SyncObserveStats {
    room_list_updates: usize,
    timeline_deltas: usize,
    append_ops: usize,
    update_ops: usize,
    remove_ops: usize,
    clear_ops: usize,
}

async fn observe_sync_events(
    events: &mut EventStream,
    duration: Duration,
    verbose: bool,
    watch_room_id: Option<&str>,
) -> Result<SyncObserveStats, String> {
    let deadline = tokio::time::Instant::now() + duration;
    let mut stats = SyncObserveStats::default();

    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);

        let event = match timeout(remaining, recv_event(events)).await {
            Ok(result) => result?,
            Err(_) => break,
        };

        match event {
            BackendEvent::RoomListUpdated { .. } => {
                stats.room_list_updates += 1;
            }
            BackendEvent::RoomTimelineDelta { room_id, ops } => {
                if watch_room_id.is_some_and(|watch| watch != room_id) {
                    continue;
                }

                stats.timeline_deltas += 1;
                for op in ops {
                    match op {
                        backend_core::TimelineOp::Append(item) => {
                            stats.append_ops += 1;
                            if verbose {
                                println!(
                                    "sync delta room={} op=append event_id={:?} sender={} body={}",
                                    room_id, item.event_id, item.sender, item.body
                                );
                            }
                        }
                        backend_core::TimelineOp::Prepend(item) => {
                            if verbose {
                                println!(
                                    "sync delta room={} op=prepend event_id={:?} sender={} body={}",
                                    room_id, item.event_id, item.sender, item.body
                                );
                            }
                        }
                        backend_core::TimelineOp::UpdateBody { event_id, new_body } => {
                            stats.update_ops += 1;
                            if verbose {
                                println!(
                                    "sync delta room={} op=update event_id={} body={}",
                                    room_id, event_id, new_body
                                );
                            }
                        }
                        backend_core::TimelineOp::Remove { event_id } => {
                            stats.remove_ops += 1;
                            if verbose {
                                println!(
                                    "sync delta room={} op=remove event_id={}",
                                    room_id, event_id
                                );
                            }
                        }
                        backend_core::TimelineOp::Clear => {
                            stats.clear_ops += 1;
                            if verbose {
                                println!("sync delta room={} op=clear", room_id);
                            }
                        }
                    }
                }
            }
            BackendEvent::FatalError { code, message, .. } => {
                return Err(format!("fatal backend error ({code}): {message}"));
            }
            _ => {}
        }
    }

    Ok(stats)
}

async fn wait_for_event<T, F>(
    events: &mut EventStream,
    label: &str,
    mut matcher: F,
) -> Result<T, String>
where
    F: FnMut(BackendEvent) -> Option<Result<T, String>>,
{
    timeout(EVENT_WAIT_TIMEOUT, async {
        loop {
            let event = recv_event(events).await?;
            if let Some(result) = matcher(event) {
                return result;
            }
        }
    })
    .await
    .map_err(|_| format!("timed out waiting for {label}"))?
}

async fn recv_event(events: &mut EventStream) -> Result<BackendEvent, String> {
    loop {
        match events.recv().await {
            Ok(event) => return Ok(event),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                eprintln!("event stream lagged by {skipped} messages, continuing");
            }
            Err(broadcast::error::RecvError::Closed) => {
                return Err("event channel closed".to_owned());
            }
        }
    }
}

fn env_truthy(key: &str) -> bool {
    env::var(key)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
        .unwrap_or(false)
}

fn init_config_from_env() -> Result<Option<BackendInitConfig>, String> {
    let sync_request_timeout_ms = parse_optional_u64_env("PIKACHAT_SYNC_REQUEST_TIMEOUT_MS")?;
    let default_open_room_limit = parse_optional_u16_env("PIKACHAT_OPEN_ROOM_LIMIT")?;
    let pagination_limit_cap = parse_optional_u16_env("PIKACHAT_PAGINATION_LIMIT_CAP")?;

    if sync_request_timeout_ms.is_none()
        && default_open_room_limit.is_none()
        && pagination_limit_cap.is_none()
    {
        return Ok(None);
    }

    Ok(Some(BackendInitConfig {
        sync_request_timeout_ms,
        default_open_room_limit,
        pagination_limit_cap,
    }))
}

fn parse_optional_u64_env(key: &str) -> Result<Option<u64>, String> {
    match env::var(key) {
        Ok(value) => value
            .parse::<u64>()
            .map(Some)
            .map_err(|err| format!("invalid {key}='{value}': {err}")),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!("{key} contains non-unicode data")),
    }
}

fn parse_optional_u16_env(key: &str) -> Result<Option<u16>, String> {
    match env::var(key) {
        Ok(value) => value
            .parse::<u16>()
            .map(Some)
            .map_err(|err| format!("invalid {key}='{value}': {err}")),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!("{key} contains non-unicode data")),
    }
}

fn monotonic_nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
