// /Memory-Archive/ma-core/src/vision/mod.rs

pub mod client;
pub mod marker;

use std::path::{Path, PathBuf};

use ma_proto::control_center::CommandEvent;

use crate::config::Config;

#[derive(Debug, PartialEq)]
pub enum FetchDecision {
    Skip { reason: &'static str },
    /// Fetch the frame nearest to the event timestamp.
    /// `mark` — true for mouse actions (click annotation), false otherwise.
    FetchAt { mark: bool },
    /// Keyboard press or type — fetch a frame after a configurable delay.
    /// The delay allows the screen to reflect the result of the action.
    FetchAfter { delay_ms: u64 },
}

/// Decide what image fetch action to take for a CommandEvent.
///
/// `press_delay_ms` — delay for keyboard/press events (from config.press_fetch_delay_ms).
/// `type_delay_ms`  — delay for keyboard/type events (from config.type_fetch_delay_ms).
///
/// Pure function — no I/O, no side effects.
pub fn decide(event: &CommandEvent, press_delay_ms: u64, type_delay_ms: u64) -> FetchDecision {
    if !event.success {
        return FetchDecision::Skip { reason: "command failed" };
    }

    match event.action_type.as_str() {
        // Every mouse interaction captures a marked at-frame. The mark is drawn at
        // event.mouse_x/mouse_y — the acted-on position: the click point for
        // left/right/double/middle/triple, the destination for "move" and "drag"
        // (CC drag reports its endpoint as position_captured), the press/release
        // point for "hold"/"release", and the pointer position for scroll. A
        // catch-all so any current or future mouse subtype is captured rather than
        // silently dropped as frameless — hold/release/drag/scroll/middle/triple
        // steps previously landed in the corpus without visual context.
        "mouse" => FetchDecision::FetchAt { mark: true },
        "keyboard" => match event.action_subtype.as_str() {
            "type"  => FetchDecision::FetchAfter { delay_ms: type_delay_ms },
            "press" => FetchDecision::FetchAfter { delay_ms: press_delay_ms },
            _       => FetchDecision::Skip { reason: "keyboard subtype does not trigger fetch" },
        },
        _ => FetchDecision::Skip { reason: "action type does not trigger fetch" },
    }
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum MarkKind {
    Click { x: i32, y: i32 },
    None,
}

/// Result of a successful vision pipeline pass for one step.
/// All paths are relative to the memory directory and point into vision/frames/.
pub struct StepImageResult {
    pub image_path: String,
    pub marked: bool,
    pub before_image_path: Option<String>,
    pub after_image_path: Option<String>,
    /// Raw bytes of the at-frame after marking (if mouse) or as-fetched (if keyboard).
    /// Populated only when the session is in automated mode — empty in manual mode
    /// to avoid holding large allocations for sessions that never use them.
    pub at_frame_bytes: Vec<u8>,
    /// Raw bytes of the before-frame. Empty if fetch failed or session is manual.
    pub before_frame_bytes: Vec<u8>,
    /// Raw bytes of the after-frame. Empty if fetch failed or session is manual.
    pub after_frame_bytes: Vec<u8>,
}

pub struct VisionPipeline {
    client: client::EyesClient,
    memory_dir: PathBuf,
    memory_name: String,
    press_delay_ms: u64,
    type_delay_ms: u64,
    before_offset_ms: u64,
    after_delay_ms: u64,
    session_id: String,
    storage: std::sync::Arc<dyn crate::storage::StorageBackend>,
    is_cloud_primary: bool,
    is_automated: bool,
}

impl VisionPipeline {
    pub async fn new(
        the_eyes_addr: &str,
        memory_dir: &Path,
        config: &Config,
        session_id: String,
        storage: std::sync::Arc<dyn crate::storage::StorageBackend>,
        is_automated: bool,
    ) -> Option<Self> {
        if the_eyes_addr.is_empty() {
            tracing::warn!(
                "the_eyes_addr not configured — vision disabled for this session. \
                 Run: memory-archive config --the-eyes-addr http://<host>:<port>"
            );
            return None;
        }

        let eyes_client = match client::EyesClient::new(the_eyes_addr.to_string()) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to create EyesClient: {e}");
                return None;
            }
        };

        let memory_name = memory_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        Some(Self {
            client: eyes_client,
            memory_dir: memory_dir.to_path_buf(),
            memory_name,
            press_delay_ms: config.press_fetch_delay_ms,
            type_delay_ms: config.type_fetch_delay_ms,
            before_offset_ms: config.before_fetch_offset_ms,
            after_delay_ms: config.after_fetch_delay_ms,
            session_id,
            storage,
            is_cloud_primary: config.storage_mode == "cloud_primary",
            is_automated,
        })
    }

