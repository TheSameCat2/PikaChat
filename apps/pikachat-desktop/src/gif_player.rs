//! GIF decode and playback controller for desktop media rows.

use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Cursor,
    path::Path,
    sync::Arc,
};

use image::codecs::gif::GifDecoder;
use image::{AnimationDecoder, ImageDecoder};

/// Maximum number of decoded frames per GIF.
pub const MAX_GIF_FRAMES: usize = 300;
/// Maximum decoded RGBA bytes budget per GIF.
pub const MAX_DECODED_BYTES_PER_GIF: u64 = 128 * 1024 * 1024;
/// Maximum allowed frame dimension in pixels.
pub const MAX_FRAME_DIMENSION: u32 = 4096;
/// Minimum frame delay used for playback scheduling.
pub const MIN_FRAME_DELAY_MS: u64 = 33;
/// Maximum rows animated concurrently.
pub const MAX_CONCURRENT_ANIMATED_ROWS: usize = 3;
/// UI visibility ping cadence.
pub const VISIBLE_PING_INTERVAL_MS: u64 = 250;
/// Row visibility timeout after the last ping.
pub const VISIBLE_STALE_TIMEOUT_MS: u64 = 750;

/// UI-facing hint for guardrail fallback.
pub const GUARDRAIL_FALLBACK_REASON: &str = "GIF shown as static (too large to animate)";

const DECODE_FALLBACK_REASON: &str = "GIF shown as static (failed to decode animation)";
const DEFAULT_DECODE_CACHE_CAPACITY: usize = 64;

/// Decoded media result for GIF-capable sources.
#[derive(Clone)]
pub enum GifDecodeResult {
    /// Fully decoded animated GIF data.
    Animated {
        frames: Vec<slint::Image>,
        delays_ms: Vec<u64>,
        /// GIF loop metadata. `None` means unknown or infinite.
        loop_count: Option<u32>,
    },
    /// Static fallback when animation is blocked by guardrails/decode issues.
    StaticFallback {
        first_frame: slint::Image,
        reason: String,
    },
    /// Input is not a GIF.
    NotGif,
}

/// Per-tick frame update emitted by [`GifPlaybackController::tick`].
#[derive(Clone)]
pub struct GifFrameUpdate {
    pub media_key: String,
    pub frame: slint::Image,
}

/// Decode GIF bytes using the `image` crate animated GIF APIs.
pub fn decode_gif_bytes(bytes: &[u8]) -> GifDecodeResult {
    if !looks_like_gif(bytes) {
        return GifDecodeResult::NotGif;
    }

    let decoder = match GifDecoder::new(Cursor::new(bytes)) {
        Ok(decoder) => decoder,
        Err(_) => return GifDecodeResult::NotGif,
    };

    let (logical_width, logical_height) = decoder.dimensions();
    let logical_dimension_guardrail =
        logical_width > MAX_FRAME_DIMENSION || logical_height > MAX_FRAME_DIMENSION;

    let mut frame_count = 0usize;
    let mut decoded_bytes = 0u64;
    let mut first_frame: Option<slint::Image> = None;
    let mut frames = Vec::new();
    let mut delays_ms = Vec::new();
    let mut frame_iter = decoder.into_frames();

    while let Some(frame_result) = frame_iter.next() {
        let frame = match frame_result {
            Ok(frame) => frame,
            Err(_) => return fallback_or_not_gif(first_frame, DECODE_FALLBACK_REASON),
        };

        let frame_delay_ms = normalize_delay_ms(frame.delay());
        let rgba = frame.into_buffer();
        let frame_image = rgba_to_slint_image(&rgba);
        if first_frame.is_none() {
            first_frame = Some(frame_image.clone());
        }

        frame_count = frame_count.saturating_add(1);
        decoded_bytes = decoded_bytes
            .saturating_add(decoded_frame_bytes(rgba.width(), rgba.height()).unwrap_or(u64::MAX));

        let frame_dimension_guardrail =
            rgba.width() > MAX_FRAME_DIMENSION || rgba.height() > MAX_FRAME_DIMENSION;
        if logical_dimension_guardrail
            || frame_dimension_guardrail
            || frame_count > MAX_GIF_FRAMES
            || decoded_bytes > MAX_DECODED_BYTES_PER_GIF
        {
            return fallback_or_not_gif(first_frame, GUARDRAIL_FALLBACK_REASON);
        }

        frames.push(frame_image);
        delays_ms.push(frame_delay_ms);
    }

    if frames.is_empty() {
        return GifDecodeResult::NotGif;
    }

    GifDecodeResult::Animated {
        frames,
        delays_ms,
        // The image crate decoder does not currently expose GIF loop extension metadata.
        loop_count: None,
    }
}

