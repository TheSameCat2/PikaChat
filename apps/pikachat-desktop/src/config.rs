//! Environment-backed runtime configuration for `pikachat-desktop`.

use std::{
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
};

use backend_core::BackendInitConfig;

const DEFAULT_DATA_DIR_ROOT: &str = "./.pikachat-desktop-store";
const LOGIN_PROFILE_FILENAME: &str = ".pikachat-login-profile.json";
const DEFAULT_TIMELINE_MAX_ITEMS: usize = 1_200;
const DEFAULT_PAGINATE_LIMIT: u16 = 30;
const DEFAULT_PAGINATION_TOP_THRESHOLD_PX: f32 = 80.0;
const DEFAULT_PAGINATION_COOLDOWN_MS: u64 = 750;

/// Runtime configuration used by the desktop app.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopConfig {
    /// Optional homeserver prefill value for the login form.
    pub prefill_homeserver: Option<String>,
    /// Optional user-ID prefill value for the login form.
    pub prefill_user_id: Option<String>,
    /// Optional password prefill value for the login form.
    pub prefill_password: Option<String>,
    /// Optional fixed data dir override. When set, every login uses this directory.
    pub data_dir_override: Option<PathBuf>,
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
        let prefill_homeserver = optional_trimmed_env("PIKACHAT_HOMESERVER", &mut lookup);
        let prefill_user_id = optional_trimmed_env("PIKACHAT_USER", &mut lookup);
        let prefill_password = optional_trimmed_env("PIKACHAT_PASSWORD", &mut lookup);

        let data_dir_override =
            optional_trimmed_env("PIKACHAT_DATA_DIR", &mut lookup).map(PathBuf::from);

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
            prefill_homeserver,
            prefill_user_id,
            prefill_password,
            data_dir_override,
            init_config,
            timeline_max_items,
            paginate_limit,
            pagination_top_threshold_px,
            pagination_cooldown_ms,
        })
    }

    /// Resolve the Matrix SDK store data dir for a login attempt.
    pub fn data_dir_for_account(&self, homeserver: &str, user_id: &str) -> PathBuf {
        self.data_dir_override
            .clone()
            .unwrap_or_else(|| default_data_dir(homeserver, user_id))
    }

    /// Location of the desktop login profile metadata file.
    pub fn auth_profile_path(&self) -> PathBuf {
        match &self.data_dir_override {
            Some(data_dir) => data_dir.join(LOGIN_PROFILE_FILENAME),
            None => PathBuf::from(DEFAULT_DATA_DIR_ROOT).join(LOGIN_PROFILE_FILENAME),
        }
    }

    /// Local paths that should be removed for destructive logout.
    pub fn logout_wipe_targets(&self) -> Vec<PathBuf> {
        match &self.data_dir_override {
            Some(data_dir) => vec![data_dir.clone()],
            None => vec![PathBuf::from(DEFAULT_DATA_DIR_ROOT)],
        }
    }
}

/// Errors produced while parsing runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
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
            Self::InvalidValue { key, value, reason } => {
                write!(f, "invalid {key}='{value}': {reason}")
            }
        }
    }
}

impl Error for ConfigError {}

