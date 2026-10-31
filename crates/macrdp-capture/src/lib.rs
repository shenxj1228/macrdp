//! macOS screen capture via ScreenCaptureKit

use std::ffi::c_void;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use core_graphics::access::ScreenCaptureAccess;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::{AudioChannelCount, AudioSampleRate};
use tokio::sync::mpsc;

/// Check if Screen Recording permission is granted (no prompt)
pub fn check_screen_recording_permission() -> bool {
    ScreenCaptureAccess.preflight()
}

/// Request Screen Recording permission (triggers system dialog if not granted)
/// Returns true if already granted. Note: even after granting, the app
/// may need to be restarted for the permission to take effect.
pub fn request_screen_recording_permission() -> bool {
    ScreenCaptureAccess.request()
}

/// Open System Settings to Privacy & Security page
pub fn open_screen_recording_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
        .spawn();
}

/// A rectangle region
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Pixel format for screen capture output
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePixelFormat {
    /// BGRA 32-bit (default, needed for OpenH264 and bitmap fallback path)
    Bgra,
    /// NV12 '420f' full range (Y/CbCr 0–255) — zero-copy to VideoToolbox.
    Nv12Full,
    /// NV12 '420v' video/limited range (Y 16–235) — zero-copy to VideoToolbox.
    Nv12Video,
}

impl CapturePixelFormat {
    /// Legacy alias for [`Self::Nv12Full`]; prefer `Self::nv12(macrdp_encode::LIMITED_RANGE)`.
    #[allow(non_upper_case_globals)]
    pub const Nv12: Self = Self::Nv12Full;

    /// NV12 variant matching the encoder's Y'CbCr range (`macrdp_encode::LIMITED_RANGE`).
    pub const fn nv12(limited_range: bool) -> Self {
        if limited_range { Self::Nv12Video } else { Self::Nv12Full }
    }

    /// True for either NV12 variant.
    pub const fn is_nv12(self) -> bool {
        matches!(self, Self::Nv12Full | Self::Nv12Video)
    }
}

/// Frame pixel data — either raw BGRA bytes or a zero-copy CVPixelBuffer reference
#[derive(Debug)]
pub enum FrameData {
    /// BGRA raw bytes copied from CVPixelBuffer (existing behavior)
    Raw(Bytes),
    /// IOSurface-backed CVPixelBuffer — zero copy, passed directly to VideoToolbox
    PixelBuffer(SafePixelBuffer),
}

impl FrameData {
    /// Get raw BGRA bytes if this is a Raw frame. Returns None for PixelBuffer frames.
    pub fn as_bgra_bytes(&self) -> Option<&[u8]> {
        match self {
            FrameData::Raw(bytes) => Some(bytes),
            FrameData::PixelBuffer(_) => None,
        }
    }
}

/// Events from the screen capture pipeline.
#[derive(Debug)]
pub enum CaptureEvent {
    /// A complete frame with pixel data ready for encoding.
    Frame(CapturedFrame),
    /// Desktop is unchanged — no new pixel data.
    Idle,
}

/// A captured screen frame
#[derive(Debug)]
pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    pub data: FrameData,
    /// Bytes per row (valid for FrameData::Raw only)
    pub stride: usize,
    pub timestamp_us: u64,
    /// Regions that changed since the last frame.
    /// Empty means info unavailable — treat as full frame change.
    pub dirty_rects: Vec<Rect>,
}

/// Raw audio frame from ScreenCaptureKit audio callback.
/// Data is interleaved Float32 PCM (L0,R0, L1,R1, ...).
#[derive(Clone)]
pub struct AudioFrame {
    pub data: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u16,
    pub num_samples: usize,
    pub timestamp_ms: u64,
}

/// Configuration for screen capture
#[derive(Clone)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub frame_rate: u32,
    pub pixel_format: CapturePixelFormat,
    pub show_cursor: bool,
}

/// Screen capturer using ScreenCaptureKit
pub struct ScreenCapturer {
    stream: Arc<SCStream>,
    slot: Arc<LatestSlot<CaptureEvent>>,
    /// The full stream configuration: SCK's updateConfiguration replaces the whole object,
    /// so a frame-rate change must resend size/pixel format/cursor/queue depth too.
    config: SCStreamConfiguration,
    busy: SckBusy,
}

/// Set while a helper thread is inside an SCK call on one stream. Cleared by the helper
/// when SCK finally calls back (also on panic), never by the caller: a hung call must keep
/// refusing new ones on *that* stream, so a dead stream cannot pile up a thread per retry
/// (the CG fallback rebuilds the capturer every second) — while other streams are unaffected.
type SckBusy = Arc<std::sync::atomic::AtomicBool>;

struct SckBusyGuard(SckBusy);

impl Drop for SckBusyGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::Release);
    }
}

fn sck_claim(busy: &SckBusy, what: &str) -> Result<SckBusyGuard> {
    if busy.swap(true, std::sync::atomic::Ordering::AcqRel) {
        anyhow::bail!("SCStream {what}: an earlier SCK call on this stream is still hung — not issuing another");
    }
    Ok(SckBusyGuard(Arc::clone(busy)))
}

