//! Persisted login metadata used by desktop login/session restore flow.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

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

    let encoded = serde_json::to_vec(profile).map_err(|err| err.to_string())?;
    let temp_path = auth_profile_temp_path(path);
    fs::write(&temp_path, encoded)
        .map_err(|err| format!("failed writing temp auth profile {}: {err}", temp_path.display()))?;

    if let Err(rename_err) = fs::rename(&temp_path, path) {
        // Windows does not allow replacing existing files via rename.
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                let _ = fs::remove_file(&temp_path);
                return Err(format!(
                    "failed replacing auth profile {} after rename error ({rename_err}): {err}",
                    path.display()
                ));
            }
        }
        fs::rename(&temp_path, path).map_err(|err| {
            let _ = fs::remove_file(&temp_path);
            format!(
                "failed writing auth profile {} after temp write: {err}",
                path.display()
            )
        })?;
    }

    Ok(())
}

fn auth_profile_temp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("auth-profile.json");
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{file_name}.{now_nanos}.tmp"))
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
