use crate::{
    error::{BackendError, BackendErrorCategory},
    types::{BackendEvent, SendAck},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    Success { event_id: String },
    Failure { error: BackendError },
}

pub fn normalize_send_outcome(
    client_txn_id: impl Into<String>,
    outcome: SendOutcome,
) -> BackendEvent {
    let client_txn_id = client_txn_id.into();
    match outcome {
        SendOutcome::Success { event_id } => BackendEvent::SendAck(SendAck {
            client_txn_id,
            event_id: Some(event_id),
            error_code: None,
        }),
        SendOutcome::Failure { error } => BackendEvent::SendAck(SendAck {
            client_txn_id,
            event_id: None,
            error_code: Some(error.code),
        }),
    }
}

pub fn normalize_fatal_error(error: BackendError, recoverable: bool) -> BackendEvent {
    BackendEvent::FatalError {
        code: error.code,
        message: error.message,
        recoverable,
    }
}

pub fn classify_send_error_message(message: impl Into<String>) -> BackendError {
    BackendError::new(BackendErrorCategory::Network, "send_failed", message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_success_to_send_ack() {
        let event = normalize_send_outcome(
            "txn-1",
            SendOutcome::Success {
                event_id: "$abc".into(),
            },
        );

        match event {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-1");
                assert_eq!(ack.event_id.as_deref(), Some("$abc"));
                assert_eq!(ack.error_code, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn maps_failure_to_send_ack_with_stable_error_code() {
        let event = normalize_send_outcome(
            "txn-2",
            SendOutcome::Failure {
                error: BackendError::new(
                    BackendErrorCategory::RateLimited,
                    "rate_limited",
                    "slow down",
                ),
            },
        );

        match event {
            BackendEvent::SendAck(ack) => {
                assert_eq!(ack.client_txn_id, "txn-2");
                assert_eq!(ack.event_id, None);
                assert_eq!(ack.error_code.as_deref(), Some("rate_limited"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