/// Run a blocking SCK call (whose crate wrapper waits unbounded on the completion callback)
/// on a helper thread and give up after `timeout`. The helper thread keeps waiting and exits
/// when SCK finally calls back; only the caller is unblocked. If the call succeeds after the
/// caller gave up, `undo_late` runs on the helper thread (e.g. stop a stream nobody owns).
/// The channel is a rendezvous (`sync_channel(0)`): once the caller has timed out and dropped
/// the receiver, `send` fails, so a late success can never slip through unnoticed.
fn sck_call_timeout<T: Send + 'static>(
    busy: &SckBusy,
    what: &'static str,
    timeout: std::time::Duration,
    f: impl FnOnce() -> Result<T, screencapturekit::prelude::SCError> + Send + 'static,
    undo_late: impl FnOnce() + Send + 'static,
) -> Result<T> {
    let guard = sck_claim(busy, what)?;
    let (tx, rx) = std::sync::mpsc::sync_channel(0);
    std::thread::Builder::new()
        .name(format!("sck-{what}"))
        .spawn(move || {
            let _guard = guard;
            let r = f();
            if let Err(std::sync::mpsc::SendError(Ok(_))) = tx.send(r) {
                tracing::warn!("SCStream {what} completed after the caller gave up");
                undo_late();
            }
        })
        .context("spawn SCK helper thread")?;
    match rx.recv_timeout(timeout) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("SCStream {what} failed: {e:?}")),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(anyhow::anyhow!("SCStream {what} timed out after {timeout:?}")),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("SCStream {what} helper thread panicked")),
    }
}

/// Single-slot "latest wins" mailbox from the SCK callback thread to the consumer. A new
/// value replaces an unread one (the stale IOSurface is released right away), so the newest
/// frame is never lost — unlike a full `mpsc::channel(1)`, which drops the *new* event.
pub struct LatestSlot<T> {
    state: std::sync::Mutex<(Option<T>, bool)>, // (item, closed)
    notify: tokio::sync::Notify,
}

impl<T> Default for LatestSlot<T> {
    fn default() -> Self {
        Self { state: std::sync::Mutex::new((None, false)), notify: tokio::sync::Notify::new() }
    }
}

impl<T> LatestSlot<T> {
    /// Store `item`, replacing any unread one. Returns `true` if an unread item was replaced.
    pub fn put(&self, item: T) -> bool {
        self.put_with(item, |_, _| {})
    }

    /// Like [`Self::put`], but when an unread item is replaced, `merge(&old, &mut new)` runs
    /// under the slot lock first so `new` can absorb what the consumer never saw. The old
    /// item is dropped after the lock is released.
    pub fn put_with(&self, mut item: T, merge: impl FnOnce(&T, &mut T)) -> bool {
        let old = {
            let mut st = self.state.lock().unwrap();
            if st.1 {
                return false;
            }
            if let Some(old) = &st.0 {
                merge(old, &mut item);
            }
            st.0.replace(item)
        };
        self.notify.notify_one();
        old.is_some()
    }

    /// Store `item` only if the slot is empty (an `Idle` must not hide an unread `Frame`).
    pub fn put_if_empty(&self, item: T) {
        {
            let mut st = self.state.lock().unwrap();
            if st.1 || st.0.is_some() {
                return;
            }
            st.0 = Some(item);
        }
        self.notify.notify_one();
    }

    /// Take the item without waiting. `None` if empty or closed.
    pub fn try_take(&self) -> Option<T> {
        let mut st = self.state.lock().unwrap();
        if st.1 { None } else { st.0.take() }
    }

    /// Wait for an item. `None` once closed. Cancellation safe: the item stays in the slot
    /// until actually taken, and a `notify_one` permit survives a dropped waiter.
    pub async fn take(&self) -> Option<T> {
        loop {
            {
                let mut st = self.state.lock().unwrap();
                if st.1 {
                    return None;
                }
                if let Some(item) = st.0.take() {
                    return Some(item);
                }
            }
            self.notify.notified().await;
        }
    }

    /// Close: drop the pending item, `take` returns `None` from now on.
    pub fn close(&self) {
        let old = {
            let mut st = self.state.lock().unwrap();
            st.1 = true;
            st.0.take()
        };
        drop(old);
        self.notify.notify_one();
    }
}

/// Past this many merged dirty rects, one bounding box is cheaper for everyone downstream.
pub const MAX_MERGED_DIRTY_RECTS: usize = 16;

/// Drop exact duplicates from a merged dirty-rect list; collapse to the bounding box when
/// the list is still longer than [`MAX_MERGED_DIRTY_RECTS`]. Never leaves the list empty
/// (empty means "whole frame").
pub fn compact_dirty_rects(rects: &mut Vec<Rect>) {
    rects.sort_unstable_by_key(|r| (r.x, r.y, r.width, r.height));
    rects.dedup();
    if rects.len() > MAX_MERGED_DIRTY_RECTS {
        let x0 = rects.iter().map(|r| r.x).min().unwrap_or(0);
        let y0 = rects.iter().map(|r| r.y).min().unwrap_or(0);
        let x1 = rects.iter().map(|r| r.x.saturating_add(r.width)).max().unwrap_or(0);
        let y1 = rects.iter().map(|r| r.y.saturating_add(r.height)).max().unwrap_or(0);
        rects.clear();
        rects.push(Rect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 });
    }
}

struct VideoOutputHandler {
    slot: Arc<LatestSlot<CaptureEvent>>,
    pixel_format: CapturePixelFormat,
    /// Unread frames replaced by a newer one (consumer slower than capture).
    dropped: std::sync::atomic::AtomicU64,
}

/// Log one debug line per this many dropped frames.
const DROP_LOG_EVERY: u64 = 60;

struct AudioOutputHandler {
    audio_tx: Arc<mpsc::Sender<AudioFrame>>,
}