/// Main-thread playback controller for timeline/fullscreen GIF rows.
pub struct GifPlaybackController {
    decode_cache: DecodedGifCache,
    rows: HashMap<String, RowPlaybackState>,
}

impl Default for GifPlaybackController {
    fn default() -> Self {
        Self::new()
    }
}

impl GifPlaybackController {
    /// Create a controller with a default decoded GIF cache size.
    pub fn new() -> Self {
        Self::with_cache_capacity(DEFAULT_DECODE_CACHE_CAPACITY)
    }

    /// Create a controller with an explicit decoded GIF cache size.
    pub fn with_cache_capacity(cache_capacity: usize) -> Self {
        Self {
            decode_cache: DecodedGifCache::new(cache_capacity.max(1)),
            rows: HashMap::new(),
        }
    }

    /// Record a visibility ping for a row/media key.
    pub fn register_visibility_ping(&mut self, media_key: impl Into<String>, now_ms: u64) {
        let row = self.rows.entry(media_key.into()).or_default();
        row.last_visibility_ping_ms = Some(now_ms);
    }

    /// Attach a decoded source path to a row/media key.
    pub fn attach_source(&mut self, media_key: impl Into<String>, file_path: impl AsRef<Path>) {
        let media_key = media_key.into();
        let source_path = file_path.as_ref();
        let source_cache_key = source_path.to_string_lossy().into_owned();
        let _decoded = self
            .decode_cache
            .get_or_load(source_cache_key.clone(), || decode_gif_file(source_path));

        let row = self.rows.entry(media_key).or_default();
        row.source_cache_key = Some(source_cache_key);
        row.current_frame_index = 0;
        row.completed_loops = 0;
        row.next_frame_due_ms = None;
        row.is_active = false;
    }

    /// Advance animation state and emit changed row frames.
    pub fn tick(&mut self, now_ms: u64) -> Vec<GifFrameUpdate> {
        let mut candidates: Vec<(String, u64)> = self
            .rows
            .iter()
            .filter_map(|(media_key, row)| {
                let last_ping = row.last_visibility_ping_ms?;
                if now_ms.saturating_sub(last_ping) > VISIBLE_STALE_TIMEOUT_MS {
                    return None;
                }

                let source_key = row.source_cache_key.as_deref()?;
                let decoded = self.decode_cache.peek(source_key)?;
                match decoded.as_ref() {
                    GifDecodeResult::Animated { frames, .. } if frames.len() > 1 => {
                        Some((media_key.clone(), last_ping))
                    }
                    _ => None,
                }
            })
            .collect();

        candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        candidates.truncate(MAX_CONCURRENT_ANIMATED_ROWS);
        let active_order: Vec<String> = candidates.into_iter().map(|(key, _)| key).collect();
        let active_set: HashSet<String> = active_order.iter().cloned().collect();

        for (media_key, row) in &mut self.rows {
            if !active_set.contains(media_key) {
                row.is_active = false;
                row.next_frame_due_ms = None;
            }
        }

        let mut updates = Vec::new();
        for media_key in active_order {
            let Some(source_cache_key) = self
                .rows
                .get(&media_key)
                .and_then(|row| row.source_cache_key.clone())
            else {
                continue;
            };
            let Some(decoded) = self.decode_cache.get(&source_cache_key) else {
                continue;
            };
            let GifDecodeResult::Animated {
                frames,
                delays_ms,
                loop_count,
            } = decoded.as_ref()
            else {
                continue;
            };
            if frames.len() <= 1 {
                continue;
            }

            let Some(row) = self.rows.get_mut(&media_key) else {
                continue;
            };
            if row.current_frame_index >= frames.len() {
                row.current_frame_index = 0;
                row.completed_loops = 0;
            }

            if !row.is_active {
                row.is_active = true;
                row.next_frame_due_ms = Some(
                    now_ms.saturating_add(delay_for_index(delays_ms, row.current_frame_index)),
                );
            }

            let mut next_due = row.next_frame_due_ms.unwrap_or_else(|| {
                now_ms.saturating_add(delay_for_index(delays_ms, row.current_frame_index))
            });
            let mut changed = false;

            while now_ms >= next_due {
                if !advance_row_frame(row, frames.len(), *loop_count) {
                    row.next_frame_due_ms = None;
                    break;
                }
                changed = true;
                next_due =
                    next_due.saturating_add(delay_for_index(delays_ms, row.current_frame_index));
                row.next_frame_due_ms = Some(next_due);
            }

            if row.next_frame_due_ms.is_some() {
                row.next_frame_due_ms = Some(next_due);
            }

            if changed {
                updates.push(GifFrameUpdate {
                    media_key,
                    frame: frames[row.current_frame_index].clone(),
                });
            }
        }

        updates
    }

