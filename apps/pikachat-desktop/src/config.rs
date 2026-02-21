//! Environment-backed runtime configuration for `pikachat-desktop`.

use std::{env, error::Error, fmt, path::PathBuf};

use backend_core::BackendInitConfig;

const DEFAULT_DATA_DIR_ROOT: &str = "./.pikachat-desktop-store";
const DEFAULT_TIMELINE_MAX_ITEMS: usize = 1_200;
const DEFAULT_PAGINATE_LIMIT: u16 = 30;
const DEFAULT_PAGINATION_TOP_THRESHOLD_PX: f32 = 80.0;
const DEFAULT_PAGINATION_COOLDOWN_MS: u64 = 750;

/// Runtime configuration used by the desktop app.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopConfig {
    /// Matrix homeserver URL, for example `https://matrix.example.org`.
    pub homeserver: String,
    /// Matrix user ID used for password login and own-message detection.
    pub user_id: String,
    /// Password used for startup auth flow.
    pub password: String,
    /// Local on-disk store directory.
    pub data_dir: PathBuf,
    /// Optional backend runtime tuning forwarded to `BackendCommand::Init`.
    pub init_config: Option<BackendInitConfig>,
    /// Timeline cap used by frontend in-memory message retention.
    pub timeline_max_items: usize,
    /// Pagination request size sent with `BackendCommand::PaginateBack`.
    pub paginate_limit: u16,
    /// Viewport threshold (near top) for auto-pagination trigger.
    pub pagination_top_threshold_px: f32,
    /// Cooldown used to suppress repeated pagination requests.
    pub pagination_cooldown_ms: u64,
}

impl DesktopConfig {
    /// Parse configuration from environment variables.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    fn from_lookup<F>(mut lookup: F) -> Result<Self, ConfigError>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let homeserver = required_env("PIKACHAT_HOMESERVER", &mut lookup)?;
        let user_id = required_env("PIKACHAT_USER", &mut lookup)?;
        let password = required_env("PIKACHAT_PASSWORD", &mut lookup)?;

        let data_dir = lookup("PIKACHAT_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_data_dir(&homeserver, &user_id));

        let sync_request_timeout_ms =
            parse_optional_u64("PIKACHAT_SYNC_REQUEST_TIMEOUT_MS", &mut lookup)?;
        let default_open_room_limit = parse_optional_u16("PIKACHAT_OPEN_ROOM_LIMIT", &mut lookup)?;
        let pagination_limit_cap =
            parse_optional_u16("PIKACHAT_PAGINATION_LIMIT_CAP", &mut lookup)?;

        let init_config = if sync_request_timeout_ms.is_none()
            && default_open_room_limit.is_none()
            && pagination_limit_cap.is_none()
        {
            None
        } else {
            Some(BackendInitConfig {
                sync_request_timeout_ms,
                default_open_room_limit,
                pagination_limit_cap,
            })
        };

        let timeline_max_items = parse_optional_usize(
            "PIKACHAT_DESKTOP_TIMELINE_MAX_ITEMS",
            DEFAULT_TIMELINE_MAX_ITEMS,
            &mut lookup,
        )?;
        let paginate_limit = parse_optional_u16_with_default(
            "PIKACHAT_DESKTOP_PAGINATE_LIMIT",
            DEFAULT_PAGINATE_LIMIT,
            &mut lookup,
        )?;
        let pagination_top_threshold_px = parse_optional_f32(
            "PIKACHAT_DESKTOP_PAGINATION_TOP_THRESHOLD_PX",
            DEFAULT_PAGINATION_TOP_THRESHOLD_PX,
            &mut lookup,
        )?;
        let pagination_cooldown_ms = parse_optional_u64_with_default(
            "PIKACHAT_DESKTOP_PAGINATION_COOLDOWN_MS",
            DEFAULT_PAGINATION_COOLDOWN_MS,
            &mut lookup,
        )?;

        if paginate_limit == 0 {
            return Err(ConfigError::InvalidValue {
                key: "PIKACHAT_DESKTOP_PAGINATE_LIMIT",
                value: "0".to_owned(),
                reason: "must be at least 1".to_owned(),
            });
        }
        if timeline_max_items == 0 {
            return Err(ConfigError::InvalidValue {
                key: "PIKACHAT_DESKTOP_TIMELINE_MAX_ITEMS",
                value: "0".to_owned(),
                reason: "must be at least 1".to_owned(),
            });
        }
        if pagination_top_threshold_px <= 0.0 {
            return Err(ConfigError::InvalidValue {
                key: "PIKACHAT_DESKTOP_PAGINATION_TOP_THRESHOLD_PX",
                value: pagination_top_threshold_px.to_string(),
                reason: "must be greater than 0".to_owned(),
            });
        }

        Ok(Self {
            homeserver,
            user_id,
            password,
            data_dir,
            init_config,
            timeline_max_items,
            paginate_limit,
            pagination_top_threshold_px,
            pagination_cooldown_ms,
        })
    }
}