impl SCStreamOutputTrait for VideoOutputHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Screen {
            return;
        }

        let event = match self.pixel_format {
            CapturePixelFormat::Nv12Full | CapturePixelFormat::Nv12Video => extract_frame_nv12(&sample),
            CapturePixelFormat::Bgra => extract_frame(&sample),
        };
        let Some(event) = event else { return };

        // Never blocks the SCK callback: a Frame replaces an unread Frame (latest wins), an
        // Idle only fills an empty slot so it cannot hide an unread Frame. The replaced
        // Frame's dirty rects are folded into the new one so the consumer still refreshes
        // the regions that only changed in the frame it never saw.
        let replaced = match event {
            CaptureEvent::Frame(_) => self.slot.put_with(event, |old, new| {
                if let (CaptureEvent::Frame(old), CaptureEvent::Frame(new)) = (old, new) {
                    // Empty = whole frame; stays empty if either side was
                    if old.dirty_rects.is_empty() || new.dirty_rects.is_empty() {
                        new.dirty_rects.clear();
                    } else {
                        new.dirty_rects.extend(old.dirty_rects.iter().cloned());
                        compact_dirty_rects(&mut new.dirty_rects);
                    }
                }
            }),
            CaptureEvent::Idle => {
                self.slot.put_if_empty(event);
                false
            }
        };
        if replaced {
            let n = self.dropped.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // `is_multiple_of` needs Rust 1.87; workspace MSRV is 1.75
            #[allow(clippy::manual_is_multiple_of)]
            if n % DROP_LOG_EVERY == 0 {
                tracing::debug!(dropped = n, "Capture slot overwritten — stale frames dropped");
            }
        }
    }
}

impl SCStreamOutputTrait for AudioOutputHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }

        let Ok(audio_buffer_list) = sample.audio_buffer_list() else { return };

        let timestamp = sample.presentation_timestamp();
        let timestamp_ms = if timestamp.timescale > 0 {
            ((timestamp.value as u128 * 1000) / timestamp.timescale as u128) as u64
        } else {
            0
        };

        // Get sample rate and channel count from format description
        let (sample_rate, channels) = if let Some(fmt) = sample.format_description() {
            let sr = fmt.audio_sample_rate().unwrap_or(48000.0) as u32;
            let ch = fmt.audio_channel_count().unwrap_or(2) as u16;
            (sr, ch)
        } else {
            (48000u32, 2u16)
        };

        let num_buffers = audio_buffer_list.num_buffers();

        // SCK delivers stereo as a single interleaved buffer (num_buffers == 1, number_channels == 2)
        // or as separate per-channel buffers (num_buffers == 2, number_channels == 1 each).
        let interleaved_data = if num_buffers <= 1 {
            if let Some(buf) = audio_buffer_list.get(0) {
                let raw = buf.data();
                let samples: &[f32] = unsafe {
                    std::slice::from_raw_parts(raw.as_ptr() as *const f32, raw.len() / 4)
                };
                samples.to_vec()
            } else {
                return;
            }
        } else {
            // Non-interleaved: one buffer per channel — interleave manually
            let channel_bufs: Vec<&[f32]> = (0..num_buffers)
                .filter_map(|i| audio_buffer_list.get(i))
                .map(|buf| {
                    let raw = buf.data();
                    unsafe {
                        std::slice::from_raw_parts(raw.as_ptr() as *const f32, raw.len() / 4)
                    }
                })
                .collect();

            if channel_bufs.is_empty() {
                return;
            }
            let n = channel_bufs[0].len();
            let mut out = Vec::with_capacity(n * channel_bufs.len());
            for i in 0..n {
                for ch in &channel_bufs {
                    out.push(ch[i]);
                }
            }
            out
        };

        let num_samples = if channels > 0 {
            interleaved_data.len() / channels as usize
        } else {
            interleaved_data.len()
        };

        let frame = AudioFrame {
            data: interleaved_data,
            sample_rate,
            channels,
            num_samples,
            timestamp_ms,
        };
        let _ = self.audio_tx.try_send(frame);
    }
}

fn extract_frame(sample: &CMSampleBuffer) -> Option<CaptureEvent> {
    use screencapturekit::cm::SCFrameStatus;

    // Idle means desktop is unchanged — propagate as CaptureEvent::Idle.
    // Blank/Suspended/Stopped are non-displayable states — discard silently.
    match sample.frame_status() {
        Some(SCFrameStatus::Idle) => {
            return Some(CaptureEvent::Idle);
        }
        Some(SCFrameStatus::Blank)
        | Some(SCFrameStatus::Suspended)
        | Some(SCFrameStatus::Stopped) => {
            return None;
        }
        _ => {}
    }

    let pixel_buffer = sample.pixel_buffer()?;

    let guard = pixel_buffer.lock(CVPixelBufferLockFlags::READ_ONLY).ok()?;

    let width = guard.width() as u32;
    let height = guard.height() as u32;
    let stride = guard.bytes_per_row();
    let pixels = unsafe { guard.as_slice()? };

    if width == 0 || height == 0 || pixels.is_empty() {
        return None;
    }

    // Extract dirty rects from the sample buffer
    // Extract dirty rects — screencapturekit's CGRect has origin/size fields
    let dirty_rects = sample
        .dirty_rects()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.size.width > 0.0 && r.size.height > 0.0)
        .map(|r| Rect {
            x: r.origin.x.max(0.0) as u32,
            y: r.origin.y.max(0.0) as u32,
            width: r.size.width as u32,
            height: r.size.height as u32,
        })
        .collect::<Vec<_>>();

    let data = Bytes::copy_from_slice(pixels);

    let t = sample.presentation_timestamp();
    let timestamp_us = if t.timescale > 0 {
        ((t.value as u128 * 1_000_000) / t.timescale as u128) as u64
    } else {
        0
    };

    Some(CaptureEvent::Frame(CapturedFrame {
        width,
        height,
        data: FrameData::Raw(data),
        stride,
        timestamp_us,
        dirty_rects,
    }))
}