    /// Read the current frame to render for a media key.
    pub fn current_frame_for(&self, media_key: &str) -> Option<slint::Image> {
        let row = self.rows.get(media_key)?;
        let source_cache_key = row.source_cache_key.as_deref()?;
        let decoded = self.decode_cache.peek(source_cache_key)?;

        match decoded.as_ref() {
            GifDecodeResult::Animated { frames, .. } => {
                let frame_index = row.current_frame_index.min(frames.len().saturating_sub(1));
                frames.get(frame_index).cloned()
            }
            GifDecodeResult::StaticFallback { first_frame, .. } => Some(first_frame.clone()),
            GifDecodeResult::NotGif => None,
        }
    }

    /// Read a static-fallback hint for a media key, if present.
    pub fn hint_for(&self, media_key: &str) -> Option<String> {
        let row = self.rows.get(media_key)?;
        let source_cache_key = row.source_cache_key.as_deref()?;
        let decoded = self.decode_cache.peek(source_cache_key)?;
        match decoded.as_ref() {
            GifDecodeResult::StaticFallback { reason, .. } => Some(reason.clone()),
            _ => None,
        }
    }
}

fn advance_row_frame(
    row: &mut RowPlaybackState,
    frame_count: usize,
    loop_count: Option<u32>,
) -> bool {
    if frame_count <= 1 {
        return false;
    }
    if row.current_frame_index + 1 < frame_count {
        row.current_frame_index += 1;
        return true;
    }

    if let Some(max_loops) = loop_count {
        if row.completed_loops.saturating_add(1) >= max_loops {
            row.current_frame_index = frame_count - 1;
            return false;
        }
    }

    row.completed_loops = row.completed_loops.saturating_add(1);
    row.current_frame_index = 0;
    true
}

fn delay_for_index(delays_ms: &[u64], frame_index: usize) -> u64 {
    delays_ms
        .get(frame_index)
        .copied()
        .unwrap_or(MIN_FRAME_DELAY_MS)
        .max(MIN_FRAME_DELAY_MS)
}

fn decode_gif_file(path: &Path) -> GifDecodeResult {
    match fs::read(path) {
        Ok(bytes) => decode_gif_bytes(&bytes),
        Err(_) => GifDecodeResult::NotGif,
    }
}

fn decoded_frame_bytes(width: u32, height: u32) -> Option<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
}

fn looks_like_gif(bytes: &[u8]) -> bool {
    bytes.len() >= 6 && (&bytes[..6] == b"GIF87a" || &bytes[..6] == b"GIF89a")
}

fn normalize_delay_ms(delay: image::Delay) -> u64 {
    let (numerator, denominator) = delay.numer_denom_ms();
    if denominator == 0 {
        return MIN_FRAME_DELAY_MS;
    }
    let denominator = u64::from(denominator);
    let millis = u64::from(numerator).saturating_add(denominator.saturating_sub(1)) / denominator;
    millis.max(MIN_FRAME_DELAY_MS)
}

