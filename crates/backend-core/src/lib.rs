//! Core backend contract shared between runtime and frontend consumers.
//!
//! This crate defines the command/event protocol, lifecycle model, retry and
//! timeline helpers, and common error/channel abstractions.

/// Async command/event channel primitives.
pub mod channel;
/// Stable backend error types and HTTP classification helpers.
pub mod error;
/// Event normalization helpers (for example send acknowledgements).
pub mod normalization;
/// Backoff policy used by retry loops.
pub mod retry;
/// Backend lifecycle state machine.
pub mod state_machine;
/// Timeline merge buffer utilities.
pub mod timeline;
/// Frontend-facing protocol types (commands, events, payloads).
pub mod types;

pub use channel::{BackendChannelError, BackendChannels, EventStream};
pub use error::{BackendError, BackendErrorCategory, classify_http_status};
pub use normalization::{SendOutcome, normalize_send_outcome};
pub use retry::RetryPolicy;
pub use state_machine::BackendStateMachine;
pub use timeline::{TimelineBuffer, TimelineMergeError};
pub use types::{
    BackendCommand, BackendEvent, BackendInitConfig, BackendLifecycleState, CryptoStatus,
    EncryptedFileSource, KeyBackupState, MediaDownloadAck, MediaSourceRef, MediaUploadAck,
    MessageType, OutgoingMedia, RecoveryEnableAck, RecoveryRestoreAck, RecoveryState,
    RecoveryStatus, RoomSummary, SendAck, SyncStatus, TimelineContent, TimelineImageContent,
    TimelineImageMetadata, TimelineItem, TimelineOp,
};