/// Extract a frame in NV12 mode — zero-copy CVPixelBuffer wrapped as SafePixelBuffer.
/// The pixel buffer is retained and passed through the channel without locking or copying.
fn extract_frame_nv12(sample: &CMSampleBuffer) -> Option<CaptureEvent> {
    use screencapturekit::cm::SCFrameStatus;

    // Idle means desktop is unchanged — propagate as CaptureEvent::Idle.
    // Blank/Suspended/Stopped are non-displayable states — discard silently.
    match sample.frame_status() {
        Some(SCFrameStatus::Idle) => {
            return Some(CaptureEvent::Idle);
        }
        Some(SCFrameStatus::Blank)
        | Some(SCFrameStatus::Suspended)
        | Some(SCFrameStatus::Stopped) => {
            return None;
        }
        _ => {}
    }

    let pixel_buffer = sample.pixel_buffer()?;

    // width()/height() read from the CVPixelBuffer header — no lock required
    let width = pixel_buffer.width() as u32;
    let height = pixel_buffer.height() as u32;

    if width == 0 || height == 0 {
        return None;
    }

    // Extract dirty rects (same logic as the BGRA path)
    let dirty_rects = sample
        .dirty_rects()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.size.width > 0.0 && r.size.height > 0.0)
        .map(|r| Rect {
            x: r.origin.x.max(0.0) as u32,
            y: r.origin.y.max(0.0) as u32,
            width: r.size.width as u32,
            height: r.size.height as u32,
        })
        .collect::<Vec<_>>();

    let t = sample.presentation_timestamp();
    let timestamp_us = if t.timescale > 0 {
        ((t.value as u128 * 1_000_000) / t.timescale as u128) as u64
    } else {
        0
    };

    // Zero-copy: retain the CVPixelBuffer and wrap it as SafePixelBuffer
    let safe_buf = unsafe { SafePixelBuffer::from_raw(pixel_buffer.as_ptr()) };

    Some(CaptureEvent::Frame(CapturedFrame {
        width,
        height,
        data: FrameData::PixelBuffer(safe_buf),
        stride: 0, // Not applicable for NV12 PixelBuffer mode
        timestamp_us,
        dirty_rects,
    }))
}

/// Detect the main display's native scale factor (1 for non-Retina, 2 for Retina).
pub fn detect_display_scale() -> Result<u32> {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    let physical_w = main.pixels_wide() as u32;
    let content = SCShareableContent::get()
        .context("Failed to get shareable content")?;
    let display = content.displays().into_iter().next()
        .context("No display found")?;
    let logical_w = display.width();
    let scale = if logical_w > 0 { physical_w / logical_w } else { 1 };
    Ok(scale.max(1))
}

/// Query the main display's resolution (from ScreenCaptureKit, used for capture sizing)
pub fn detect_display_size() -> Result<(u32, u32)> {
    let content = SCShareableContent::get()
        .context("Failed to get shareable content")?;
    let display = content
        .displays()
        .into_iter()
        .next()
        .context("No display found")?;
    Ok((display.width(), display.height()))
}

/// Query the main display's logical bounds from CoreGraphics.
/// This is the coordinate system CGEvent uses, and MUST be used for mouse mapping.
/// May differ from SCDisplay dimensions on non-standard scaling modes.
pub fn detect_cg_display_size() -> Result<(u32, u32)> {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    let bounds = main.bounds();
    let w = bounds.size.width as u32;
    let h = bounds.size.height as u32;
    if w == 0 || h == 0 {
        anyhow::bail!("CGDisplay returned zero bounds");
    }
    Ok((w, h))
}