fn rgba_to_slint_image(rgba: &image::RgbaImage) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    slint::Image::from_rgba8(buffer)
}

fn fallback_or_not_gif(first_frame: Option<slint::Image>, reason: &str) -> GifDecodeResult {
    first_frame
        .map(|first_frame| GifDecodeResult::StaticFallback {
            first_frame,
            reason: reason.to_owned(),
        })
        .unwrap_or(GifDecodeResult::NotGif)
}

struct DecodedGifCache {
    capacity: usize,
    access_clock: u64,
    entries: HashMap<String, CachedDecodeEntry>,
}

struct CachedDecodeEntry {
    decoded: Arc<GifDecodeResult>,
    last_access: u64,
}

impl DecodedGifCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            access_clock: 0,
            entries: HashMap::new(),
        }
    }

    fn peek(&self, key: &str) -> Option<&Arc<GifDecodeResult>> {
        self.entries.get(key).map(|entry| &entry.decoded)
    }

    fn get(&mut self, key: &str) -> Option<Arc<GifDecodeResult>> {
        let entry = self.entries.get_mut(key)?;
        self.access_clock = self.access_clock.saturating_add(1);
        entry.last_access = self.access_clock;
        Some(Arc::clone(&entry.decoded))
    }

    fn get_or_load<F>(&mut self, key: String, load: F) -> Arc<GifDecodeResult>
    where
        F: FnOnce() -> GifDecodeResult,
    {
        if let Some(hit) = self.get(&key) {
            return hit;
        }

        let decoded = Arc::new(load());
        self.access_clock = self.access_clock.saturating_add(1);
        self.entries.insert(
            key,
            CachedDecodeEntry {
                decoded: Arc::clone(&decoded),
                last_access: self.access_clock,
            },
        );
        self.evict_if_needed();
        decoded
    }

    fn evict_if_needed(&mut self) {
        while self.entries.len() > self.capacity {
            let Some(evict_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&evict_key);
        }
    }
}

