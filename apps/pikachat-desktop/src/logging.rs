//! Tracing/logging bootstrap for the desktop app.

use std::env;

use tracing_subscriber::EnvFilter;

const DEFAULT_FILTER: &str = "info,pikachat_desktop=debug,backend_matrix=debug";

/// Initialize global tracing subscriber with severity gating from environment.
///
/// Precedence:
/// 1) `RUST_LOG`
/// 2) `PIKACHAT_DESKTOP_LOG`
/// 3) `PIKACHAT_LOG`
/// 4) internal default filter
pub fn init() {
    let env_filter = filter_from_env();
    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_thread_ids(true)
        .with_thread_names(true)
        .with_env_filter(env_filter)
        .try_init();
}

fn filter_from_env() -> EnvFilter {
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return filter;
    }

    if let Some(value) = env::var("PIKACHAT_DESKTOP_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        && let Ok(filter) = EnvFilter::try_new(value)
    {
        return filter;
    }

    if let Some(value) = env::var("PIKACHAT_LOG")
        .ok()
        .filter(|v| !v.trim().is_empty())
        && let Ok(filter) = EnvFilter::try_new(value)
    {
        return filter;
    }

    EnvFilter::new(DEFAULT_FILTER)
}