impl ScreenCapturer {
    /// Create a new screen capturer for the main display.
    ///
    /// If `audio_tx` is `Some`, audio capture is enabled and frames are sent
    /// to the provided channel as `AudioFrame` (interleaved Float32 PCM, 48 kHz stereo).
    pub async fn new(
        config: CaptureConfig,
        audio_tx: Option<mpsc::Sender<AudioFrame>>,
    ) -> Result<Self> {
        // SCShareableContent::get() is synchronous, run in blocking task
        let content = tokio::task::spawn_blocking(SCShareableContent::get)
            .await?
            .context("Failed to get shareable content (Screen Recording permission needed)")?;

        let display = content
            .displays()
            .into_iter()
            .next()
            .context("No display found")?;

        let actual_width = if config.width == 0 {
            display.width() as u32
        } else {
            config.width
        };
        let actual_height = if config.height == 0 {
            display.height() as u32
        } else {
            config.height
        };

        let filter = SCContentFilter::create()
            .with_display(&display)
            .with_excluding_windows(&[])
            .build()?;

        let frame_interval = CMTime::new(1, config.frame_rate as i32);

        // Only ask SCK to rescale when the requested size differs from the display's
        // physical pixel size — otherwise it is a pointless GPU resample pass.
        let cg_main = core_graphics::display::CGDisplay::main();
        let needs_scale = actual_width != cg_main.pixels_wide() as u32
            || actual_height != cg_main.pixels_high() as u32;

        let captures_audio = audio_tx.is_some();
        let stream_config = SCStreamConfiguration::new()
            .with_width(actual_width)
            .with_height(actual_height)
            .with_scales_to_fit(needs_scale)
            // The IOSurface pool size equals queueDepth (valid range 3-8). The server holds up
            // to three surfaces at once (previous last_frame_cache, the frame being encoded,
            // the newest one waiting in the slot), so 4 always leaves SCK one to write into;
            // anything less starves the pool and SCK drops frames.
            .with_queue_depth(4)
            // Match the encoder: BT.601 matrix, sRGB primaries.
            // Values are the CFString contents of kCGDisplayStreamYCbCrMatrix_ITU_R_601_4 / kCGColorSpaceSRGB.
            // screencapturekit 11 retains these values for the configuration's lifetime;
            // newer SCK declares both as `assign`, so a dangling CFString would make
            // SCStream's configuration copy crash.
            .with_color_matrix("ITU_R_601_4")?
            .with_color_space_name("kCGColorSpaceSRGB")?
            .with_minimum_frame_interval(&frame_interval)
            .with_pixel_format(match config.pixel_format {
                // Must match the encoder session's NV12 tag (see `K_CV_PIXEL_FORMAT_NV12` in
                // macrdp-encode/src/videotoolbox.rs); mismatch = VT rejects the buffer.
                CapturePixelFormat::Nv12Full => PixelFormat::YCbCr_420f,
                CapturePixelFormat::Nv12Video => PixelFormat::YCbCr_420v,
                CapturePixelFormat::Bgra => PixelFormat::BGRA,
            })
            .with_shows_cursor(config.show_cursor)
            .with_captures_audio(captures_audio)
            .with_sample_rate(AudioSampleRate::Rate48000)
            .with_channel_count(AudioChannelCount::Stereo)
            .with_excludes_current_process_audio(true);

        // One "latest wins" slot for capture events: the consumer only ever wants the newest
        // frame, so a deeper buffer just holds stale IOSurfaces. SCK does not resend a frame
        // it already delivered, so the newest one must replace the old, never be dropped.
        let slot = Arc::new(LatestSlot::default());

        let video_handler = VideoOutputHandler {
            slot: Arc::clone(&slot),
            pixel_format: config.pixel_format,
            dropped: std::sync::atomic::AtomicU64::new(0),
        };

        let mut stream = SCStream::new(&filter, &stream_config)?;
        stream.add_output_handler(video_handler, SCStreamOutputType::Screen)?;

        // Register audio handler only if a sender was provided
        if let Some(tx) = audio_tx {
            let audio_handler = AudioOutputHandler {
                audio_tx: Arc::new(tx),
            };
            stream.add_output_handler(audio_handler, SCStreamOutputType::Audio)?;
        }

        let stream = Arc::new(stream);
        let busy: SckBusy = Arc::default();
        {
            let (s, late, b) = (Arc::clone(&stream), Arc::clone(&stream), Arc::clone(&busy));
            // Started after we gave up: nobody owns it, stop it instead of leaving it rendering.
            // The up-to-5 s wait runs on the blocking pool so a current_thread runtime keeps
            // serving input/ACKs/accept meanwhile.
            tokio::task::spawn_blocking(move || {
                sck_call_timeout(&b, "startCapture", std::time::Duration::from_secs(5), move || s.start_capture(), move || { let _ = late.stop_capture(); })
            })
            .await?
            .context("Failed to start capture")?;
        }

        tracing::info!(
            width = actual_width,
            height = actual_height,
            fps = config.frame_rate,
            pixel_format = ?config.pixel_format,
            audio = captures_audio,
            scales_to_fit = needs_scale,
            "Screen capture started"
        );

        Ok(Self { stream, slot, config: stream_config, busy })
    }

    /// Receive the next capture event (async, cancellation safe)
    pub async fn next_frame(&mut self) -> Option<CaptureEvent> {
        self.slot.take().await
    }

    /// Try to get a buffered capture event without waiting. Returns None if no event ready.
    pub fn try_next_frame(&mut self) -> Option<CaptureEvent> {
        self.slot.try_take()
    }

    /// Close the frame slot: `next_frame` returns `None` from now on, so the caller falls
    /// through to its SCStream-stopped path (which rebuilds the capturer).
    pub fn close(&mut self) {
        self.slot.close();
    }

    /// Update capture frame rate at runtime. Blocks until SCK confirms the new
    /// configuration, at most 2 s. Prefer [`Self::set_frame_rate_async`] from tokio tasks.
    pub fn set_frame_rate(&self, fps: u32) -> anyhow::Result<()> {
        apply_frame_rate(Arc::clone(&self.stream), self.config.clone(), &self.busy, fps)
    }

    /// Same as [`Self::set_frame_rate`] but runs the blocking SCK call on
    /// `spawn_blocking`, so it never stalls a tokio worker thread.
    pub async fn set_frame_rate_async(&self, fps: u32) -> anyhow::Result<()> {
        let stream = Arc::clone(&self.stream);
        let config = self.config.clone();
        let busy = Arc::clone(&self.busy);
        tokio::task::spawn_blocking(move || apply_frame_rate(stream, config, &busy, fps)).await?
    }
}

impl Drop for ScreenCapturer {
    /// Stop the SCStream so a replaced capturer does not keep rendering into its IOSurface
    /// pool. `close()` only closes the slot, so this is the single stop call.
    ///
    /// Fire and forget: the stop runs on a helper thread and we never wait for it, since a
    /// hung SCK would otherwise stall the (current_thread) runtime that drops us.
    fn drop(&mut self) {
        self.slot.close();
        let guard = match sck_claim(&self.busy, "stopCapture") {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "Screen capture stop");
                return;
            }
        };
        let stream = Arc::clone(&self.stream);
        // SCK copies the configuration into the stream by pointer (its color properties are
        // `assign`), so the retained CFStrings live as long as this config object. Keep a
        // reference alive until stop_capture returns instead of letting `config` drop first.
        let keep_config = self.config.clone();
        let spawned = std::thread::Builder::new().name("sck-stopCapture".into()).spawn(move || {
            let _guard = guard;
            let _config = keep_config;
            if let Err(e) = stream.stop_capture() {
                tracing::warn!(error = ?e, "Screen capture stop");
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "Screen capture stop: spawn SCK helper thread");
        }
    }
}

/// `config` is a retained reference to the capturer's own configuration object
/// (`SCStreamConfiguration::clone` retains the same ObjC object, it does not copy), so only
/// the interval changes and every other field stays as built.
fn apply_frame_rate(stream: Arc<SCStream>, mut config: SCStreamConfiguration, busy: &SckBusy, fps: u32) -> anyhow::Result<()> {
    let interval = CMTime::new(1, fps as i32);
    config.set_minimum_frame_interval(&interval);
    sck_call_timeout(busy, "updateConfiguration", std::time::Duration::from_secs(2), move || stream.update_configuration(&config), || {})
        .context("Failed to update SCStream frame rate")?;
    tracing::info!(fps, "Capture frame rate updated");
    Ok(())
}

