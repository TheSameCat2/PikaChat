use std::{
    env::{self, VarError},
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use backend_core::{
    BackendCommand, BackendEvent, BackendInitConfig, BackendLifecycleState, EventStream,
};
use backend_matrix::spawn_runtime;
use tokio::{sync::broadcast, time::timeout};

const EVENT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_SYNC_OBSERVE_SECS: u64 = 8;

#[tokio::main]
async fn main() {
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

    let runtime = spawn_runtime();
    let mut events = runtime.subscribe();

    send_command(
        &runtime,
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
        send_command(&runtime, BackendCommand::RestoreSession, "RestoreSession").await?;
        wait_for_auth_success(&mut events, "restore session").await?;
        println!("Session restored successfully.");
    } else if let (Some(user), Some(password)) = (maybe_user, maybe_password) {
        send_command(
            &runtime,
            BackendCommand::LoginPassword {
                user_id_or_localpart: user.clone(),
                password,
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
            "Optional tuning: PIKACHAT_SYNC_REQUEST_TIMEOUT_MS, PIKACHAT_OPEN_ROOM_LIMIT, PIKACHAT_PAGINATION_LIMIT_CAP."
        );
        return Ok(());
    }

    send_command(&runtime, BackendCommand::ListRooms, "ListRooms").await?;
    let rooms = wait_for_room_list(&mut events).await?;
    println!("Loaded {rooms} rooms from backend.");

    if env_truthy("PIKACHAT_START_SYNC") {
        send_command(&runtime, BackendCommand::StartSync, "StartSync").await?;
        wait_for_state(&mut events, BackendLifecycleState::Syncing, "syncing").await?;
        println!("Sync started.");

        let observe_secs = env::var("PIKACHAT_SYNC_OBSERVE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SYNC_OBSERVE_SECS);
        let stats = observe_sync_events(&mut events, Duration::from_secs(observe_secs)).await?;
        println!(
            "Observed sync for {}s: room_list_updates={}, timeline_deltas={}",
            observe_secs, stats.room_list_updates, stats.timeline_deltas
        );

        send_command(&runtime, BackendCommand::StopSync, "StopSync").await?;
        wait_for_state(
            &mut events,
            BackendLifecycleState::Authenticated,
            "authenticated after stop sync",
        )
        .await?;
        println!("Sync stopped.");
    }

    if let Some(dm_target) = env::var("PIKACHAT_DM_TARGET").ok() {
        let dm_body = env::var("PIKACHAT_DM_BODY")
            .unwrap_or_else(|_| "PikaChat backend smoke test".to_owned());
        let client_txn_id = format!("smoke-dm-{}", monotonic_nonce());

        send_command(
            &runtime,
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

    Ok(())
}

async fn send_command(
    runtime: &backend_matrix::MatrixRuntimeHandle,
    command: BackendCommand,
    label: &str,
) -> Result<(), String> {
    runtime
        .send(command)
        .await
        .map_err(|err| format!("failed to enqueue {label}: {err}"))
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

#[derive(Debug, Default)]
struct SyncObserveStats {
    room_list_updates: usize,
    timeline_deltas: usize,
}

async fn observe_sync_events(
    events: &mut EventStream,
    duration: Duration,
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
            BackendEvent::RoomTimelineDelta { .. } => {
                stats.timeline_deltas += 1;
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