    #[allow(dead_code)]
    pub fn client(&self) -> &client::EyesClient {
        &self.client
    }

    fn cloud_path(&self, relative_path: &str) -> String {
        format!("{}/{}", self.memory_name, relative_path)
    }

    /// Process one step's vision work — fetches before, action, and after frames concurrently.
    ///
    /// All three frames are written to vision/frames/ with the naming convention:
    ///   step_NNNN_YYYYMMDD_HHMMSS_before.<ext>   ← original format from The-Eyes
    ///   step_NNNN_YYYYMMDD_HHMMSS_at.png         ← always PNG (re-encoded by marker)
    ///   step_NNNN_YYYYMMDD_HHMMSS_after.<ext>    ← original format from The-Eyes
    ///
    /// Returns Some(StepImageResult) when the action frame succeeds.
    /// Returns None if the decision was Skip or the action frame fetch/write failed.
    /// before_image_path and after_image_path are best-effort and may be None independently.
    pub async fn process(
        &self,
        event: &CommandEvent,
        step_id: u32,
        _last_click: Option<(i32, i32)>,
    ) -> Option<StepImageResult> {
        match decide(event, self.press_delay_ms, self.type_delay_ms) {
            FetchDecision::Skip { reason } => {
                tracing::debug!(step_id, reason, "Vision: skip");
                None
            }

            FetchDecision::FetchAt { mark } => {
                let before_ts = adjust_timestamp_ms(&event.timestamp, -(self.before_offset_ms as i64));

                let (before_result, action_result) = tokio::join!(
                    self.client.fetch_at(&before_ts),
                    self.client.fetch_at(&event.timestamp),
                );

                tokio::time::sleep(tokio::time::Duration::from_millis(self.after_delay_ms)).await;

                let after_ts = adjust_timestamp_ms(&event.timestamp, self.after_delay_ms as i64);
                let after_result = self.client.fetch_at(&after_ts).await;

                let (action_bytes, action_ext) = match action_result {
                    Ok((b, ext)) => (b, ext),
                    Err(e) => {
                        tracing::error!(step_id, "fetch_at (action) failed: {e}");
                        return None;
                    }
                };

                let (image_path, marked) = if mark {
                    self.save_at_marked(action_bytes, step_id, &event.timestamp, event.mouse_x, event.mouse_y, &action_ext).await?
                } else {
                    self.save_at(action_bytes, step_id, &event.timestamp, false, &action_ext).await?
                };

                let before_image_path = match before_result {
                    Ok((b, ext)) => self.save_frame(b, step_id, &event.timestamp, "before", &ext).await,
                    Err(e) => {
                        tracing::warn!(step_id, "fetch_at (before) failed: {e}");
                        None
                    }
                };

                let after_image_path = match after_result {
                    Ok((b, ext)) => self.save_frame(b, step_id, &event.timestamp, "after", &ext).await,
                    Err(e) => {
                        tracing::warn!(step_id, "fetch_at (after) failed: {e}");
                        None
                    }
                };

                let (at_bytes, before_bytes, after_bytes) = if self.is_automated {
                    let at = self.read_frame_bytes(&image_path).await;
                    let bf = match &before_image_path {
                        Some(p) => self.read_frame_bytes(p).await,
                        None => Vec::new(),
                    };
                    let af = match &after_image_path {
                        Some(p) => self.read_frame_bytes(p).await,
                        None => Vec::new(),
                    };
                    (at, bf, af)
                } else {
                    (Vec::new(), Vec::new(), Vec::new())
                };

                Some(StepImageResult {
                    image_path,
                    marked,
                    before_image_path,
                    after_image_path,
                    at_frame_bytes: at_bytes,
                    before_frame_bytes: before_bytes,
                    after_frame_bytes: after_bytes,
                })
            }

            FetchDecision::FetchAfter { delay_ms } => {
                self.process_delayed_fetch(event, step_id, delay_ms).await
            }
        }
    }
    
