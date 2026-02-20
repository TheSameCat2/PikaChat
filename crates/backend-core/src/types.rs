use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendLifecycleState {
    Cold,
    Configured,
    Authenticating,
    Authenticated,
    Syncing,
    LoggedOut,
    Fatal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageType {
    Text,
    Notice,
    Emote,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendCommand {
    Init {
        homeserver: String,
        data_dir: PathBuf,
    },
    LoginPassword {
        user_id_or_localpart: String,
        password: String,
    },
    RestoreSession,
    StartSync,
    StopSync,
    ListRooms,
    OpenRoom {
        room_id: String,
    },
    PaginateBack {
        room_id: String,
        limit: u16,
    },
    SendMessage {
        room_id: String,
        client_txn_id: String,
        body: String,
        msgtype: MessageType,
    },
    EditMessage {
        room_id: String,
        target_event_id: String,
        new_body: String,
        client_txn_id: String,
    },
    RedactMessage {
        room_id: String,
        target_event_id: String,
        reason: Option<String>,
    },
    Logout,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomSummary {
    pub room_id: String,
    pub name: Option<String>,
    pub unread_notifications: u64,
    pub highlight_count: u64,
    pub is_direct: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineItem {
    pub event_id: Option<String>,
    pub sender: String,
    pub body: String,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TimelineOp {
    Append(TimelineItem),
    Prepend(TimelineItem),
    UpdateBody { event_id: String, new_body: String },
    Remove { event_id: String },
    Clear,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatus {
    pub running: bool,
    pub lag_hint_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendAck {
    pub client_txn_id: String,
    pub event_id: Option<String>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CryptoStatus {
    pub cross_signing_ready: bool,
    pub trusted_devices: u64,
    pub untrusted_devices: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendEvent {
    StateChanged {
        state: BackendLifecycleState,
    },
    AuthResult {
        success: bool,
        error_code: Option<String>,
    },
    SyncStatus(SyncStatus),
    RoomListUpdated {
        rooms: Vec<RoomSummary>,
    },
    RoomTimelineDelta {
        room_id: String,
        ops: Vec<TimelineOp>,
    },
    SendAck(SendAck),
    CryptoStatus(CryptoStatus),
    FatalError {
        code: String,
        message: String,
        recoverable: bool,
    },
}