/// Errors produced while parsing runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A required environment variable was missing or empty.
    MissingRequired { key: &'static str },
    /// An environment variable could not be parsed.
    InvalidValue {
        key: &'static str,
        value: String,
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequired { key } => {
                write!(f, "missing required environment variable: {key}")
            }
            Self::InvalidValue { key, value, reason } => {
                write!(f, "invalid {key}='{value}': {reason}")
            }
        }
    }
}

impl Error for ConfigError {}

fn required_env<F>(key: &'static str, lookup: &mut F) -> Result<String, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    let value = lookup(key).unwrap_or_default();
    if value.trim().is_empty() {
        return Err(ConfigError::MissingRequired { key });
    }
    Ok(value)
}

fn parse_optional_u16<F>(key: &'static str, lookup: &mut F) -> Result<Option<u16>, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(value) = lookup(key) else {
        return Ok(None);
    };
    value
        .parse::<u16>()
        .map(Some)
        .map_err(|err| ConfigError::InvalidValue {
            key,
            value,
            reason: err.to_string(),
        })
}

fn parse_optional_u64<F>(key: &'static str, lookup: &mut F) -> Result<Option<u64>, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(value) = lookup(key) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|err| ConfigError::InvalidValue {
            key,
            value,
            reason: err.to_string(),
        })
}

fn parse_optional_usize<F>(
    key: &'static str,
    default: usize,
    lookup: &mut F,
) -> Result<usize, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(value) = lookup(key) else {
        return Ok(default);
    };
    value
        .parse::<usize>()
        .map_err(|err| ConfigError::InvalidValue {
            key,
            value,
            reason: err.to_string(),
        })
}

fn parse_optional_f32<F>(
    key: &'static str,
    default: f32,
    lookup: &mut F,
) -> Result<f32, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    let Some(value) = lookup(key) else {
        return Ok(default);
    };
    value
        .parse::<f32>()
        .map_err(|err| ConfigError::InvalidValue {
            key,
            value,
            reason: err.to_string(),
        })
}

fn parse_optional_u16_with_default<F>(
    key: &'static str,
    default: u16,
    lookup: &mut F,
) -> Result<u16, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    Ok(parse_optional_u16(key, lookup)?.unwrap_or(default))
}

fn parse_optional_u64_with_default<F>(
    key: &'static str,
    default: u64,
    lookup: &mut F,
) -> Result<u64, ConfigError>
where
    F: FnMut(&str) -> Option<String>,
{
    Ok(parse_optional_u64(key, lookup)?.unwrap_or(default))
}

fn default_data_dir(homeserver: &str, user_id: &str) -> PathBuf {
    let homeserver_slug = slugify_component(homeserver, 64);
    let user_slug = slugify_component(user_id, 64);
    PathBuf::from(DEFAULT_DATA_DIR_ROOT)
        .join(format!("hs-{homeserver_slug}"))
        .join(format!("user-{user_slug}"))
}