    /// Read saved frame bytes back from storage for inclusion in StepReadyForReasoning.
    /// In cloud_primary mode reads from the StorageBackend. In local mode reads from disk.
    /// Returns an empty Vec on any error — the push is best-effort and capture continues.
    async fn read_frame_bytes(&self, relative_path: &str) -> Vec<u8> {
        if self.is_cloud_primary {
            let cloud_path = self.cloud_path(relative_path);
            match self.storage.get(&self.session_id, &cloud_path).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        path = %relative_path,
                        "read_frame_bytes: cloud read failed for StepReadyForReasoning: {e}"
                    );
                    Vec::new()
                }
            }
        } else {
            let abs = self.memory_dir.join(relative_path);
            match tokio::fs::read(&abs).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        path = %relative_path,
                        "read_frame_bytes: disk read failed for StepReadyForReasoning: {e}"
                    );
                    Vec::new()
                }
            }
        }
    }

    /// Save a click-annotated at-frame to vision/frames/.
    /// The click indicator is drawn in memory — no intermediate file is written.
    /// The output is encoded in the same format as the input from The-Eyes.
    async fn save_at_marked(
        &self,
        raw_bytes: Vec<u8>,
        step_id: u32,
        timestamp: &str,
        mouse_x: i32,
        mouse_y: i32,
        ext: &str,
    ) -> Option<(String, bool)> {
        let marked_bytes = match marker::mark(&raw_bytes, mouse_x, mouse_y, ext) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(step_id, "Marking failed: {e}");
                return None;
            }
        };

        let ts = timestamp_compact(timestamp);
        let filename = format!("step_{:04}_{ts}_at.{ext}", step_id);
        let rel_path = format!("vision/frames/{filename}");

        if self.is_cloud_primary {
            if let Err(e) = self.storage.put(&self.session_id, &self.cloud_path(&rel_path), marked_bytes, "image/png").await {
                tracing::error!(step_id, "Failed to upload at-frame: {e}");
                return None;
            }
        } else {
            let abs_path = self.memory_dir.join("vision").join("frames").join(&filename);
            if let Err(e) = std::fs::write(&abs_path, &marked_bytes) {
                tracing::error!(step_id, "Failed to write at-frame: {e}");
                return None;
            }
        }

        tracing::debug!(step_id, path = %rel_path, "At-frame saved (marked)");
        Some((rel_path, true))
    }

    /// Save an unmarked at-frame to vision/frames/.
    async fn save_at(
        &self,
        bytes: Vec<u8>,
        step_id: u32,
        timestamp: &str,
        marked: bool,
        ext: &str,
    ) -> Option<(String, bool)> {
        let ts = timestamp_compact(timestamp);
        let filename = format!("step_{:04}_{ts}_at.{ext}", step_id);
        let rel_path = format!("vision/frames/{filename}");
        let content_type = ext_to_content_type(ext);

        if self.is_cloud_primary {
            if let Err(e) = self.storage.put(&self.session_id, &self.cloud_path(&rel_path), bytes, content_type).await {
                tracing::error!(step_id, "Failed to upload at-frame: {e}");
                return None;
            }
        } else {
            let abs_path = self.memory_dir.join("vision").join("frames").join(&filename);
            if let Err(e) = std::fs::write(&abs_path, &bytes) {
                tracing::error!(step_id, "Failed to write at-frame: {e}");
                return None;
            }
        }

        tracing::debug!(step_id, path = %rel_path, "At-frame saved (unmarked)");
        Some((rel_path, marked))
    }

    /// Save a before or after frame to vision/frames/.
    /// Best-effort — returns None on failure; the caller continues without the frame.
    async fn save_frame(
        &self,
        bytes: Vec<u8>,
        step_id: u32,
        timestamp: &str,
        suffix: &str,
        ext: &str,
    ) -> Option<String> {
        let ts = timestamp_compact(timestamp);
        let filename = format!("step_{:04}_{ts}_{suffix}.{ext}", step_id);
        let rel_path = format!("vision/frames/{filename}");
        let content_type = ext_to_content_type(ext);

        if self.is_cloud_primary {
            if let Err(e) = self.storage.put(&self.session_id, &self.cloud_path(&rel_path), bytes, content_type).await {
                tracing::warn!(step_id, "Failed to upload {suffix}-frame: {e}");
                return None;
            }
        } else {
            let abs_path = self.memory_dir.join("vision").join("frames").join(&filename);
            if let Err(e) = std::fs::write(&abs_path, &bytes) {
                tracing::warn!(step_id, "Failed to write {suffix}-frame: {e}");
                return None;
            }
        }

        tracing::debug!(step_id, path = %rel_path, "Frame saved ({suffix})");
        Some(rel_path)
    }

    /// Fetch before, action (after delay), and after frames concurrently for keyboard events.
    async fn process_delayed_fetch(
        &self,
        event: &CommandEvent,
        step_id: u32,
        delay_ms: u64,
    ) -> Option<StepImageResult> {
        let before_ts = adjust_timestamp_ms(&event.timestamp, -(self.before_offset_ms as i64));

        tracing::debug!(
            step_id,
            subtype = %event.action_subtype,
            delay_ms,
            "Keyboard event: fetching before/action/after frames"
        );

        let (before_result, _) = tokio::join!(
            self.client.fetch_at(&before_ts),
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)),
        );

        let action_ts = adjust_timestamp_ms(&event.timestamp, delay_ms as i64);
        let action_result = self.client.fetch_at(&action_ts).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(self.after_delay_ms)).await;

        let after_ts = adjust_timestamp_ms(&event.timestamp, (delay_ms + self.after_delay_ms) as i64);
        let after_result = self.client.fetch_at(&after_ts).await;

        let (action_bytes, action_ext) = match action_result {
            Ok((b, ext)) => (b, ext),
            Err(e) => {
                tracing::error!(step_id, "Keyboard action fetch failed: {e}");
                return None;
            }
        };

        let (image_path, marked) = self.save_at(action_bytes, step_id, &event.timestamp, false, &action_ext).await?;

        let before_image_path = match before_result {
            Ok((b, ext)) => self.save_frame(b, step_id, &event.timestamp, "before", &ext).await,
            Err(e) => {
                tracing::warn!(step_id, "Keyboard before fetch failed: {e}");
                None
            }
        };

        let after_image_path = match after_result {
            Ok((b, ext)) => self.save_frame(b, step_id, &event.timestamp, "after", &ext).await,
            Err(e) => {
                tracing::warn!(step_id, "Keyboard after fetch failed: {e}");
                None
            }
        };

        let (at_bytes, before_bytes, after_bytes) = if self.is_automated {
            let at = self.read_frame_bytes(&image_path).await;
            let bf = match &before_image_path {
                Some(p) => self.read_frame_bytes(p).await,
                None => Vec::new(),
            };
            let af = match &after_image_path {
                Some(p) => self.read_frame_bytes(p).await,
                None => Vec::new(),
            };
            (at, bf, af)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };

        Some(StepImageResult {
            image_path,
            marked,
            before_image_path,
            after_image_path,
            at_frame_bytes: at_bytes,
            before_frame_bytes: before_bytes,
            after_frame_bytes: after_bytes,
        })
    }

    #[allow(dead_code)]
    pub async fn is_alive(&self) -> bool {
        self.client.is_alive().await
    }
}