/// Fallback capturer using CGDisplayCreateImage (CoreGraphics).
/// Works during lock screen because it captures at the display level,
/// below the window server / ScreenCaptureKit layer.
pub struct CgFallbackCapturer {
    display_id: u32,
    width: u32,
    height: u32,
    frame_interval: std::time::Duration,
}

impl CgFallbackCapturer {
    /// Create a fallback capturer for the main display
    pub fn new(config: &CaptureConfig) -> Self {
        let display_id = core_graphics::display::CGDisplay::main().id;
        let fps = if config.frame_rate > 0 { config.frame_rate } else { 30 };
        Self {
            display_id,
            width: config.width,
            height: config.height,
            frame_interval: std::time::Duration::from_micros(1_000_000 / fps as u64),
        }
    }

    /// Capture a single frame using CGDisplayCreateImage.
    ///
    /// `CGDisplayCreateImage` always returns the display's physical pixel size. When the
    /// configured size differs (Retina + client-chosen resolution) the image is rescaled
    /// to the configured size so `width/height/stride/data` stay consistent for the encoder.
    pub fn capture_frame(&self) -> Option<CapturedFrame> {
        let display = core_graphics::display::CGDisplay::new(self.display_id);
        let image = display.image()?;

        let target_w = if self.width > 0 { self.width } else { image.width() as u32 };
        let target_h = if self.height > 0 { self.height } else { image.height() as u32 };

        let (raw, stride) = cg_image_to_bgra(&image, target_w, target_h)?;

        Some(CapturedFrame {
            width: target_w,
            height: target_h,
            data: FrameData::Raw(Bytes::from(raw)),
            stride,
            timestamp_us: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_micros() as u64,
            dirty_rects: vec![],
        })
    }

    /// Frame interval for pacing
    pub fn frame_interval(&self) -> std::time::Duration {
        self.frame_interval
    }
}

