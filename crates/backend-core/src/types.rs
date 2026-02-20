use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// High-level backend lifecycle state reported to the frontend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendLifecycleState {
    /// Backend has not been initialized yet.
    Cold,
    /// Backend has accepted `Init` and basic configuration is loaded.
    Configured,
    /// A login or session restore flow is currently running.
    Authenticating,
    /// Backend is authenticated and ready for room/media commands.
    Authenticated,
    /// Backend sync loop is running.
    Syncing,
    /// Backend logged out and cleared authenticated session state.
    LoggedOut,
    /// Backend entered unrecoverable fatal state.
    Fatal,
}

/// Matrix message type used when sending room messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageType {
    /// Standard text message (`m.text`).
    Text,
    /// Notice message (`m.notice`), usually non-intrusive/system-like.
    Notice,
    /// Emote message (`m.emote`).
    Emote,
}

/// Optional runtime tuning values supplied with `BackendCommand::Init`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct BackendInitConfig {
    /// Optional `/sync` request timeout in milliseconds.
    ///
    /// When `None`, backend default behavior is used.
    pub sync_request_timeout_ms: Option<u64>,
    /// Optional default room-history limit for `OpenRoom`.
    pub default_open_room_limit: Option<u16>,
    /// Optional hard cap used when paginating backward.
    pub pagination_limit_cap: Option<u16>,
}

/// Command channel input accepted by the backend runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendCommand {
    /// Initialize backend with homeserver and local data directory.
    Init {
        /// Homeserver base URL, for example `https://matrix.example.org`.
        homeserver: String,
        /// Local data directory used by the SDK store.
        data_dir: PathBuf,
        /// Optional runtime tuning overrides.
        config: Option<BackendInitConfig>,
    },
    /// Login with Matrix user ID/localpart and password.
    LoginPassword {
        /// Matrix user ID (or localpart depending on homeserver behavior).
        user_id_or_localpart: String,
        /// Password for the user account.
        password: String,
    },
    /// Attempt session restore from persisted session data.
    RestoreSession,
    /// Start continuous sync loop.
    StartSync,
    /// Stop continuous sync loop.
    StopSync,
    /// Emit latest room list snapshot.
    ListRooms,
    /// Load initial room timeline chunk.
    OpenRoom {
        /// Target room ID.
        room_id: String,
    },
    /// Paginate backward in a room timeline.
    PaginateBack {
        /// Target room ID.
        room_id: String,
        /// Requested pagination limit (subject to backend caps).
        limit: u16,
    },
    /// Send a text DM to a user.
    SendDmText {
        /// DM target user ID.
        user_id: String,
        /// Frontend-provided transaction ID echoed in `SendAck`.
        client_txn_id: String,
        /// Message body.
        body: String,
    },
    /// Send a room message.
    SendMessage {
        /// Target room ID.
        room_id: String,
        /// Frontend-provided transaction ID echoed in `SendAck`.
        client_txn_id: String,
        /// Message body.
        body: String,
        /// Message kind (`m.text`/`m.notice`/`m.emote`).
        msgtype: MessageType,
    },
    /// Edit an existing room message.
    EditMessage {
        /// Target room ID.
        room_id: String,
        /// Event ID of the message being edited.
        target_event_id: String,
        /// Replacement message body.
        new_body: String,
        /// Frontend-provided transaction ID echoed in `SendAck`.
        client_txn_id: String,
    },
    /// Redact (remove) an existing room event.
    RedactMessage {
        /// Target room ID.
        room_id: String,
        /// Event ID to redact.
        target_event_id: String,
        /// Optional redaction reason.
        reason: Option<String>,
    },
    /// Upload media bytes to the homeserver.
    UploadMedia {
        /// Frontend-provided transaction ID echoed in `MediaUploadAck`.
        client_txn_id: String,
        /// MIME content type, for example `image/png`.
        content_type: String,
        /// Raw media bytes.
        data: Vec<u8>,
    },
    /// Download media content from an `mxc://` source.
    DownloadMedia {
        /// Frontend-provided transaction ID echoed in `MediaDownloadAck`.
        client_txn_id: String,
        /// `mxc://` URI source.
        source: String,
    },
    /// Logout and clear persisted session state.
    Logout,
}

