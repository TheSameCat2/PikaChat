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

/// Membership classification for room-list entries.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RoomMembership {
    /// The user has joined the room.
    Joined,
    /// The user has a pending invite for the room.
    Invited,
}

/// Invite action requested by the frontend.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum InviteAction {
    /// Accept a pending room invite.
    Accept,
    /// Reject a pending room invite.
    Reject,
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
        /// Whether the authenticated session should be persisted for restore.
        persist_session: bool,
    },
    /// Attempt session restore from persisted session data.
    RestoreSession,
    /// Start continuous sync loop.
    StartSync,
    /// Stop continuous sync loop.
    StopSync,
    /// Emit latest room list snapshot.
    ListRooms,
    /// Accept a pending room invite.
    AcceptRoomInvite {
        /// Target room ID.
        room_id: String,
        /// Frontend-provided transaction ID echoed in `InviteActionAck`.
        client_txn_id: String,
    },
    /// Reject a pending room invite.
    RejectRoomInvite {
        /// Target room ID.
        room_id: String,
        /// Frontend-provided transaction ID echoed in `InviteActionAck`.
        client_txn_id: String,
    },
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
    /// Fetch latest local/server key-backup and recovery status summary.
    GetRecoveryStatus,
    /// Enable recovery and server-side room-key backups.
    ///
    /// On success emits `BackendEvent::RecoveryEnableAck` with a generated
    /// recovery key that should be shown to the user and stored safely.
    EnableRecovery {
        /// Frontend-provided transaction ID echoed in `RecoveryEnableAck`.
        client_txn_id: String,
        /// Optional passphrase to derive secret-storage/recovery material.
        passphrase: Option<String>,
        /// Whether to wait for initial room-key upload to settle before acking.
        wait_for_backups_to_upload: bool,
    },
    /// Reset identity backup/recovery and create a new recovery key.
    ///
    /// This flow deletes the active server-side backup and creates a fresh
    /// backup/recovery setup. Old encrypted history that depended on the prior
    /// backup key may become unrecoverable if that key was lost.
    ///
    /// On success emits `BackendEvent::RecoveryEnableAck` with the new
    /// generated recovery key.
    ResetRecovery {
        /// Frontend-provided transaction ID echoed in `RecoveryEnableAck`.
        client_txn_id: String,
        /// Optional passphrase to derive secret-storage/recovery material.
        passphrase: Option<String>,
        /// Whether to wait for initial room-key upload to settle before acking.
        wait_for_backups_to_upload: bool,
    },
    /// Recover E2EE secrets from secret storage using a recovery key/passphrase.
    RecoverSecrets {
        /// Frontend-provided transaction ID echoed in `RecoveryRestoreAck`.
        client_txn_id: String,
        /// Recovery key or passphrase used to unlock secret storage.
        recovery_key: String,
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
    /// Current membership classification for this room.
    pub membership: RoomMembership,
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

/// Acknowledgement for invite accept/reject requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InviteActionAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
    /// Target room ID.
    pub room_id: String,
    /// Invite action requested.
    pub action: InviteAction,
    /// Stable backend error code on failure.
    pub error_code: Option<String>,
}

/// Coarse-grained key-backup state projected from Matrix SDK internals.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum KeyBackupState {
    /// Backup state has not been determined yet.
    Unknown,
    /// Client is creating a new backup on the homeserver.
    Creating,
    /// Client is enabling an existing backup.
    Enabling,
    /// Client is resuming backup from existing local state.
    Resuming,
    /// Backup is enabled and usable.
    Enabled,
    /// Client is downloading keys from backup.
    Downloading,
    /// Client is disabling/deleting backup.
    Disabling,
}

/// Coarse-grained secret-storage recovery state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RecoveryState {
    /// Recovery state has not been determined yet.
    Unknown,
    /// Recovery is fully configured and usable.
    Enabled,
    /// Recovery/secret-storage is disabled.
    Disabled,
    /// Recovery is partially configured but missing some secrets.
    Incomplete,
}

/// Consolidated backup/recovery readiness summary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryStatus {
    /// Current local key-backup state.
    pub backup_state: KeyBackupState,
    /// Whether this client currently has backups enabled locally.
    pub backup_enabled: bool,
    /// Whether a backup currently exists on the homeserver.
    pub backup_exists_on_server: bool,
    /// Current recovery/secret-storage state.
    pub recovery_state: RecoveryState,
}

/// Acknowledgement for recovery enable requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryEnableAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
    /// Generated recovery key/passphrase on success.
    pub recovery_key: Option<String>,
    /// Stable backend error code on failure.
    pub error_code: Option<String>,
}

/// Acknowledgement for recovery restore requests.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecoveryRestoreAck {
    /// Original frontend transaction ID.
    pub client_txn_id: String,
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
    /// Invite accept/reject acknowledgement.
    InviteActionAck(InviteActionAck),
    /// Recovery/key-backup status snapshot.
    RecoveryStatus(RecoveryStatus),
    /// Recovery setup acknowledgement.
    RecoveryEnableAck(RecoveryEnableAck),
    /// Recovery restore acknowledgement.
    RecoveryRestoreAck(RecoveryRestoreAck),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_summary_carries_membership() {
        let summary = RoomSummary {
            room_id: "!abc:example.org".into(),
            name: Some("Room".into()),
            unread_notifications: 1,
            highlight_count: 0,
            is_direct: false,
            membership: RoomMembership::Invited,
        };

        assert_eq!(summary.membership, RoomMembership::Invited);
    }

    #[test]
    fn invite_command_variants_include_txn_and_room() {
        let accept = BackendCommand::AcceptRoomInvite {
            room_id: "!a:example.org".into(),
            client_txn_id: "txn-accept".into(),
        };
        let reject = BackendCommand::RejectRoomInvite {
            room_id: "!b:example.org".into(),
            client_txn_id: "txn-reject".into(),
        };

        match accept {
            BackendCommand::AcceptRoomInvite {
                room_id,
                client_txn_id,
            } => {
                assert_eq!(room_id, "!a:example.org");
                assert_eq!(client_txn_id, "txn-accept");
            }
            _ => panic!("expected AcceptRoomInvite"),
        }

        match reject {
            BackendCommand::RejectRoomInvite {
                room_id,
                client_txn_id,
            } => {
                assert_eq!(room_id, "!b:example.org");
                assert_eq!(client_txn_id, "txn-reject");
            }
            _ => panic!("expected RejectRoomInvite"),
        }
    }

    #[test]
    fn invite_action_ack_event_exposes_contract_fields() {
        let event = BackendEvent::InviteActionAck(InviteActionAck {
            client_txn_id: "txn-1".into(),
            room_id: "!abc:example.org".into(),
            action: InviteAction::Reject,
            error_code: Some("not_invited".into()),
        });

        match event {
            BackendEvent::InviteActionAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-1");
                assert_eq!(ack.room_id, "!abc:example.org");
                assert_eq!(ack.action, InviteAction::Reject);
                assert_eq!(ack.error_code.as_deref(), Some("not_invited"));
            }
            _ => panic!("expected InviteActionAck event"),
        }
    }
}