/// Convert a CGImage to tightly-packed BGRA at `width`x`height`.
/// Returns `(pixels, stride)`. If the image already matches, its bytes are copied as-is
/// (keeping CG's row stride); otherwise it is drawn through a CGBitmapContext, which rescales.
fn cg_image_to_bgra(
    image: &core_graphics::image::CGImage,
    width: u32,
    height: u32,
) -> Option<(Vec<u8>, usize)> {
    use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::{CGContext, CGInterpolationQuality};
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};

    if width == 0 || height == 0 {
        return None;
    }

    if image.width() as u32 == width
        && image.height() as u32 == height
        && image.bits_per_pixel() == 32
    {
        return Some((image.data().bytes().to_vec(), image.bytes_per_row()));
    }

    let stride = width as usize * 4;
    let mut buf = vec![0u8; stride * height as usize];
    {
        // Context borrows `buf` as its backing store; it is dropped before `buf` is returned.
        let ctx = CGContext::create_bitmap_context(
            Some(buf.as_mut_ptr() as *mut c_void),
            width as usize,
            height as usize,
            8,
            stride,
            &CGColorSpace::create_device_rgb(),
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little, // BGRA in memory
        );
        ctx.set_interpolation_quality(CGInterpolationQuality::CGInterpolationQualityLow);
        ctx.draw_image(
            CGRect::new(
                &CGPoint::new(0.0, 0.0),
                &CGSize::new(width as f64, height as f64),
            ),
            image,
        );
        ctx.flush();
    }
    Some((buf, stride))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::CGContext;
    use core_graphics::image::CGImage;

    /// Build a solid-colour BGRA CGImage of the given size (no screen access needed).
    fn solid_image(w: usize, h: usize, bgra: [u8; 4]) -> CGImage {
        let stride = w * 4;
        let mut px = Vec::with_capacity(stride * h);
        for _ in 0..w * h {
            px.extend_from_slice(&bgra);
        }
        let mut ctx = CGContext::create_bitmap_context(
            None,
            w,
            h,
            8,
            stride,
            &CGColorSpace::create_device_rgb(),
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little,
        );
        ctx.data().copy_from_slice(&px);
        ctx.create_image().unwrap()
    }

    #[test]
    fn fallback_frame_scales_physical_to_configured_size() {
        // "Retina" physical 64x48 vs configured 32x24
        let img = solid_image(64, 48, [10, 20, 30, 255]);
        let (data, stride) = cg_image_to_bgra(&img, 32, 24).unwrap();
        assert!(stride >= 32 * 4);
        assert!(data.len() >= stride * 24);
        // Every pixel must be the same colour as the source — proves scaling, not cropping.
        for row in 0..24 {
            for x in 0..32 {
                let o = row * stride + x * 4;
                assert_eq!(&data[o..o + 4], &[10, 20, 30, 255], "pixel ({x},{row})");
            }
        }
    }

    #[test]
    fn fallback_frame_passthrough_when_sizes_match() {
        let img = solid_image(16, 8, [1, 2, 3, 255]);
        let (data, stride) = cg_image_to_bgra(&img, 16, 8).unwrap();
        assert!(stride >= 16 * 4);
        assert!(data.len() >= stride * 8);
        assert_eq!(&data[0..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn fallback_rejects_zero_size() {
        let img = solid_image(4, 4, [0; 4]);
        assert!(cg_image_to_bgra(&img, 0, 4).is_none());
    }

    #[test]
    fn slot_latest_frame_wins() {
        let slot = LatestSlot::<u32>::default();
        assert!(!slot.put(1));
        assert!(slot.put(2)); // unread 1 replaced
        assert_eq!(slot.try_take(), Some(2));
        assert_eq!(slot.try_take(), None);
    }

    #[test]
    fn slot_idle_does_not_hide_frame() {
        let slot = LatestSlot::<u32>::default();
        slot.put(7);
        slot.put_if_empty(0);
        assert_eq!(slot.try_take(), Some(7));
        slot.put_if_empty(0);
        assert_eq!(slot.try_take(), Some(0));
    }

    #[test]
    fn slot_put_with_merges_replaced_item() {
        let slot = LatestSlot::<Vec<u32>>::default();
        assert!(!slot.put_with(vec![1], |_, _| unreachable!()));
        assert!(slot.put_with(vec![2], |old, new| new.extend(old)));
        assert_eq!(slot.try_take(), Some(vec![2, 1]));
        assert!(!slot.put_with(vec![3], |_, _| unreachable!())); // slot was empty
        assert_eq!(slot.try_take(), Some(vec![3]));
    }

    #[tokio::test]
    async fn slot_take_waits_then_none_after_close() {
        let slot = Arc::new(LatestSlot::<u32>::default());
        let producer = Arc::clone(&slot);
        let waiter = tokio::spawn(async move { slot.take().await });
        tokio::task::yield_now().await;
        producer.put(5);
        assert_eq!(waiter.await.unwrap(), Some(5));
        producer.put(6);
        producer.close();
        assert_eq!(producer.try_take(), None);
        assert_eq!(producer.take().await, None);
        assert!(!producer.put(8)); // ignored after close
        assert_eq!(producer.take().await, None);
    }

    extern "C" {
        fn CVPixelBufferGetPixelFormatType(pixel_buffer: *mut c_void) -> u32;
        /// TCC query only — safe without a GUI session, unlike SCShareableContent::get().
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    /// Next real `Frame` (skipping `Idle`) as (pixel format fourcc, width, height).
    async fn next_pixel_frame(cap: &mut ScreenCapturer) -> (u32, u32, u32) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, cap.next_frame()).await
                .expect("no frame within 10s").expect("capturer closed");
            if let CaptureEvent::Frame(f) = ev {
                let fmt = match &f.data {
                    FrameData::PixelBuffer(pb) => unsafe { CVPixelBufferGetPixelFormatType(pb.as_ptr()) },
                    FrameData::Raw(_) => 0,
                };
                return (fmt, f.width, f.height);
            }
        }
    }

    /// Needs Screen Recording permission and a real display session (skips otherwise).
    /// Verifies a frame-rate change keeps the NV12 pixel format and size. SCStream holds
    /// exactly one configuration object and updateConfiguration replaces it, so a fresh
    /// `SCStreamConfiguration::new()` with only the interval set would reset width/height/
    /// pixelFormat(BGRA)/showsCursor/queueDepth to their defaults; the capturer therefore
    /// resends its own full config.
    ///
    /// Ignored by default: on a headless CI runner (no GUI session) SCK segfaults instead of
    /// erroring, taking the whole test binary down. Run locally with
    /// `cargo test -p macrdp-capture -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a real display session; SCK segfaults on headless CI"]
    async fn frame_rate_update_keeps_stream_config() {
        // Only a missing TCC grant is a legitimate skip; any `new()` failure past that is a bug.
        // Preflight the TCC grant instead of calling SCShareableContent::get(): on a headless
        // box (CI runner, no GUI session) the SCK call crashes the process instead of erroring.
        if !unsafe { CGPreflightScreenCaptureAccess() } {
            eprintln!("skipped: no Screen Recording permission (TCC not granted)");
            return;
        }
        let mut cap = ScreenCapturer::new(
            CaptureConfig { width: 0, height: 0, frame_rate: 30, pixel_format: CapturePixelFormat::Nv12Full, show_cursor: false },
            None,
        ).await.unwrap();
        let before = next_pixel_frame(&mut cap).await;
        assert_eq!(before.0, u32::from_be_bytes(*b"420f"), "initial frame is not NV12 420f");
        cap.set_frame_rate_async(5).await.unwrap();
        // A config change makes SCK emit one fresh frame even on a static desktop; waiting
        // for more would hang there, so check exactly one (10 s timeout inside).
        let after = next_pixel_frame(&mut cap).await;
        assert_eq!(after, before, "frame rate update changed pixel format or size");
    }

    #[test]
    fn compact_dirty_rects_dedups_then_collapses() {
        let r = |x, y| Rect { x, y, width: 10, height: 10 };
        let mut v = vec![r(0, 0), r(5, 5), r(0, 0)];
        compact_dirty_rects(&mut v);
        assert_eq!(v, vec![r(0, 0), r(5, 5)]);
        let mut v: Vec<Rect> = (0..MAX_MERGED_DIRTY_RECTS as u32 + 1).map(|i| r(i * 10, 3)).collect();
        compact_dirty_rects(&mut v);
        assert_eq!(v, vec![Rect { x: 0, y: 3, width: MAX_MERGED_DIRTY_RECTS as u32 * 10 + 10, height: 10 }]);
    }
}

// ---------------------------------------------------------------------------
// CoreVideo FFI for CVPixelBuffer retain/release and plane access
// ---------------------------------------------------------------------------

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    fn CVPixelBufferRetain(pixel_buffer: *mut c_void) -> *mut c_void;
    fn CVPixelBufferRelease(pixel_buffer: *mut c_void);
    fn CVPixelBufferLockBaseAddress(pixel_buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(pixel_buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddressOfPlane(pixel_buffer: *mut c_void, plane: usize) -> *mut u8;
    fn CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer: *mut c_void, plane: usize) -> usize;
    fn CVPixelBufferGetHeightOfPlane(pixel_buffer: *mut c_void, plane: usize) -> usize;
}

/// kCVPixelBufferLock_ReadOnly
const CV_PIXEL_BUFFER_LOCK_READ_ONLY: u64 = 0x0000_0001;