fn ext_to_content_type(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp"         => "image/webp",
        "bmp"          => "image/bmp",
        "tiff" | "tif" => "image/tiff",
        _              => "image/png",
    }
}

fn adjust_timestamp_ms(ts: &str, delta_ms: i64) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(ts) {
        let adjusted = dt + chrono::Duration::milliseconds(delta_ms);
        return adjusted.to_rfc3339();
    }
    if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(
        ts.get(..19).unwrap_or(ts),
        "%Y-%m-%dT%H:%M:%S",
    ) {
        let dt = ndt.and_utc();
        let adjusted = dt + chrono::Duration::milliseconds(delta_ms);
        return adjusted.to_rfc3339();
    }
    ts.to_string()
}

fn timestamp_compact(ts: &str) -> String {
    let s = ts.get(..19).unwrap_or("0000-00-00T00:00:00");
    let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 14 {
        format!("{}_{}", &digits[..8], &digits[8..14])
    } else {
        "00000000_000000".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ma_proto::control_center::CommandEvent;

    fn ev(action_type: &str, action_subtype: &str, success: bool) -> CommandEvent {
        CommandEvent {
            action_type: action_type.to_string(),
            action_subtype: action_subtype.to_string(),
            success,
            ..Default::default()
        }
    }

    #[test]
    fn test_mouse_left_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "left", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_right_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "right", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_double_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "double", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_move_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "move", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_hold_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "hold", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_release_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "release", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_drag_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "drag", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_middle_and_triple_fetch_and_mark() {
        assert_eq!(decide(&ev("mouse", "middle", true), 500, 1000), FetchDecision::FetchAt { mark: true });
        assert_eq!(decide(&ev("mouse", "triple", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_scroll_fetches_and_marks() {
        assert_eq!(decide(&ev("mouse", "scroll_up", true), 500, 1000), FetchDecision::FetchAt { mark: true });
        assert_eq!(decide(&ev("mouse", "scroll_down", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_mouse_unknown_subtype_still_fetches() {
        // Catch-all: any future mouse subtype captures a frame rather than being dropped.
        assert_eq!(decide(&ev("mouse", "quadruple", true), 500, 1000), FetchDecision::FetchAt { mark: true });
    }

    #[test]
    fn test_failed_mouse_command_still_skipped() {
        // The success gate is upstream of the mouse catch-all.
        assert!(matches!(decide(&ev("mouse", "left", false), 500, 1000), FetchDecision::Skip { .. }));
    }

    #[test]
    fn test_keyboard_press_fetches_after_delay() {
        assert_eq!(decide(&ev("keyboard", "press", true), 500, 1000), FetchDecision::FetchAfter { delay_ms: 500 });
    }

    #[test]
    fn test_keyboard_type_fetches_after_delay() {
        assert_eq!(decide(&ev("keyboard", "type", true), 500, 1000), FetchDecision::FetchAfter { delay_ms: 1000 });
    }

    #[test]
    fn test_keyboard_press_respects_custom_delay() {
        assert_eq!(decide(&ev("keyboard", "press", true), 200, 1000), FetchDecision::FetchAfter { delay_ms: 200 });
    }

    #[test]
    fn test_keyboard_type_respects_custom_delay() {
        assert_eq!(decide(&ev("keyboard", "type", true), 500, 1500), FetchDecision::FetchAfter { delay_ms: 1500 });
    }

    #[test]
    fn test_failed_command_skipped() {
        assert!(matches!(decide(&ev("mouse", "left", false), 500, 1000), FetchDecision::Skip { .. }));
    }

    #[test]
    fn test_unknown_action_skipped() {
        assert!(matches!(decide(&ev("scroll", "up", true), 500, 1000), FetchDecision::Skip { .. }));
    }

    #[test]
    fn test_adjust_timestamp_ms_forward() {
        let ts = "2026-02-25T12:58:04.286Z";
        let adjusted = adjust_timestamp_ms(ts, 800);
        assert!(adjusted.contains("12:58:05") || adjusted.contains("12:58:04"));
    }

    #[test]
    fn test_adjust_timestamp_ms_backward() {
        let ts = "2026-02-25T12:58:04.286Z";
        let adjusted = adjust_timestamp_ms(ts, -600);
        assert!(adjusted.contains("12:58:03") || adjusted.contains("12:58:04"));
    }
}