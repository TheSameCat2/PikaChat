//! Disk-backed media cache with JSON metadata index and LRU eviction.

use std::{
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const MEDIA_CACHE_DIR: &str = "media-cache";
const INDEX_FILE: &str = "index.json";
const DEFAULT_CAPACITY_BYTES: u64 = 1_024 * 1_024 * 1_024;

#[derive(Debug, Clone)]
pub struct MediaCache {
    root: PathBuf,
    index_path: PathBuf,
    capacity_bytes: u64,
    index: CacheIndex,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CacheIndex {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    path: String,
    size: u64,
    last_access_ms: u64,
}

impl MediaCache {
    pub fn new(data_dir: &Path) -> io::Result<Self> {
        Self::with_capacity(data_dir, DEFAULT_CAPACITY_BYTES)
    }

    pub fn with_capacity(data_dir: &Path, capacity_bytes: u64) -> io::Result<Self> {
        let root = data_dir.join(MEDIA_CACHE_DIR);
        fs::create_dir_all(&root)?;
        let index_path = root.join(INDEX_FILE);
        let index = load_index(&index_path).unwrap_or_default();

        let mut cache = Self {
            root,
            index_path,
            capacity_bytes: capacity_bytes.max(1),
            index,
        };
        cache.prune_missing_files();
        cache.evict_if_needed(None)?;
        cache.persist_index()?;
        Ok(cache)
    }

    pub fn get(&mut self, key: &str) -> Option<PathBuf> {
        let now = now_millis();
        let mut remove_stale_entry = false;
        let path = {
            let entry = self.index.entries.get_mut(key)?;
            let path = self.root.join(&entry.path);
            if !path.exists() {
                None
            } else if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
            {
                let _ = fs::remove_file(&path);
                remove_stale_entry = true;
                None
            } else {
                entry.last_access_ms = now;
                Some(path)
            }
        };

        if remove_stale_entry {
            self.index.entries.remove(key);
            let _ = self.persist_index();
            return None;
        }
        let path = path?;
        let _ = self.persist_index();
        Some(path)
    }

    pub fn insert(
        &mut self,
        key: &str,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> io::Result<PathBuf> {
        let ext = extension_from_content_type(content_type)
            .or_else(|| extension_from_bytes(bytes))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsupported image format; expected png/jpeg/gif/webp bytes",
                )
            })?;
        let stem = format!("{:016x}", stable_hash(key));
        let file_name = format!("{stem}.{ext}");
        let relative_path = PathBuf::from(file_name);
        let absolute_path = self.root.join(&relative_path);

        // Remove stale siblings for the same hash with different extensions.
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let path = entry.path();
            if path == absolute_path || !path.is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name == INDEX_FILE || !name.starts_with(&stem) || !name[stem.len()..].starts_with('.') {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
        }

        fs::write(&absolute_path, bytes)?;

        self.index.entries.insert(
            key.to_owned(),
            CacheEntry {
                key: key.to_owned(),
                path: relative_path.to_string_lossy().to_string(),
                size: bytes.len() as u64,
                last_access_ms: now_millis(),
            },
        );

        self.evict_if_needed(Some(key))?;
        self.persist_index()?;
        Ok(absolute_path)
    }

    fn evict_if_needed(&mut self, protected_key: Option<&str>) -> io::Result<()> {
        while self.total_size_bytes() > self.capacity_bytes {
            let Some(evict_key) = self
                .index
                .entries
                .values()
                .filter(|entry| Some(entry.key.as_str()) != protected_key)
                .min_by_key(|entry| entry.last_access_ms)
                .map(|entry| entry.key.clone())
            else {
                break;
            };

            if let Some(entry) = self.index.entries.remove(&evict_key) {
                let path = self.root.join(entry.path);
                match fs::remove_file(path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(err),
                }
            }
        }
        Ok(())
    }

    fn prune_missing_files(&mut self) {
        self.index
            .entries
            .retain(|_, entry| self.root.join(&entry.path).exists());
    }

    fn total_size_bytes(&self) -> u64 {
        self.index.entries.values().map(|entry| entry.size).sum()
    }

    fn persist_index(&self) -> io::Result<()> {
        let encoded = serde_json::to_vec_pretty(&self.index)
            .map_err(|err| io::Error::other(err.to_string()))?;
        fs::write(&self.index_path, encoded)
    }
}

fn load_index(path: &Path) -> Option<CacheIndex> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice::<CacheIndex>(&bytes).ok()
}

fn extension_from_content_type(content_type: Option<&str>) -> Option<&'static str> {
    match content_type?.to_ascii_lowercase().as_str() {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        _ => None,
    }
}

fn extension_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes[0..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("png");
    }
    if bytes.len() >= 3 && bytes[0..3] == [0xFF, 0xD8, 0xFF] {
        return Some("jpg");
    }
    if bytes.len() >= 6 && (&bytes[0..6] == b"GIF87a" || &bytes[0..6] == b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    None
}

fn stable_hash(key: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    hasher.finish()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
