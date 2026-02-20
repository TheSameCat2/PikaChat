use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::BackendLifecycleState;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum BackendErrorCategory {
    Config,
    Auth,
    Network,
    RateLimited,
    Crypto,
    Storage,
    Serialization,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Error)]
#[error("{category:?}:{code}: {message}")]
pub struct BackendError {
    pub category: BackendErrorCategory,
    pub code: String,
    pub message: String,
    pub retry_after_ms: Option<u64>,
}

impl BackendError {
    pub fn new(
        category: BackendErrorCategory,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            category,
            code: code.into(),
            message: message.into(),
            retry_after_ms: None,
        }
    }

    pub fn with_retry_after(mut self, retry_after: Duration) -> Self {
        self.retry_after_ms = Some(retry_after.as_millis() as u64);
        self
    }

    pub fn invalid_state(current: BackendLifecycleState, action: impl Into<String>) -> Self {
        let action = action.into();
        Self::new(
            BackendErrorCategory::Internal,
            "invalid_state_transition",
            format!("cannot run '{action}' while backend is in state {current:?}"),
        )
    }
}

pub fn classify_http_status(status: u16) -> BackendErrorCategory {
    match status {
        401 | 403 => BackendErrorCategory::Auth,
        408 | 429 => BackendErrorCategory::RateLimited,
        400..=499 => BackendErrorCategory::Config,
        500..=599 => BackendErrorCategory::Network,
        _ => BackendErrorCategory::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_http_status_categories() {
        assert_eq!(classify_http_status(401), BackendErrorCategory::Auth);
        assert_eq!(classify_http_status(429), BackendErrorCategory::RateLimited);
        assert_eq!(classify_http_status(404), BackendErrorCategory::Config);
        assert_eq!(classify_http_status(503), BackendErrorCategory::Network);
        assert_eq!(classify_http_status(700), BackendErrorCategory::Internal);
    }

    #[test]
    fn keeps_invalid_state_error_code_stable() {
        let err = BackendError::invalid_state(BackendLifecycleState::Cold, "start_sync");
        assert_eq!(err.code, "invalid_state_transition");
        assert_eq!(err.category, BackendErrorCategory::Internal);
    }

    #[test]
    fn persists_retry_after_in_millis() {
        let err = BackendError::new(BackendErrorCategory::RateLimited, "rate_limited", "wait")
            .with_retry_after(Duration::from_secs(3));
        assert_eq!(err.retry_after_ms, Some(3000));
    }
}
