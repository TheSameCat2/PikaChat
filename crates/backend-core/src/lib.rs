pub mod channel;
pub mod error;
pub mod normalization;
pub mod retry;
pub mod state_machine;
pub mod timeline;
pub mod types;

pub use channel::{BackendChannelError, BackendChannels, EventStream};
pub use error::{BackendError, BackendErrorCategory, classify_http_status};
pub use normalization::{SendOutcome, normalize_send_outcome};
pub use retry::RetryPolicy;
pub use state_machine::BackendStateMachine;
pub use timeline::{TimelineBuffer, TimelineMergeError};
pub use types::{
    BackendCommand, BackendEvent, BackendInitConfig, BackendLifecycleState, CryptoStatus,
    MessageType, RoomSummary, SendAck, SyncStatus, TimelineItem, TimelineOp,
};