// ---------------------------------------------------------------------------
// NV12PlaneData — extracted Y and UV plane data from an NV12 pixel buffer
// ---------------------------------------------------------------------------

/// Holds copied plane data from an NV12 CVPixelBuffer.
/// Used for the OpenH264 software encoding fallback path.
pub struct NV12PlaneData {
    /// Y (luma) plane data, one byte per pixel, row-major
    pub y_data: Vec<u8>,
    /// Y plane stride (bytes per row, may include padding)
    pub y_stride: usize,
    /// UV (chroma) plane data, interleaved U/V, half resolution
    pub uv_data: Vec<u8>,
    /// UV plane stride (bytes per row, may include padding)
    pub uv_stride: usize,
    /// Width of the Y plane in pixels
    pub width: usize,
    /// Height of the Y plane in pixels
    pub height: usize,
}

// ---------------------------------------------------------------------------
// SafePixelBuffer — RAII wrapper around a retained CVPixelBufferRef
// ---------------------------------------------------------------------------

/// A safe RAII wrapper around a `CVPixelBufferRef` that manages the
/// retain/release lifecycle. Intended for zero-copy frame passing to
/// VideoToolbox (hardware encoder) while also supporting a locked-read
/// path for OpenH264 (software encoder fallback).
///
/// # Safety
///
/// The inner pointer must originate from a valid `CVPixelBufferRef`.
/// `Send` is implemented because IOSurface-backed pixel buffers are safe
/// to transfer across threads. `Sync` is deliberately NOT implemented
/// because `CVPixelBufferLockBaseAddress` / `UnlockBaseAddress` are not
/// safe for concurrent access from multiple threads.
pub struct SafePixelBuffer {
    ptr: *mut c_void,
}

// SAFETY: IOSurface-backed CVPixelBuffers can be sent across threads.
// We do NOT implement Sync — lock/unlock is not thread-safe for
// concurrent access.
unsafe impl Send for SafePixelBuffer {}

impl SafePixelBuffer {
    /// Create a `SafePixelBuffer` by retaining the given `CVPixelBufferRef`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null `CVPixelBufferRef`.
    pub unsafe fn from_raw(ptr: *mut c_void) -> Self {
        debug_assert!(!ptr.is_null(), "CVPixelBufferRef must not be null");
        CVPixelBufferRetain(ptr);
        Self { ptr }
    }

    /// Return the raw `CVPixelBufferRef` pointer (e.g. for passing to
    /// VideoToolbox's `VTCompressionSessionEncodeFrame`).
    pub fn as_ptr(&self) -> *mut c_void {
        self.ptr
    }

    /// Create a new `SafePixelBuffer` that shares the same underlying
    /// `CVPixelBuffer`, incrementing its retain count.
    pub fn clone_ref(&self) -> Self {
        unsafe {
            CVPixelBufferRetain(self.ptr);
        }
        Self { ptr: self.ptr }
    }

    /// Lock the pixel buffer, copy NV12 plane data out, and unlock.
    ///
    /// This is the software-encoding path: we lock the buffer read-only,
    /// memcpy the Y and UV planes into owned `Vec<u8>`s, then unlock.
    /// The lock is held for the shortest possible duration.
    ///
    /// Returns `None` if the lock fails or plane pointers are null.
    pub fn lock_and_read_nv12(&self) -> Option<NV12PlaneData> {
        unsafe {
            // Lock for read-only access
            let status = CVPixelBufferLockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY);
            if status != 0 {
                tracing::warn!(status, "CVPixelBufferLockBaseAddress failed");
                return None;
            }

            let result = self.read_nv12_planes();

            // Always unlock, even if plane read failed
            CVPixelBufferUnlockBaseAddress(self.ptr, CV_PIXEL_BUFFER_LOCK_READ_ONLY);

            result
        }
    }

    /// Read Y and UV planes while the buffer is locked.
    /// Caller must ensure the buffer is locked before calling.
    unsafe fn read_nv12_planes(&self) -> Option<NV12PlaneData> {
        // Plane 0 = Y (luma)
        let y_ptr = CVPixelBufferGetBaseAddressOfPlane(self.ptr, 0);
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(self.ptr, 0);
        let y_height = CVPixelBufferGetHeightOfPlane(self.ptr, 0);

        // Plane 1 = UV (chroma, interleaved)
        let uv_ptr = CVPixelBufferGetBaseAddressOfPlane(self.ptr, 1);
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(self.ptr, 1);
        let uv_height = CVPixelBufferGetHeightOfPlane(self.ptr, 1);

        if y_ptr.is_null() || uv_ptr.is_null() {
            tracing::warn!("NV12 plane base address is null");
            return None;
        }

        let y_len = y_stride * y_height;
        let uv_len = uv_stride * uv_height;

        let y_data = std::slice::from_raw_parts(y_ptr, y_len).to_vec();
        let uv_data = std::slice::from_raw_parts(uv_ptr, uv_len).to_vec();

        // Width is derived from plane 0 stride and pixel format.
        // For NV12 Y plane, each pixel is one byte, but stride may include
        // padding. We use the plane height directly and report stride so
        // callers can handle padding.
        Some(NV12PlaneData {
            y_data,
            y_stride,
            uv_data,
            uv_stride,
            width: y_stride, // conservative: callers should clamp to actual width
            height: y_height,
        })
    }
}

impl Drop for SafePixelBuffer {
    fn drop(&mut self) {
        // SAFETY: ptr was retained in `from_raw`, so we must release it.
        unsafe {
            CVPixelBufferRelease(self.ptr);
        }
    }
}

impl std::fmt::Debug for SafePixelBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SafePixelBuffer")
            .field("ptr", &self.ptr)
            .finish()
    }
}