/// Lightweight room metadata for frontend room lists.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoomSummary {
    /// Matrix room ID.
    pub room_id: String,
    /// Best-effort display name for the room.
    pub name: Option<String>,
    /// Notification count reported by sync.
    pub unread_notifications: u64,
    /// Highlight/mention count reported by sync.
    pub highlight_count: u64,
    /// Whether the room is considered a direct message room.
    pub is_direct: bool,
}

/// Canonical timeline item payload used by timeline snapshots/deltas.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineItem {
    /// Event ID when available.
    pub event_id: Option<String>,
    /// Sender user ID.
    pub sender: String,
    /// Display-ready text body.
    pub body: String,
    /// Event timestamp in milliseconds since Unix epoch.
    pub timestamp_ms: u64,
}

/// Incremental timeline operation applied by frontend timeline stores.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TimelineOp {
    /// Append item at the end of the timeline.
    Append(TimelineItem),
    /// Prepend item at the beginning of the timeline.
    Prepend(TimelineItem),
    /// Update body of an existing event.
    UpdateBody { event_id: String, new_body: String },
    /// Remove an existing event.
    Remove { event_id: String },
    /// Clear all timeline items.
    Clear,
}

/// Sync loop status updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncStatus {
    /// Whether sync is currently running.
    pub running: bool,
    /// Optional hint about next retry delay.
    pub lag_hint_ms: Option<u64>,
}

/// Acknowledgement for message/DM send commands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SendAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
    /// Event ID on success.
    pub event_id: Option<String>,
    /// Stable backend error code on failure.
    pub error_code: Option<String>,
}

/// Acknowledgement for media upload requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaUploadAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
    /// Uploaded media URI (`mxc://...`) on success.
    pub content_uri: Option<String>,
    /// Stable backend error code on failure.
    pub error_code: Option<String>,
}

/// Acknowledgement for media download requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaDownloadAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
    /// Requested source URI.
    pub source: String,
    /// Downloaded bytes on success.
    pub data: Option<Vec<u8>>,
    /// Optional MIME content type when known.
    pub content_type: Option<String>,
    /// Stable backend error code on failure.
    pub error_code: Option<String>,
}

/// E2EE/trust status summary intended for future frontend consumption.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CryptoStatus {
    /// Whether cross-signing is configured/ready.
    pub cross_signing_ready: bool,
    /// Number of trusted devices.
    pub trusted_devices: u64,
    /// Number of untrusted devices.
    pub untrusted_devices: u64,
}

/// Event channel output emitted by backend runtime and adapter layers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendEvent {
    /// Backend lifecycle transition.
    StateChanged {
        /// New lifecycle state.
        state: BackendLifecycleState,
    },
    /// Result of login/session-restore flow.
    AuthResult {
        /// `true` when auth flow completed successfully.
        success: bool,
        /// Stable backend error code when `success == false`.
        error_code: Option<String>,
    },
    /// Sync loop status update.
    SyncStatus(SyncStatus),
    /// Full room list replacement.
    RoomListUpdated {
        /// Latest room summaries.
        rooms: Vec<RoomSummary>,
    },
    /// Incremental timeline delta for a room.
    RoomTimelineDelta {
        /// Target room ID.
        room_id: String,
        /// Operations to apply in order.
        ops: Vec<TimelineOp>,
    },
    /// Snapshot timeline replacement for a room (adapter-level convenience event).
    RoomTimelineSnapshot {
        /// Target room ID.
        room_id: String,
        /// Snapshot items in display order.
        items: Vec<TimelineItem>,
    },
    /// Send acknowledgement (`SendMessage`, `SendDmText`, `EditMessage`).
    SendAck(SendAck),
    /// Media upload acknowledgement.
    MediaUploadAck(MediaUploadAck),
    /// Media download acknowledgement.
    MediaDownloadAck(MediaDownloadAck),
    /// Crypto status update.
    CryptoStatus(CryptoStatus),
    /// Fatal runtime error.
    FatalError {
        /// Stable backend error code.
        code: String,
        /// Human-readable error message.
        message: String,
        /// Indicates whether retrying may recover.
        recoverable: bool,
    },
}