#[derive(Default)]
struct RowPlaybackState {
    source_cache_key: Option<String>,
    last_visibility_ping_ms: Option<u64>,
    current_frame_index: usize,
    completed_loops: u32,
    next_frame_due_ms: Option<u64>,
    is_active: bool,
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame as ImageFrame, Rgba, RgbaImage};

    use super::*;

    #[test]
    fn decode_guardrails_and_fallback() {
        let many_frames = vec![10; MAX_GIF_FRAMES + 1];
        let bytes = build_test_gif(2, 2, &many_frames);
        match decode_gif_bytes(&bytes) {
            GifDecodeResult::StaticFallback {
                first_frame,
                reason,
            } => {
                assert_eq!(reason, GUARDRAIL_FALLBACK_REASON);
                let rgba = first_frame
                    .to_rgba8()
                    .expect("first frame should be readable");
                assert_eq!(rgba.width(), 2);
                assert_eq!(rgba.height(), 2);
            }
            _ => panic!("expected static fallback when frame count exceeds guardrail"),
        }

        let oversized = build_test_gif(MAX_FRAME_DIMENSION + 1, 1, &[10]);
        assert!(matches!(
            decode_gif_bytes(&oversized),
            GifDecodeResult::StaticFallback { reason, .. } if reason == GUARDRAIL_FALLBACK_REASON
        ));

        assert!(matches!(
            decode_gif_bytes(b"not-a-gif"),
            GifDecodeResult::NotGif
        ));
    }

    #[test]
    fn delay_normalization_applies_minimum_delay() {
        let bytes = build_test_gif(1, 1, &[0, 10, 50]);
        match decode_gif_bytes(&bytes) {
            GifDecodeResult::Animated { delays_ms, .. } => {
                assert_eq!(delays_ms, vec![MIN_FRAME_DELAY_MS, MIN_FRAME_DELAY_MS, 50]);
            }
            _ => panic!("expected animated decode result"),
        }
    }

    #[test]
    fn visibility_timeout_pauses_rows() {
        let gif_file = TempGifFile::new(&build_test_gif(1, 1, &[40, 40, 40]));
        let mut controller = GifPlaybackController::with_cache_capacity(8);
        controller.attach_source("row-1", gif_file.path());

        assert_eq!(
            first_pixel_red(
                &controller
                    .current_frame_for("row-1")
                    .expect("initial frame")
            ),
            0
        );

        controller.register_visibility_ping("row-1", 0);
        assert!(controller.tick(0).is_empty());
        let updates = controller.tick(40);
        assert_eq!(updates.len(), 1);
        assert_eq!(
            first_pixel_red(
                &controller
                    .current_frame_for("row-1")
                    .expect("advanced frame should exist")
            ),
            80
        );

        let stale_tick = VISIBLE_STALE_TIMEOUT_MS + 100;
        assert!(controller.tick(stale_tick).is_empty());
        assert_eq!(
            first_pixel_red(
                &controller
                    .current_frame_for("row-1")
                    .expect("stale row should keep last frame")
            ),
            80
        );
        assert!(controller.tick(stale_tick + 1000).is_empty());
        assert_eq!(
            first_pixel_red(
                &controller
                    .current_frame_for("row-1")
                    .expect("stale row should still be paused")
            ),
            80
        );
    }

    #[test]
    fn active_row_cap_limits_concurrent_animation() {
        let gif_file = TempGifFile::new(&build_test_gif(1, 1, &[40, 40]));
        let mut controller = GifPlaybackController::with_cache_capacity(8);

        for key in ["row-a", "row-b", "row-c", "row-d"] {
            controller.attach_source(key, gif_file.path());
            controller.register_visibility_ping(key, 0);
        }

        assert!(controller.tick(0).is_empty());
        let updates = controller.tick(40);
        assert_eq!(updates.len(), MAX_CONCURRENT_ANIMATED_ROWS);
        let mut updated_keys: Vec<&str> = updates
            .iter()
            .map(|update| update.media_key.as_str())
            .collect();
        updated_keys.sort_unstable();
        assert_eq!(updated_keys, vec!["row-a", "row-b", "row-c"]);
        assert_eq!(
            first_pixel_red(
                &controller
                    .current_frame_for("row-d")
                    .expect("row-d should have first frame")
            ),
            0
        );

        controller.register_visibility_ping("row-d", 100);
        let _ = controller.tick(100);
        let updates = controller.tick(140);
        let updated_keys: HashSet<&str> = updates
            .iter()
            .map(|update| update.media_key.as_str())
            .collect();
        assert!(updated_keys.contains("row-d"));
        assert!(!updated_keys.contains("row-c"));
    }

    fn build_test_gif(width: u32, height: u32, delays_ms: &[u32]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = GifEncoder::new(&mut out);
            encoder
                .set_repeat(Repeat::Infinite)
                .expect("repeat metadata should be writable");

            let frames = delays_ms.iter().enumerate().map(|(idx, delay_ms)| {
                let mut rgba = RgbaImage::new(width, height);
                let red = ((idx as u16 * 80) % 256) as u8;
                for pixel in rgba.pixels_mut() {
                    *pixel = Rgba([red, 0, 0, 255]);
                }
                ImageFrame::from_parts(rgba, 0, 0, Delay::from_numer_denom_ms(*delay_ms, 1))
            });
            encoder
                .encode_frames(frames)
                .expect("test gif encoding should succeed");
        }
        out
    }

    fn first_pixel_red(image: &slint::Image) -> u8 {
        let rgba = image
            .to_rgba8()
            .expect("test image should be convertible to rgba");
        rgba.as_slice()
            .first()
            .expect("test image should contain at least one pixel")
            .r
    }

    struct TempGifFile {
        path: PathBuf,
    }

    impl TempGifFile {
        fn new(bytes: &[u8]) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pikachat-gif-player-test-{}-{id}.gif",
                std::process::id()
            ));
            fs::write(&path, bytes).expect("should write temp gif");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempGifFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