fn slugify_component(input: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(input.len().min(max_len));
    let mut last_was_sep = false;
    for ch in input.chars() {
        let next = if ch.is_ascii_alphanumeric() {
            Some(ch.to_ascii_lowercase())
        } else {
            Some('_')
        };

        let Some(next) = next else {
            continue;
        };

        if next == '_' {
            if last_was_sep {
                continue;
            }
            last_was_sep = true;
        } else {
            last_was_sep = false;
        }

        out.push(next);
        if out.len() >= max_len {
            break;
        }
    }

    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "default".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, path::Path};

    fn config_from_pairs(pairs: &[(&str, &str)]) -> Result<DesktopConfig, ConfigError> {
        let map = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect::<HashMap<_, _>>();
        DesktopConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn parses_required_fields_and_defaults() {
        let cfg = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
        ])
        .expect("config should parse");

        assert_eq!(cfg.homeserver, "https://matrix.example.org");
        assert_eq!(cfg.user_id, "@alice:example.org");
        assert_eq!(cfg.password, "secret");
        assert_eq!(
            cfg.data_dir,
            Path::new(
                "./.pikachat-desktop-store/hs-https_matrix_example_org/user-alice_example_org"
            )
        );
        assert_eq!(cfg.timeline_max_items, DEFAULT_TIMELINE_MAX_ITEMS);
        assert_eq!(cfg.paginate_limit, DEFAULT_PAGINATE_LIMIT);
        assert_eq!(
            cfg.pagination_top_threshold_px,
            DEFAULT_PAGINATION_TOP_THRESHOLD_PX
        );
        assert_eq!(cfg.pagination_cooldown_ms, DEFAULT_PAGINATION_COOLDOWN_MS);
        assert!(cfg.init_config.is_none());
    }

    #[test]
    fn rejects_missing_required_values() {
        let err = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
        ])
        .expect_err("missing password should fail");
        assert_eq!(
            err,
            ConfigError::MissingRequired {
                key: "PIKACHAT_PASSWORD"
            }
        );
    }

    #[test]
    fn parses_backend_init_tuning_when_present() {
        let cfg = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
            ("PIKACHAT_SYNC_REQUEST_TIMEOUT_MS", "20000"),
            ("PIKACHAT_OPEN_ROOM_LIMIT", "50"),
            ("PIKACHAT_PAGINATION_LIMIT_CAP", "80"),
            ("PIKACHAT_DATA_DIR", "/tmp/pika"),
        ])
        .expect("config should parse");

        assert_eq!(cfg.data_dir, Path::new("/tmp/pika"));
        let init = cfg.init_config.expect("init config should be present");
        assert_eq!(init.sync_request_timeout_ms, Some(20_000));
        assert_eq!(init.default_open_room_limit, Some(50));
        assert_eq!(init.pagination_limit_cap, Some(80));
    }

    #[test]
    fn defaults_data_dir_per_homeserver_and_user() {
        let cfg_a = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
        ])
        .expect("config should parse");
        let cfg_b = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@bob:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
        ])
        .expect("config should parse");

        assert_ne!(cfg_a.data_dir, cfg_b.data_dir);
        assert_eq!(
            cfg_a.data_dir,
            Path::new(
                "./.pikachat-desktop-store/hs-https_matrix_example_org/user-alice_example_org"
            )
        );
        assert_eq!(
            cfg_b.data_dir,
            Path::new("./.pikachat-desktop-store/hs-https_matrix_example_org/user-bob_example_org")
        );
    }

    #[test]
    fn rejects_invalid_numeric_values() {
        let err = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
            ("PIKACHAT_DESKTOP_PAGINATE_LIMIT", "abc"),
        ])
        .expect_err("invalid paginate value should fail");

        assert!(matches!(
            err,
            ConfigError::InvalidValue {
                key: "PIKACHAT_DESKTOP_PAGINATE_LIMIT",
                ..
            }
        ));
    }
}
