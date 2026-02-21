//! Persisted login metadata used by desktop login/session restore flow.

use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

/// Non-secret login metadata remembered between app launches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthProfile {
    /// Last homeserver used by the desktop login flow.
    pub homeserver: String,
    /// Last Matrix user ID used by the desktop login flow.
    pub user_id: String,
    /// Whether session restore should be attempted on startup.
    pub remember_password: bool,
}

/// Load profile JSON from disk when available.
pub fn load_auth_profile(path: &Path) -> Result<Option<AuthProfile>, String> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "failed reading auth profile {}: {err}",
                path.display()
            ));
        }
    };

    let profile = serde_json::from_str::<AuthProfile>(&raw)
        .map_err(|err| format!("failed parsing auth profile {}: {err}", path.display()))?;
    Ok(Some(profile))
}

/// Persist profile JSON to disk, creating parent directories when needed.
pub fn save_auth_profile(path: &Path, profile: &AuthProfile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "failed creating auth profile directory {}: {err}",
                parent.display()
            )
        })?;
    }

    let encoded = serde_json::to_string(profile).map_err(|err| err.to_string())?;
    fs::write(path, encoded)
        .map_err(|err| format!("failed writing auth profile {}: {err}", path.display()))
}

/// Remove profile JSON from disk.
pub fn clear_auth_profile(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!(
            "failed deleting auth profile {}: {err}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, path::PathBuf};

    fn unique_temp_path(label: &str) -> PathBuf {
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        env::temp_dir().join(format!("pikachat-{label}-{now_nanos}.json"))
    }

    #[test]
    fn profile_round_trip() {
        let path = unique_temp_path("auth-profile");
        let profile = AuthProfile {
            homeserver: "https://matrix.example.org".to_owned(),
            user_id: "@alice:example.org".to_owned(),
            remember_password: true,
        };

        save_auth_profile(&path, &profile).expect("save should work");
        let loaded = load_auth_profile(&path)
            .expect("load should work")
            .expect("profile should be present");
        assert_eq!(loaded, profile);

        clear_auth_profile(&path).expect("clear should work");
        let after_clear = load_auth_profile(&path).expect("load after clear should work");
        assert_eq!(after_clear, None);
    }
}