fn optional_trimmed_env<F>(key: &'static str, lookup: &mut F) -> Option<String>
where
    F: FnMut(&str) -> Option<String>,
{
    lookup(key)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
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
    default_data_dir_under(Path::new(DEFAULT_DATA_DIR_ROOT), homeserver, user_id)
}

fn default_data_dir_under(data_dir_root: &Path, homeserver: &str, user_id: &str) -> PathBuf {
    let homeserver_slug = slugify_component(homeserver, 64);
    let user_slug = slugify_component(user_id, 64);
    data_dir_root
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
    fn parses_prefill_fields_and_defaults() {
        let cfg = config_from_pairs(&[
            ("PIKACHAT_HOMESERVER", "https://matrix.example.org"),
            ("PIKACHAT_USER", "@alice:example.org"),
            ("PIKACHAT_PASSWORD", "secret"),
        ])
        .expect("config should parse");

        assert_eq!(
            cfg.prefill_homeserver.as_deref(),
            Some("https://matrix.example.org")
        );
        assert_eq!(cfg.prefill_user_id.as_deref(), Some("@alice:example.org"));
        assert_eq!(cfg.prefill_password.as_deref(), Some("secret"));
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
    fn credentials_are_optional_for_login_ui_mode() {
        let cfg = config_from_pairs(&[]).expect("config without credentials should parse");
        assert_eq!(cfg.prefill_homeserver, None);
        assert_eq!(cfg.prefill_user_id, None);
        assert_eq!(cfg.prefill_password, None);
    }

    #[test]
    fn parses_backend_init_tuning_when_present() {
        let cfg = config_from_pairs(&[
            ("PIKACHAT_SYNC_REQUEST_TIMEOUT_MS", "20000"),
            ("PIKACHAT_OPEN_ROOM_LIMIT", "50"),
            ("PIKACHAT_PAGINATION_LIMIT_CAP", "80"),
            ("PIKACHAT_DATA_DIR", "/tmp/pika"),
        ])
        .expect("config should parse");

        assert_eq!(cfg.data_dir_override, Some(PathBuf::from("/tmp/pika")));
        let init = cfg.init_config.expect("init config should be present");
        assert_eq!(init.sync_request_timeout_ms, Some(20_000));
        assert_eq!(init.default_open_room_limit, Some(50));
        assert_eq!(init.pagination_limit_cap, Some(80));
    }

    #[test]
    fn derives_data_dir_per_homeserver_and_user_without_override() {
        let cfg = config_from_pairs(&[]).expect("config should parse");

        let a = cfg.data_dir_for_account("https://matrix.example.org", "@alice:example.org");
        let b = cfg.data_dir_for_account("https://matrix.example.org", "@bob:example.org");

        assert_ne!(a, b);
        assert_eq!(
            a,
            Path::new(
                "./.pikachat-desktop-store/hs-https_matrix_example_org/user-alice_example_org"
            )
        );
        assert_eq!(
            b,
            Path::new("./.pikachat-desktop-store/hs-https_matrix_example_org/user-bob_example_org")
        );
    }

    #[test]
    fn uses_fixed_data_dir_when_override_is_set() {
        let cfg =
            config_from_pairs(&[("PIKACHAT_DATA_DIR", "/tmp/pika")]).expect("config should parse");

        assert_eq!(
            cfg.data_dir_for_account("https://matrix.example.org", "@alice:example.org"),
            Path::new("/tmp/pika")
        );
        assert_eq!(
            cfg.data_dir_for_account("https://another.example.org", "@bob:example.org"),
            Path::new("/tmp/pika")
        );
    }

    #[test]
    fn login_profile_path_tracks_data_dir_mode() {
        let default_cfg = config_from_pairs(&[]).expect("default config should parse");
        assert_eq!(
            default_cfg.auth_profile_path(),
            Path::new("./.pikachat-desktop-store/.pikachat-login-profile.json")
        );

        let override_cfg = config_from_pairs(&[("PIKACHAT_DATA_DIR", "/tmp/pika")])
            .expect("override config should parse");
        assert_eq!(
            override_cfg.auth_profile_path(),
            Path::new("/tmp/pika/.pikachat-login-profile.json")
        );
    }

    #[test]
    fn logout_wipe_targets_follow_data_dir_mode() {
        let default_cfg = config_from_pairs(&[]).expect("default config should parse");
        assert_eq!(
            default_cfg.logout_wipe_targets(),
            vec![PathBuf::from("./.pikachat-desktop-store")]
        );

        let override_cfg = config_from_pairs(&[("PIKACHAT_DATA_DIR", "/tmp/pika")])
            .expect("override config should parse");
        assert_eq!(
            override_cfg.logout_wipe_targets(),
            vec![PathBuf::from("/tmp/pika")]
        );
    }

    #[test]
    fn rejects_invalid_numeric_values() {
        let err = config_from_pairs(&[("PIKACHAT_DESKTOP_PAGINATE_LIMIT", "abc")])
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
