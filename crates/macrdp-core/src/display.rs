use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, GfxFrameUpdate, GfxUncompressedUpdate,
    PixelFormat as RdpPixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
    UncompressedRect, gfx::GfxState,
};
use macrdp_audio::SharedAudioTx;
use macrdp_capture::{CaptureConfig, CapturePixelFormat, CapturedFrame, CgFallbackCapturer, FrameData, SafePixelBuffer, ScreenCapturer};
use macrdp_encode::{self, EncoderConfig, Quality, VideoEncoder};
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bitrate_controller::{BitrateController, FrameStats, NetworkQuality, is_private_ip};
use crate::cursor::CursorWatcher;
use crate::handler::MouseCoordMapper;
use crate::perf_stats::SharedPerfStats;

/// Maximum total dirty area (in pixels) for the uncompressed GFX path.
/// Below this threshold, raw pixels are sent instead of H.264 encoding.
/// 65536 pixels ≈ 256×256 rect ≈ 256 KB of raw BGRA data.
const UNCOMPRESSED_MAX_PIXELS: u32 = 65536;

/// Display adapter that bridges ScreenCapturer to ironrdp-server
pub struct MacDisplay {
    width: u16,
    height: u16,
    /// Maximum resolution (auto-detected or configured). Client cannot exceed this.
    max_width: u16,
    max_height: u16,
    /// Whether resolution is fixed by config (true) or follows client (false)
    fixed_resolution: bool,
    frame_rate: u32,
    quality: Quality,
    encoder_pref: macrdp_encode::EncoderPreference,
    /// Whether AVC444 mode is requested by config
    mode_444: bool,
    show_cursor: bool,
    /// Send the cursor as RDP pointer PDUs (client-side rendering) instead of compositing it
    /// into the captured frame.
    cursor_pdu: bool,
    /// Explicit bitrate (config or hot-update). When set it wins over the fps/size-derived value.
    bitrate_override: Option<u32>,
    base_bitrate: u32,
    gfx_state: Arc<Mutex<GfxState>>,
    coord_mapper: MouseCoordMapper,
    /// Shared audio sender slot — updated per connection by AudioFactory
    shared_audio_tx: Option<SharedAudioTx>,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
    /// Hot-updatable frame rate (written by the config task, read per connection)
    hot_fps: Arc<AtomicU32>,
}

impl MacDisplay {
    pub fn new(
        width: u16, height: u16,
        fixed_resolution: bool,
        frame_rate: u32, quality: Quality,
        encoder_pref: macrdp_encode::EncoderPreference,
        mode_444: bool,
        show_cursor: bool,
        cursor_pdu: bool,
        bitrate_override: Option<u32>,
        gfx_state: Arc<Mutex<GfxState>>,
        coord_mapper: MouseCoordMapper,
        shared_audio_tx: Option<SharedAudioTx>,
        perf_stats: Option<SharedPerfStats>,
        hot_fps: Arc<AtomicU32>,
    ) -> Self {
        let base_bitrate = bitrate_override
            .unwrap_or_else(|| macrdp_encode::screen_bitrate(width as u32, height as u32, frame_rate as f32, quality));
        tracing::info!(base_bitrate_mbps = base_bitrate as f64 / 1_000_000.0, "Base bitrate");
        // Upper bound = physical pixels (CG logical points × backing scale). `width`/`height`
        // may already be scaled in auto mode, so never derive the bound from them.
        let (max_w, max_h) = macrdp_capture::detect_cg_display_size().ok()
            .zip(macrdp_capture::detect_display_scale().ok())
            .map(|((cg_w, cg_h), scale)| (width.max((cg_w * scale).min(u16::MAX as u32) as u16), height.max((cg_h * scale).min(u16::MAX as u32) as u16)))
            .unwrap_or((width.max(3840), height.max(2160)));
        Self {
            width, height,
            max_width: max_w, max_height: max_h,
            fixed_resolution,
            frame_rate, quality, encoder_pref, mode_444, show_cursor, cursor_pdu, bitrate_override, base_bitrate, gfx_state,
            coord_mapper,
            shared_audio_tx,
            perf_stats,
            hot_fps,
        }
    }
}

impl MacDisplay {
    /// `bitrate_override` wins; otherwise derive from current size / fps / quality.
    fn recompute_bitrate(&mut self) {
        self.base_bitrate = self.bitrate_override.unwrap_or_else(|| macrdp_encode::screen_bitrate(
            self.width as u32, self.height as u32, self.frame_rate as f32, self.quality,
        ));
    }

    fn apply_resize(&mut self, width: u16, height: u16, source: &str) {
        let w = width.min(self.max_width);
        let h = height.min(self.max_height);
        if w > 0 && h > 0 && (w != self.width || h != self.height) {
            tracing::info!(
                old_w = self.width, old_h = self.height,
                new_w = w, new_h = h,
                source,
                "Applying resolution change"
            );
            self.width = w;
            self.height = h;
            self.recompute_bitrate();
            self.coord_mapper.update_rdp_size(w, h);
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize { width: self.width, height: self.height }
    }

    fn request_resize(&mut self, width: u16, height: u16) {
        if self.fixed_resolution {
            tracing::debug!("Ignoring resize request — resolution is fixed by config");
            return;
        }
        self.apply_resize(width, height, "client-requested");
    }

    fn set_size(&mut self, width: u16, height: u16) {
        self.apply_resize(width, height, "server-config");
        self.fixed_resolution = true;
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        // Apply hot-updated settings (written by the config task into GfxState / hot_fps).
        // fps first: an explicit target_bitrate must win over the fps-derived default.
        let hot_fps = self.hot_fps.load(Ordering::Relaxed);
        if hot_fps > 0 && hot_fps != self.frame_rate {
            self.frame_rate = hot_fps;
        }
        let max_qp;
        {
            let mut gs = self.gfx_state.lock().unwrap();
            max_qp = gs.max_qp.unwrap_or(macrdp_encode::DEFAULT_MAX_QP);
            if let Some(bps) = gs.target_bitrate {
                self.bitrate_override = Some(bps);
            }
            if let Some(show) = gs.show_cursor {
                self.show_cursor = show;
            }
            if let Some(pref) = gs.encoder_pref.as_deref() {
                self.encoder_pref = macrdp_encode::EncoderPreference::from_str_opt(Some(pref));
            }
            if let Some(mode) = gs.chroma_mode.as_deref() {
                self.mode_444 = mode == "avc444";
                gs.avc444_enabled = self.mode_444;
            }
        }
        self.recompute_bitrate();

        // Create the H.264 encoder first: the capture pixel format depends on what the encoder
        // that was *actually* built accepts (VT→OpenH264 fallback must never be fed NV12).
        let enc_params = EncoderConfig {
            width: self.width as u32,
            height: self.height as u32,
            fps: self.frame_rate as f32,
            quality: self.quality,
            pref: self.encoder_pref,
            mode_444: self.mode_444,
            bitrate: self.base_bitrate,
            max_qp,
        };
        let encoder = build_encoder(enc_params);
        if encoder.is_some() {
            tracing::info!("H.264 encoder available — will use GFX path when client supports it");
        }
        let zero_copy = encoder.as_ref().is_some_and(|e| e.supports_pixel_buffer()) && !self.mode_444;

        let capture_config = CaptureConfig {
            width: self.width as u32,
            height: self.height as u32,
            frame_rate: self.frame_rate,
            pixel_format: if zero_copy {
                CapturePixelFormat::nv12(macrdp_encode::LIMITED_RANGE)
            } else {
                CapturePixelFormat::Bgra
            },
            show_cursor: self.show_cursor && !self.cursor_pdu,
        };
        // Read the current audio sender from the shared slot (set by AudioFactory per connection)
        let audio_tx = self.shared_audio_tx.as_ref()
            .and_then(|shared| shared.lock().unwrap().clone());
        let capturer = ScreenCapturer::new(capture_config.clone(), audio_tx).await?;

        let (peer_addr, max_pointer) = {
            let gfx = self.gfx_state.lock().unwrap();
            (gfx.peer_addr, gfx.max_pointer_size as usize)
        };
        let network = peer_addr
            .map(|ip| NetworkQuality::from_ip(is_private_ip(ip)))
            .unwrap_or(NetworkQuality::from_ip(false));
        let initial_fps = capture_config.frame_rate;
        let bitrate_ctrl = BitrateController::new(self.base_bitrate, initial_fps, network);

        let cursor = (self.show_cursor && self.cursor_pdu)
            .then(|| CursorWatcher::spawn(self.coord_mapper.scale(), max_pointer));

        Ok(Box::new(MacDisplayUpdates {
            capturer,
            cursor,
            capture_config,
            encoder,
            enc_params,
            gfx_state: Arc::clone(&self.gfx_state),
            mode_444: self.mode_444,
            display_frame_count: 0,
            skip_next_frame: false,
            overload_count: 0,
            idle_frame_count: 0,
            last_idr_time: Instant::now(),
            last_frame_cache: None,
            bitrate_ctrl,
            last_applied_fps: initial_fps,
            perf_stats: self.perf_stats.clone(),
            session_start: Instant::now(),
            bitmap_fallback: false,
            keyframe_requested: true,
            ack_skip_streak: 0,
            last_full_uncompressed: None,
            last_sck_retry: None,
            shared_audio_tx: self.shared_audio_tx.clone(),
            suppressed: false,
            cache_unsent: false,
            encode_fail_streak: 0,
            last_set_bitrate: self.base_bitrate,
        }))
    }
}

/// `create_encoder` with the failure logged — a missing encoder means the bitmap/uncompressed path.
fn build_encoder(cfg: EncoderConfig) -> Option<Box<dyn VideoEncoder>> {
    macrdp_encode::create_encoder(cfg)
        .map_err(|e| tracing::error!("H.264 encoder creation failed: {e}"))
        .ok()
}

/// `set_frame_rate_async` bounded by `SET_FPS_TIMEOUT`: on a stopped SCStream (CG fallback)
/// `update_configuration` may never call back; the blocking-pool thread stays stuck, the
/// display loop must not.
async fn set_fps_with_timeout(capturer: &ScreenCapturer, fps: u32) -> Result<()> {
    match tokio::time::timeout(SET_FPS_TIMEOUT, capturer.set_frame_rate_async(fps)).await {
        Ok(r) => r,
        Err(_) => anyhow::bail!("SCStream frame rate update timed out after {:?}", SET_FPS_TIMEOUT),
    }
}

/// Cached frame data for idle IDR re-encoding (also the owned encoder input for the blocking pool).
enum CachedFrame {
    Bgra { data: Bytes, width: u32, height: u32, stride: usize },
    PixelBuffer(SafePixelBuffer, u32, u32), // buffer, width, height
}

impl CachedFrame {
    fn from_frame(frame: &CapturedFrame) -> Self {
        match &frame.data {
            FrameData::PixelBuffer(buf) => CachedFrame::PixelBuffer(buf.clone_ref(), frame.width, frame.height),
            FrameData::Raw(bgra) => CachedFrame::Bgra {
                data: bgra.clone(), width: frame.width, height: frame.height, stride: frame.stride,
            },
        }
    }

    /// Cheap owned copy: Bytes is refcounted, PixelBuffer gets its own retain.
    fn clone_ref(&self) -> Self {
        match self {
            CachedFrame::Bgra { data, width, height, stride } => {
                CachedFrame::Bgra { data: data.clone(), width: *width, height: *height, stride: *stride }
            }
            CachedFrame::PixelBuffer(buf, w, h) => CachedFrame::PixelBuffer(buf.clone_ref(), *w, *h),
        }
    }

    /// AVC420 encode of this frame (synchronous; zero-copy for PixelBuffer).
    fn encode(&self, enc: &mut dyn VideoEncoder, force_keyframe: bool) -> Result<macrdp_encode::EncodedFrame> {
        if force_keyframe {
            enc.force_keyframe();
        }
        match self {
            CachedFrame::Bgra { data, width, height, stride } => enc.encode_bgra(data, *width, *height, *stride),
            CachedFrame::PixelBuffer(buf, ..) => enc.encode_pixel_buffer(buf.as_ptr(), false),
        }
    }
}

/// IDR keepalive interval during idle scenes. GFX frame ACKs already guarantee delivery;
/// this is only a recovery backstop after a long static period, so keep it rare.
const IDLE_IDR_INTERVAL: Duration = Duration::from_secs(10);
/// Minimum interval between SCStream rebuild attempts while on the CoreGraphics fallback
/// (each attempt runs SCShareableContent::get, which is expensive and fails for the whole
/// lock-screen period).
const SCK_RETRY_INTERVAL: Duration = Duration::from_secs(1);

struct MacDisplayUpdates {
    capturer: ScreenCapturer,
    /// Client-side cursor: pointer PDU on shape change (None = cursor is in the video frame).
    cursor: Option<CursorWatcher>,
    capture_config: CaptureConfig,
    encoder: Option<Box<dyn VideoEncoder>>,
    /// Parameters the encoder was built with — used to rebuild it after a panic/cancel.
    enc_params: EncoderConfig,
    gfx_state: Arc<Mutex<GfxState>>,
    mode_444: bool,
    display_frame_count: u64,
    skip_next_frame: bool,
    /// Counter for rate-limiting encode overload warnings
    overload_count: u64,
    /// Consecutive idle events from SCK (desktop unchanged).
    idle_frame_count: u32,
    /// When the last IDR keyframe was sent (natural or keepalive).
    last_idr_time: Instant,
    /// Cached last frame for idle IDR re-encoding.
    last_frame_cache: Option<CachedFrame>,
    /// Adaptive bitrate/fps controller.
    bitrate_ctrl: BitrateController,
    /// Last fps value applied to the capturer (to avoid redundant updates).
    last_applied_fps: u32,
    /// Shared performance statistics collector (None = disabled)
    perf_stats: Option<SharedPerfStats>,
    /// When this update stream was created (GFX negotiation grace period).
    session_start: Instant,
    /// Latched once GFX failed to negotiate within GFX_NEGOTIATE_TIMEOUT — legacy bitmap
    /// updates for the rest of this stream (never mix bitmap and GFX in one session).
    bitmap_fallback: bool,
    /// Next encoded frame must be a keyframe (first frame / idle recovery) — never skipped.
    keyframe_requested: bool,
    /// Consecutive frames skipped because of ACK backlog.
    ack_skip_streak: u32,
    /// When the last whole-frame uncompressed update was sent (no-AVC420 client throttle).
    last_full_uncompressed: Option<Instant>,
    /// When SCStream was last (unsuccessfully) rebuilt on the CoreGraphics fallback path.
    last_sck_retry: Option<Instant>,
    /// Audio sender slot, re-read on every capturer rebuild so audio survives SCStream restarts.
    shared_audio_tx: Option<SharedAudioTx>,
    /// Mirror of `GfxState::output_suppressed` as last seen by this stream (edge detection).
    suppressed: bool,
    /// `last_frame_cache` holds a frame the client never received (skipped for overload / ACK
    /// backlog / throttle / GFX still negotiating). Flushed by `flush_unsent_cache` once the
    /// desktop is idle or the wait times out — SCK never resends an unchanged frame.
    cache_unsent: bool,
    /// Consecutive `Err` results from the encoder; `ENCODE_FAIL_REBUILD` of them rebuild it.
    encode_fail_streak: u32,
    /// Bitrate last applied to the encoder, so identical values do not hit `set_bitrate`.
    last_set_bitrate: u32,
}

/// How long to wait for the client to open and negotiate the GFX channel before falling
/// back to legacy bitmap updates (clients without GFX support would otherwise see black).
const GFX_NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(3);
/// Skip encoding when more than this many GFX frames await client ACK. RTT-adaptive: with a
/// fixed 3 the frame rate is capped at 3/RTT (≈ 30 fps at 100 ms), so allow about one RTT
/// worth of frames in flight, bounded to [3, 8]. Unknown RTT → 3.
fn max_pending_acks(rtt_ms: f64, fps: u32) -> u32 {
    if rtt_ms <= 0.0 {
        return 3;
    }
    let frame_interval_ms = 1000.0 / fps.max(1) as f64;
    ((rtt_ms / frame_interval_ms).ceil() as u32 + 1).clamp(3, 8)
}
/// Upper bound on consecutive ACK-backlog skips, so a client that suspends frame
/// acknowledgements (SUSPEND_FRAME_ACKNOWLEDGEMENT) cannot stall the stream.
const MAX_ACK_SKIP_STREAK: u32 = 10;
/// Retry interval for flushing an unsent cached frame while SCK delivers no event
/// (GFX negotiating, uncompressed throttle).
const CACHE_FLUSH_RETRY: Duration = Duration::from_millis(100);
/// Consecutive encoder `Err`s after which the encoder is rebuilt.
const ENCODE_FAIL_REBUILD: u32 = 3;
/// Upper bound for one `SCStream::update_configuration` call (unbounded condvar wait inside
/// SCK; a stopped stream may never invoke the completion handler).
const SET_FPS_TIMEOUT: Duration = Duration::from_secs(2);
/// Minimum interval between whole-frame uncompressed updates (clients without AVC420).
/// ponytail: hard 10 fps cap — a 1080p raw frame is ~8 MB, so this bounds the no-AVC path at
/// ~80 MB/s. Upgrade path: RemoteFX / Planar codec instead of raw BGRA.
const FULL_UNCOMPRESSED_MIN_INTERVAL: Duration = Duration::from_millis(100);

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        // Drain stale frames — always use the latest available frame.
        // If SCK capturer stops (e.g. screen locked), fall back to CGDisplayCreateImage
        // which works at the display level (including lock screen).
        // Never returns `Ok(None)` — the server treats that as disconnect; a skipped frame
        // (`encode_and_send` → `None`) just loops for the next one.
        loop {
            // Suppress Output edge detection (SCK delivers Idle events even when the desktop is
            // static, so this runs at least once per capture interval — ≤ 1 s while suppressed).
            self.sync_suppressed().await;
            self.switch_to_gfx_if_negotiated();
            // Calculate timeout for idle IDR keepalive.
            // If we have a cached frame and are in idle state, wait at most until
            // the next IDR keepalive is due instead of blocking forever.
            // Also the path that repaints the client right after suppression ends.
            let idle_deadline = if self.idle_frame_count > 0 && self.last_frame_cache.is_some() && !self.suppressed {
                let elapsed = self.last_idr_time.elapsed();
                if elapsed >= IDLE_IDR_INTERVAL {
                    // IDR keepalive is overdue — send it immediately
                    if let Some(update) = self.send_idle_idr(true).await? {
                        return Ok(Some(update));
                    }
                    // Nothing to send (no encoder / GFX not AVC-capable): re-arm so we
                    // retry on the next interval instead of blocking forever.
                    self.last_idr_time = Instant::now();
                    Some(IDLE_IDR_INTERVAL)
                } else {
                    Some(IDLE_IDR_INTERVAL - elapsed)
                }
            } else {
                None
            };
            // An unsent cached frame must go out even if SCK stays silent: poll the flush.
            let idle_deadline = if self.cache_unsent && !self.suppressed {
                Some(idle_deadline.map_or(CACHE_FLUSH_RETRY, |d| d.min(CACHE_FLUSH_RETRY)))
            } else {
                idle_deadline
            };
            // Cursor shape changes preempt frame waiting (mpsc recv on both sides is cancel-safe).
            let (cur, cap) = (&mut self.cursor, &mut self.capturer);
            let cursor_changed = async {
                match cur {
                    Some(c) => c.changed().await,
                    None => std::future::pending().await,
                }
            };
            let event = if let Some(timeout) = idle_deadline {
                let waited = tokio::select! {
                    biased;
                    p = cursor_changed => Err(p),
                    r = tokio::time::timeout(timeout, cap.next_frame()) => Ok(r),
                };
                match waited {
                    Err(Some(ptr)) => return Ok(Some(DisplayUpdate::RGBAPointer(ptr))),
                    Err(None) => { self.cursor = None; continue; }
                    Ok(Ok(Some(e))) => e,
                    Ok(Ok(None)) => {
                        if let Some(update) = self.cg_fallback_update().await? {
                            return Ok(Some(update));
                        }
                        continue;
                    }
                    Ok(Err(_elapsed)) => {
                        // Timeout — nothing newer is coming: flush an unsent frame first, else
                        // idle IDR keepalive (only once actually due — the flush retry fires earlier).
                        if self.cache_unsent {
                            if let Some(update) = self.flush_unsent_cache().await? {
                                return Ok(Some(update));
                            }
                        } else if let Some(update) = self.send_idle_idr(true).await? {
                            return Ok(Some(update));
                        }
                        // No encoder/GFX available — re-armed by the deadline logic above
                        continue;
                    }
                }
            } else {
                let waited = tokio::select! {
                    biased;
                    p = cursor_changed => Err(p),
                    r = cap.next_frame() => Ok(r),
                };
                match waited {
                    Err(Some(ptr)) => return Ok(Some(DisplayUpdate::RGBAPointer(ptr))),
                    Err(None) => { self.cursor = None; continue; }
                    Ok(Some(e)) => e,
                    Ok(None) => {
                        if let Some(update) = self.cg_fallback_update().await? {
                            return Ok(Some(update));
                        }
                        continue;
                    }
                }
            };

            // Handle idle events (desktop unchanged)
            let frame = match event {
                macrdp_capture::CaptureEvent::Frame(f) => {
                    // Desktop became active — reset idle counter
                    if self.idle_frame_count > 0 {
                        // No forced IDR here: TCP is reliable and the reference chain is intact;
                        // an IDR after every typing pause is 5-20× a P-frame (latency spike, high QP).
                        tracing::debug!(
                            idle_frames = self.idle_frame_count,
                            "Desktop active again — restoring bitrate/fps"
                        );

                        // Reset BitrateController and restore initial bitrate/fps
                        self.bitrate_ctrl.on_idle_recovery();
                        let bps = self.bitrate_ctrl.current_bitrate();
                        self.set_encoder_bitrate(bps);
                        let target_fps = self.bitrate_ctrl.target_fps();
                        if self.last_applied_fps != target_fps {
                            // While suppressed the capturer stays at 1 fps; the restore edge
                            // in `sync_suppressed` applies `last_applied_fps`.
                            if !self.suppressed {
                                let _ = set_fps_with_timeout(&self.capturer, target_fps).await;
                            }
                            self.last_applied_fps = target_fps;
                        }
                    }
                    self.idle_frame_count = 0;
                    f
                }
                macrdp_capture::CaptureEvent::Idle => {
                    self.idle_frame_count += 1;
                    // Desktop idle: a skipped frame was the last of the burst — send it now.
                    if self.cache_unsent {
                        if let Some(update) = self.flush_unsent_cache().await? {
                            return Ok(Some(update));
                        }
                    // First idle after activity: SCK only delivers frames on change, so the last
                    // frame of a scroll/typing burst (high QP under low-latency RC) would stay
                    // blurry forever. Re-encode it once as a P-frame so the encoder refines it.
                    // Not while a keyframe is pending (stale surface) or in AVC444 (the single
                    // VT session alternates main/aux; a 420 P-frame would reference the aux view).
                    } else if self.idle_frame_count == 1 && !self.keyframe_requested && !self.mode_444 {
                        if let Some(update) = self.send_idle_idr(false).await? {
                            return Ok(Some(update));
                        }
                    }
                    continue;
                }
            };

            // Drain anything already buffered and keep only the newest frame — queued
            // frames add latency, but the newest one must never be dropped.
            let mut frame = frame;
            while let Some(macrdp_capture::CaptureEvent::Frame(newer)) = self.capturer.try_next_frame() {
                frame = newer;
            }
            if let Some(update) = self.encode_and_send(frame).await? {
                return Ok(Some(update));
            }
        }
    }
}

impl MacDisplayUpdates {
    /// Mirror `GfxState::output_suppressed` and act on the edges: suppressed → capture at
    /// 1 fps and stop encoding (frames are still cached); restored → previous fps and an
    /// immediate IDR from the cache (idle-IDR path) plus a keyframe for the next live frame.
    async fn sync_suppressed(&mut self) {
        let suppressed = self.gfx_state.lock().unwrap().output_suppressed;
        if suppressed == self.suppressed {
            return;
        }
        self.suppressed = suppressed;
        if suppressed {
            tracing::info!("Client suppressed output — encoding paused, capture at 1 fps");
            if let Err(e) = set_fps_with_timeout(&self.capturer, 1).await {
                tracing::warn!("Failed to lower frame rate for suppressed output: {e}");
            }
        } else {
            tracing::info!(fps = self.last_applied_fps, "Client restored output — resuming with keyframe");
            if let Err(e) = set_fps_with_timeout(&self.capturer, self.last_applied_fps).await {
                tracing::warn!("Failed to restore frame rate after suppressed output: {e}");
            }
            self.keyframe_requested = true;
            if self.last_frame_cache.is_some() {
                self.idle_frame_count = self.idle_frame_count.max(1);
                self.last_idr_time = Instant::now().checked_sub(IDLE_IDR_INTERVAL).unwrap_or(self.session_start);
            }
        }
    }

    /// New SCStream with the current config and the *current* audio sender (the slot is
    /// replaced per connection by AudioFactory; passing `None` would silence audio after a
    /// rebuild).
    /// The new stream starts at `capture_config.frame_rate`; re-apply the live rate
    /// (1 fps while suppressed, else the adaptive `last_applied_fps`).
    async fn new_capturer(&mut self) -> Result<ScreenCapturer> {
        let audio_tx = self.shared_audio_tx.as_ref().and_then(|shared| shared.lock().unwrap().clone());
        let capturer = ScreenCapturer::new(self.capture_config.clone(), audio_tx).await?;
        let fps = if self.suppressed { 1 } else { self.last_applied_fps };
        if fps != self.capture_config.frame_rate {
            if let Err(e) = set_fps_with_timeout(&capturer, fps).await {
                tracing::warn!(fps, "Failed to re-apply frame rate on rebuilt capturer: {e}");
            }
        }
        Ok(capturer)
    }

    /// Late GFX negotiation: the DVC side still completes caps exchange after our bitmap
    /// fallback latched, and a client in GFX mode ignores legacy bitmaps (frozen screen).
    /// Switch over with a keyframe; on a static desktop the cached frame is the only
    /// source for it, so mark it unsent and let `flush_unsent_cache` repaint.
    fn switch_to_gfx_if_negotiated(&mut self) {
        if !self.bitmap_fallback || !self.gfx_state.lock().unwrap().is_ready() {
            return;
        }
        tracing::info!("GFX channel negotiated after bitmap fallback — switching to GFX");
        self.bitmap_fallback = false;
        self.keyframe_requested = true;
        self.display_frame_count = 0;
        if let Some(enc) = &mut self.encoder {
            enc.force_keyframe();
        }
        self.cache_unsent = self.last_frame_cache.is_some();
    }

    /// Send the cached frame the client never received (see `cache_unsent`): H.264 IDR / P
    /// via `send_idle_idr` when the client takes AVC420, otherwise the whole frame uncompressed
    /// (throttled; retried on the next idle event / `CACHE_FLUSH_RETRY`). `None` = not possible yet.
    async fn flush_unsent_cache(&mut self) -> Result<Option<DisplayUpdate>> {
        if !self.cache_unsent || self.suppressed || self.bitmap_fallback {
            return Ok(None);
        }
        let (ready, avc420) = {
            let gs = self.gfx_state.lock().unwrap();
            (gs.is_ready(), gs.avc420_supported())
        };
        if !ready {
            // GFX never negotiated and the desktop is static (SCK sends nothing more, the cursor
            // is not captured): replay the cached frame so `encode_and_send` latches the bitmap
            // fallback and paints it, instead of leaving the client black until something moves.
            if self.encoder.is_none() || self.session_start.elapsed() >= GFX_NEGOTIATE_TIMEOUT {
                if let Some(frame) = self.cached_bgra_frame() {
                    return self.encode_and_send(frame).await;
                }
            }
            return Ok(None);
        }
        if avc420 && self.encoder.is_some() {
            // AVC444: a 420 P-frame would reference the aux view — always an IDR there.
            return self.send_idle_idr(self.keyframe_requested || self.mode_444).await;
        }
        let Some(frame) = self.cached_bgra_frame() else {
            // NV12 cache on a client without H.264: `ensure_capture_format` already switched
            // capture to BGRA, the next frame goes out whole.
            self.cache_unsent = false;
            return Ok(None);
        };
        Ok(self.try_uncompressed(&frame, true))
    }

    /// The cached frame as a whole-frame BGRA `CapturedFrame` (`None` for an NV12 cache).
    fn cached_bgra_frame(&self) -> Option<CapturedFrame> {
        let Some(CachedFrame::Bgra { data, width, height, stride }) = &self.last_frame_cache else { return None };
        Some(CapturedFrame {
            width: *width, height: *height, data: FrameData::Raw(data.clone()), stride: *stride,
            timestamp_us: 0, dirty_rects: Vec::new(),
        })
    }

    /// `set_bitrate` only when the value changed (VT/OpenH264 property writes are not free).
    fn set_encoder_bitrate(&mut self, bps: u32) {
        if bps == self.last_set_bitrate {
            return;
        }
        if let Some(enc) = &mut self.encoder {
            enc.set_bitrate(bps);
            self.last_set_bitrate = bps;
        }
    }

    /// SCStream stopped (lock screen?) — serve one CoreGraphics frame, retrying the SCStream
    /// rebuild at most every `SCK_RETRY_INTERVAL`. Returns `None` once SCK has recovered or
    /// the CG frame produced no output (caller loops), `Some(update)` for a CG frame.
    async fn cg_fallback_update(&mut self) -> Result<Option<DisplayUpdate>> {
        self.idle_frame_count = 0;
        let fallback = CgFallbackCapturer::new(&self.capture_config);
        loop {
            if self.last_sck_retry.is_none_or(|t| t.elapsed() >= SCK_RETRY_INTERVAL) {
                tracing::warn!("SCStream stopped — switching to CoreGraphics fallback (lock screen?)");
                self.last_sck_retry = Some(Instant::now());
                if let Ok(new_capturer) = self.new_capturer().await {
                    tracing::info!("SCStream recovered — switching back from CoreGraphics");
                    self.capturer = new_capturer;
                    self.last_sck_retry = None;
                    return Ok(None);
                }
            }
            // Throttle to the configured fps on both the success and the failure path.
            tokio::time::sleep(fallback.frame_interval()).await;
            if let Some(cg_frame) = fallback.capture_frame() {
                return self.encode_and_send(cg_frame).await;
            }
        }
    }

    /// Run a blocking encoder call on the blocking pool so the single-threaded server
    /// runtime keeps handling input/ACKs. The encoder is *always* restored afterwards: on a
    /// panic inside `f` or a cancelled task it is rebuilt from `enc_params` (and the next
    /// frame is forced to a keyframe) instead of being lost for the rest of the session.
    async fn with_encoder_blocking<T: Send + 'static>(
        &mut self,
        f: impl FnOnce(&mut dyn VideoEncoder) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let Some(mut enc) = self.encoder.take() else { anyhow::bail!("no encoder") };
        let joined = tokio::task::spawn_blocking(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(enc.as_mut())));
            (enc, r)
        })
        .await;
        let err = match joined {
            Ok((enc, Ok(r))) => {
                self.encoder = Some(enc);
                match &r {
                    Ok(_) => self.encode_fail_streak = 0,
                    Err(_) => {
                        self.encode_fail_streak += 1;
                        if self.encode_fail_streak >= ENCODE_FAIL_REBUILD {
                            tracing::error!(streak = self.encode_fail_streak, "encoder keeps failing — rebuilding H.264 encoder");
                            self.encode_fail_streak = 0;
                            self.rebuild_encoder().await;
                        }
                    }
                }
                return r;
            }
            Ok((_, Err(_))) => anyhow::anyhow!("encoder panicked"),
            Err(join) => anyhow::anyhow!("encoder task cancelled: {join}"),
        };
        tracing::error!("{err} — rebuilding H.264 encoder");
        self.rebuild_encoder().await;
        Err(err)
    }

    /// Rebuild the encoder from `enc_params`. If the rebuilt encoder accepts a different capture
    /// pixel format (VT→OpenH264 fallback: NV12 `PixelBuffer` frames would be dropped forever on
    /// the synchronous path), switch `capture_config.pixel_format` and rebuild the capturer too;
    /// if that fails, close the old stream so the CG fallback loop retries with the new config.
    async fn rebuild_encoder(&mut self) {
        self.encoder = build_encoder(self.enc_params);
        self.keyframe_requested = true;
        self.last_set_bitrate = self.enc_params.bitrate;

        let zero_copy = self.encoder.as_ref().is_some_and(|e| e.supports_pixel_buffer()) && !self.mode_444;
        self.ensure_capture_format(zero_copy).await;
    }

    /// Switch capture between NV12 zero-copy and BGRA (also for clients that take no H.264 at
    /// all: the uncompressed / bitmap paths need BGRA, NV12 frames would be dropped forever).
    /// An NV12 cache is useless to those consumers and to OpenH264, so it is cleared.
    async fn ensure_capture_format(&mut self, want_nv12: bool) {
        let want = if want_nv12 {
            CapturePixelFormat::nv12(macrdp_encode::LIMITED_RANGE)
        } else {
            CapturePixelFormat::Bgra
        };
        if want == self.capture_config.pixel_format {
            return;
        }
        tracing::warn!(
            from = ?self.capture_config.pixel_format, to = ?want,
            "capture pixel format no longer matches the consumer — rebuilding capturer"
        );
        self.capture_config.pixel_format = want;
        if matches!(self.last_frame_cache, Some(CachedFrame::PixelBuffer(..))) {
            self.last_frame_cache = None;
            self.cache_unsent = false;
        }
        match self.new_capturer().await {
            Ok(c) => self.capturer = c,
            Err(e) => {
                tracing::error!("capturer rebuild failed ({e:#}) — closing stream, CG fallback will retry");
                self.capturer.close();
            }
        }
    }

    /// Size of the cached last frame.
    fn cached_frame_size(&self) -> (u16, u16) {
        match &self.last_frame_cache {
            Some(CachedFrame::Bgra { width, height, .. }) => (*width as u16, *height as u16),
            Some(CachedFrame::PixelBuffer(_, w, h)) => (*w as u16, *h as u16),
            None => (self.capture_config.width as u16, self.capture_config.height as u16),
        }
    }

    /// Re-encode the cached last frame: as an IDR keyframe (`force_kf`, idle keepalive / resume)
    /// or as a refinement P-frame (first idle after activity). The cache itself is unchanged.
    /// Returns `None` if no cached frame or the client cannot take H.264 over GFX.
    async fn send_idle_idr(&mut self, force_kf: bool) -> Result<Option<DisplayUpdate>> {
        // `bitmap_fallback` covers the window where GFX negotiated late but the stream has not
        // yet switched over (next live frame does) — a refinement P-frame there has no IDR to reference.
        if self.suppressed || self.bitmap_fallback || !self.gfx_state.lock().unwrap().avc420_supported() {
            return Ok(None);
        }
        let Some(cache) = self.last_frame_cache.as_ref() else { return Ok(None) };
        if self.encoder.is_none() {
            return Ok(None);
        }
        let (vis_w, vis_h) = self.cached_frame_size();
        // Owned copy for the blocking pool: Bytes is refcounted, PixelBuffer gets its own retain.
        let input = cache.clone_ref();

        let t0 = Instant::now();
        let result = self
            .with_encoder_blocking(move |enc| input.encode(enc, force_kf))
            .await;

        match result {
            Ok(encoded) if !encoded.data.is_empty() => {
                let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    h264_bytes = encoded.data.len(),
                    is_keyframe = encoded.is_keyframe,
                    encode_ms = format!("{:.1}", encode_ms),
                    idle_frames = self.idle_frame_count,
                    "{}", if force_kf { "Sent idle IDR keepalive" } else { "Sent idle refinement P-frame" }
                );
                if encoded.is_keyframe {
                    self.last_idr_time = Instant::now();
                    // This IDR already repaints the whole surface — no second one on the next live frame.
                    self.keyframe_requested = false;
                }
                self.cache_unsent = false;
                Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                    h264_data: encoded.data,
                    width: vis_w,
                    height: vis_h,
                    enc_width: encoded.width as u16,
                    enc_height: encoded.height as u16,
                    is_keyframe: encoded.is_keyframe,
                    avc444: false,
                    h264_aux: None,
                })))
            }
            Ok(_) => {
                macrdp_encode::warn_limited!(force_kf, "Idle re-encode returned empty data");
                Ok(None)
            }
            Err(e) => {
                macrdp_encode::warn_limited!(force_kf, "Idle re-encode failed: {e}");
                Ok(None)
            }
        }
    }

    /// Evaluate the BitrateController window and apply any adaptive bitrate/fps changes.
    async fn apply_adaptive_decision(&mut self) {
        if !self.bitrate_ctrl.should_evaluate() {
            return;
        }

        // Phase 2 RTT check
        {
            let gfx = self.gfx_state.lock().unwrap();
            if gfx.rtt_ewma_ms > 0.0 {
                self.bitrate_ctrl.update_network_rtt(gfx.rtt_ewma_ms);
                if let Some(ps) = &self.perf_stats {
                    ps.lock().unwrap().record_rtt(gfx.rtt_ewma_ms);
                }
            }
        }

        let decision = self.bitrate_ctrl.evaluate();
        self.set_encoder_bitrate(decision.bitrate_bps);

        if decision.fps != self.last_applied_fps {
            if let Err(e) = set_fps_with_timeout(&self.capturer, decision.fps).await {
                tracing::warn!("Failed to update frame rate: {e}");
            } else {
                self.last_applied_fps = decision.fps;
                tracing::info!(
                    fps = decision.fps,
                    bitrate_mbps = decision.bitrate_bps as f64 / 1_000_000.0,
                    "Adaptive: fps/bitrate updated"
                );
            }
        }
    }

    /// GFX uncompressed path: send raw BGRA rects instead of H.264 (zero encode latency).
    ///
    /// `full = false`: only when the total dirty area is small (≤ UNCOMPRESSED_MAX_PIXELS).
    /// `full = true` (client negotiated GFX without AVC420): no area cap; the whole frame is
    /// sent when dirty-rect info is unavailable or this is the first frame of the stream.
    /// Returns `None` for PixelBuffer frames, no/too-large dirty rects, extraction failure, or
    /// a throttled whole frame (`full` only — nothing to send this round).
    fn try_uncompressed(&mut self, frame: &CapturedFrame, full: bool) -> Option<DisplayUpdate> {
        let whole = macrdp_capture::Rect { x: 0, y: 0, width: frame.width, height: frame.height };
        let whole_frame = full && (frame.dirty_rects.is_empty() || self.display_frame_count == 0 || self.keyframe_requested);
        if whole_frame {
            if self.last_full_uncompressed.is_some_and(|t| t.elapsed() < FULL_UNCOMPRESSED_MIN_INTERVAL) {
                // Throttled: keep it so `flush_unsent_cache` sends it once the interval passed.
                self.last_frame_cache = Some(CachedFrame::from_frame(frame));
                self.cache_unsent = true;
                return None;
            }
            self.last_full_uncompressed = Some(Instant::now());
            self.keyframe_requested = false;
        }
        let dirty_rects: &[macrdp_capture::Rect] = if whole_frame {
            std::slice::from_ref(&whole)
        } else {
            &frame.dirty_rects
        };
        if dirty_rects.is_empty() {
            return None;
        }
        let total_area: u32 = dirty_rects.iter()
            .map(|r| r.width * r.height)
            .sum();
        if total_area == 0 || (!full && total_area > UNCOMPRESSED_MAX_PIXELS) {
            return None;
        }
        let FrameData::Raw(bgra) = &frame.data else { return None };

        let rects: Vec<UncompressedRect> = dirty_rects.iter()
            .filter(|r| r.width > 0 && r.height > 0)
            .filter_map(|r| {
                // Clamp to frame bounds
                let x = r.x.min(frame.width.saturating_sub(1));
                let y = r.y.min(frame.height.saturating_sub(1));
                let w = r.width.min(frame.width - x);
                let h = r.height.min(frame.height - y);
                if w == 0 || h == 0 { return None; }

                let bpp = 4usize;
                let row_bytes = w as usize * bpp;
                let mut pixels = Vec::with_capacity(h as usize * row_bytes);
                for row in 0..h as usize {
                    let src_y = y as usize + row;
                    let offset = src_y * frame.stride + x as usize * bpp;
                    let end = offset + row_bytes;
                    if end <= bgra.len() {
                        pixels.extend_from_slice(&bgra[offset..end]);
                    }
                }
                Some(UncompressedRect {
                    x: x as u16,
                    y: y as u16,
                    width: w as u16,
                    height: h as u16,
                    pixel_data: Bytes::from(pixels),
                })
            })
            .collect();
        if rects.is_empty() {
            return None;
        }

        self.display_frame_count += 1;
        let total_bytes: usize = rects.iter().map(|r| r.pixel_data.len()).sum();
        {
            let mut st = self.gfx_state.lock().unwrap();
            st.last_encode_ms = 0.0; // zero encode time
            st.last_frame_bytes = total_bytes as u32;
        }
        // Keep the idle-IDR cache current so a keepalive IDR re-encodes this content,
        // not the frame from before these dirty rects.
        self.last_frame_cache = Some(CachedFrame::Bgra {
            data: bgra.clone(), width: frame.width, height: frame.height, stride: frame.stride,
        });
        self.cache_unsent = false;
        tracing::debug!(
            display_frame = self.display_frame_count,
            rects = rects.len(),
            total_pixels = total_area,
            total_bytes,
            "Display: sending GFX uncompressed dirty rects"
        );
        Some(DisplayUpdate::GfxUncompressed(GfxUncompressedUpdate {
            rects,
            width: frame.width as u16,
            height: frame.height as u16,
        }))
    }

    async fn encode_and_send(&mut self, frame: CapturedFrame) -> Result<Option<DisplayUpdate>> {
        // Client minimized (Suppress Output): keep the newest frame for the restore IDR, encode nothing.
        if self.suppressed {
            self.last_frame_cache = Some(CachedFrame::from_frame(&frame));
            return Ok(None);
        }
        // Encode overload protection: skip this frame if previous encode took too long
        if self.skip_next_frame {
            self.skip_next_frame = false;
            self.last_frame_cache = Some(CachedFrame::from_frame(&frame));
            self.cache_unsent = true;
            return Ok(None);
        }

        // AVC444 configured but this client negotiated without it: the 444 encoder session
        // (align16 size, 2× bitrate, BGRA capture + per-frame vImage conversion) buys nothing.
        // Rebuild as AVC420 once per stream — `rebuild_encoder` also switches capture to NV12
        // zero-copy when the new encoder takes pixel buffers. `mode_444` stays false afterwards.
        if self.mode_444 && !self.bitmap_fallback {
            let drop_444 = {
                let gs = self.gfx_state.lock().unwrap();
                gs.is_ready() && gs.avc420_supported() && !gs.avc444_supported()
            };
            if drop_444 {
                tracing::info!("Client has no AVC444 — switching this stream to AVC420 (zero-copy capture)");
                self.mode_444 = false;
                self.enc_params.mode_444 = false;
                self.rebuild_encoder().await;
            }
        }

        // GFX negotiated ≠ H.264 capable: `is_ready()` only means the channel is open and caps
        // confirmed; `GfxFrame` needs `avc420_supported()`, otherwise go uncompressed.
        let (gfx_ready, use_h264, use_444, pending_acks, max_pending) = {
            let state = self.gfx_state.lock().unwrap();
            (
                state.is_ready() && !self.bitmap_fallback,
                state.avc420_supported() && self.encoder.is_some() && !self.bitmap_fallback,
                self.mode_444 && state.avc444_supported() && state.avc444_enabled,
                state.pending_acks,
                max_pending_acks(state.rtt_ewma_ms, self.last_applied_fps),
            )
        };

        if gfx_ready && !use_h264 {
            // Whole frame over GFX uncompressed (no H.264 on this client / no encoder).
            // `None` = throttled or not BGRA → nothing to send for this frame.
            if !matches!(frame.data, FrameData::Raw(_)) {
                macrdp_encode::warn_limited!("GFX client without AVC420 and frame is not BGRA — dropping frame, switching capture to BGRA");
                self.ensure_capture_format(false).await;
                return Ok(None);
            }
            return Ok(self.try_uncompressed(&frame, true));
        }

        if use_h264 {
            // Flow control: skip encoding (not just sending) while the client is behind on
            // ACKs, so the H.264 reference chain stays intact. Keyframes are never skipped,
            // and a client that stopped ACKing cannot stall us past MAX_ACK_SKIP_STREAK.
            if pending_acks > max_pending
                && !self.keyframe_requested
                && self.ack_skip_streak < MAX_ACK_SKIP_STREAK
            {
                self.ack_skip_streak += 1;
                tracing::debug!(pending_acks, max_pending, streak = self.ack_skip_streak, "ACK backlog — skipping frame before encode");
                self.last_frame_cache = Some(CachedFrame::from_frame(&frame));
                self.cache_unsent = true;
                return Ok(None);
            }
            self.ack_skip_streak = 0;

            // GFX uncompressed path — for small dirty regions, skip H.264 encoding entirely.
            // Not while a keyframe is requested: the client surface may be stale (suppress
            // output / encoder rebuild), so partial rects must not land before a whole frame.
            if !self.keyframe_requested {
                if let Some(update) = self.try_uncompressed(&frame, false) {
                    return Ok(Some(update));
                }
            }
            // Taken here; re-armed below if the encode produces nothing, so the IDR is not lost.
            let force_kf = std::mem::replace(&mut self.keyframe_requested, false);

            // Synchronous encode on the blocking pool — no pipelining: a one-frame pipeline
            // costs a fixed 1/fps of latency and makes encode_ms measure the frame interval,
            // which the bitrate controller then reads as permanent overload.
            if self.encoder.is_some() {
                self.display_frame_count += 1;
                let cache = CachedFrame::from_frame(&frame);

                // AVC444 dual-stream path (BGRA only; CPU colour conversion + encode → blocking pool)
                if let (true, CachedFrame::Bgra { data, width, height, stride }) =
                    (use_444 && self.encoder.as_ref().is_some_and(|e| e.supports_444()), &cache)
                {
                    let (data, w, h, stride) = (data.clone(), *width, *height, *stride);
                    let t0 = Instant::now();
                    match self
                        .with_encoder_blocking(move |enc| {
                            if force_kf {
                                enc.force_keyframe();
                            }
                            enc.encode_bgra_444(&data, w, h, stride)
                        })
                        .await
                    {
                        Ok(encoded) if !encoded.main_view.data.is_empty() => {
                            let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                            let aux_bytes = encoded.aux_view.as_ref().map_or(0, |a| a.data.len());
                            let total_bytes = encoded.main_view.data.len() + aux_bytes;
                            let is_keyframe = encoded.main_view.is_keyframe;
                            tracing::debug!(
                                display_frame = self.display_frame_count,
                                main_bytes = encoded.main_view.data.len(),
                                aux_bytes,
                                aux_skipped = encoded.aux_view.is_none(),
                                is_keyframe,
                                encode_ms = format!("{:.1}", encode_ms),
                                "Display: sending AVC444 GFX frame"
                            );
                            {
                                let mut st = self.gfx_state.lock().unwrap();
                                st.last_encode_ms = encode_ms;
                                st.last_frame_bytes = total_bytes as u32;
                            }
                            self.bitrate_ctrl.record_frame(FrameStats {
                                encode_ms,
                                frame_bytes: total_bytes as u32,
                                is_keyframe,
                            });
                            if let Some(ps) = &self.perf_stats {
                                ps.lock().unwrap().record_frame(encode_ms, total_bytes as u32, is_keyframe);
                            }
                            self.apply_adaptive_decision().await;
                            let frame_interval_ms = 1000.0 / self.capture_config.frame_rate as f64;
                            if encode_ms > frame_interval_ms * 0.95 {
                                self.skip_next_frame = true;
                                self.overload_count += 1;
                                if self.overload_count % 60 == 1 {
                                    tracing::warn!(
                                        encode_ms = format!("{:.1}", encode_ms),
                                        frame_interval_ms = format!("{:.1}", frame_interval_ms),
                                        overload_total = self.overload_count,
                                        "encode overload — skipping frames"
                                    );
                                }
                            }
                            // Cache frame for idle IDR re-encoding
                            self.last_frame_cache = Some(cache);
                            self.cache_unsent = false;
                            if is_keyframe {
                                self.last_idr_time = Instant::now();
                            }
                            return Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                                h264_data: encoded.main_view.data,
                                width: frame.width as u16,
                                height: frame.height as u16,
                                enc_width: encoded.main_view.width as u16,
                                enc_height: encoded.main_view.height as u16,
                                is_keyframe,
                                avc444: true,
                                h264_aux: encoded.aux_view.map(|a| a.data),
                            })));
                        }
                        Ok(_) => {
                            macrdp_encode::warn_limited!(
                                display_frame = self.display_frame_count,
                                "AVC444 encode returned EMPTY data — frame dropped!"
                            );
                            self.keyframe_requested |= force_kf;
                            return Ok(None);
                        }
                        Err(e) => {
                            macrdp_encode::warn_limited!(display_frame = self.display_frame_count, "AVC444 encode failed: {e}, falling back to AVC420");
                            // Fall through to AVC420 path below
                        }
                    }
                }

                // AVC420 path (default or fallback from AVC444 failure): zero-copy CVPixelBuffer
                // for VT, BGRA otherwise. Runs on the blocking pool so the single-threaded server
                // runtime keeps handling input/ACKs meanwhile. encode_ms = wall time of this call.
                let input = cache.clone_ref();
                let t0 = Instant::now();
                let encoded = self.with_encoder_blocking(move |enc| input.encode(enc, force_kf)).await;
                match encoded {
                    Ok(encoded) if !encoded.data.is_empty() => {
                        let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                        let is_keyframe = encoded.is_keyframe;
                        tracing::debug!(
                            display_frame = self.display_frame_count,
                            h264_bytes = encoded.data.len(),
                            is_keyframe,
                            encode_ms = format!("{:.1}", encode_ms),
                            "Display: sending GFX frame"
                        );
                        {
                            let mut st = self.gfx_state.lock().unwrap();
                            st.last_encode_ms = encode_ms;
                            st.last_frame_bytes = encoded.data.len() as u32;
                        }
                        self.bitrate_ctrl.record_frame(FrameStats {
                            encode_ms,
                            frame_bytes: encoded.data.len() as u32,
                            is_keyframe,
                        });
                        if let Some(ps) = &self.perf_stats {
                            ps.lock().unwrap().record_frame(encode_ms, encoded.data.len() as u32, is_keyframe);
                        }
                        self.apply_adaptive_decision().await;
                        let frame_interval_ms = 1000.0 / self.capture_config.frame_rate as f64;
                        if encode_ms > frame_interval_ms * 0.95 {
                            self.skip_next_frame = true;
                            self.overload_count += 1;
                            if self.overload_count % 60 == 1 {
                                tracing::warn!(
                                    encode_ms = format!("{:.1}", encode_ms),
                                    frame_interval_ms = format!("{:.1}", frame_interval_ms),
                                    overload_total = self.overload_count,
                                    "encode overload — skipping frames"
                                );
                            }
                        }
                        // Cache frame for idle IDR re-encoding
                        self.last_frame_cache = Some(cache);
                        self.cache_unsent = false;
                        if is_keyframe {
                            self.last_idr_time = Instant::now();
                        }
                        return Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                            h264_data: encoded.data,
                            width: frame.width as u16,
                            height: frame.height as u16,
                            enc_width: encoded.width as u16,
                            enc_height: encoded.height as u16,
                            is_keyframe,
                            avc444: false,
                            h264_aux: None,
                        })));
                    }
                    Ok(_) => {
                        macrdp_encode::warn_limited!(
                            display_frame = self.display_frame_count,
                            "H.264 encode returned EMPTY data — frame dropped!"
                        );
                        self.keyframe_requested |= force_kf;
                        return Ok(None);
                    }
                    Err(e) => {
                        macrdp_encode::warn_limited!(display_frame = self.display_frame_count, "H.264 encode failed: {e}");
                        self.keyframe_requested |= force_kf;
                        return Ok(None);
                    }
                }
            }
        } else if self.encoder.is_some() && !self.bitmap_fallback {
            // H.264 encoder exists — give the client a grace period to open and negotiate
            // the GFX channel. Mixing bitmap and GFX in one session causes 0xd06
            // DECOMPRESSION_FAILED, so once we fall back we latch to bitmap for this stream.
            if self.session_start.elapsed() < GFX_NEGOTIATE_TIMEOUT {
                // Static desktop after connect: this may be the only frame SCK ever sends.
                self.last_frame_cache = Some(CachedFrame::from_frame(&frame));
                self.cache_unsent = true;
                return Ok(None);
            }
            tracing::warn!(
                timeout_secs = GFX_NEGOTIATE_TIMEOUT.as_secs(),
                "GFX channel not negotiated — falling back to legacy bitmap updates"
            );
            self.bitmap_fallback = true;
            // Legacy bitmaps need BGRA; an NV12 zero-copy capture would be dropped forever.
            self.ensure_capture_format(false).await;
        }

        // Bitmap path (only when GFX is not available at all)
        // Requires BGRA raw bytes — PixelBuffer frames should not reach here
        let bgra_bitmap = match &frame.data {
            FrameData::Raw(bytes) => bytes,
            FrameData::PixelBuffer(_) => {
                macrdp_encode::warn_limited!("PixelBuffer frame in bitmap path — switching capture to BGRA");
                self.ensure_capture_format(false).await;
                return Ok(None);
            }
        };
        // Cached (and sent) so a late GFX switch on a static desktop has a frame for its IDR.
        self.last_frame_cache = Some(CachedFrame::from_frame(&frame));
        self.cache_unsent = false;

        // A pending keyframe request means the client surface is stale → whole frame.
        if !frame.dirty_rects.is_empty() && !std::mem::take(&mut self.keyframe_requested) {
            // Find bounding box of all dirty rects to send a single update
            let mut min_x = frame.width;
            let mut min_y = frame.height;
            let mut max_x = 0u32;
            let mut max_y = 0u32;

            for r in &frame.dirty_rects {
                min_x = min_x.min(r.x);
                min_y = min_y.min(r.y);
                max_x = max_x.max(r.x + r.width);
                max_y = max_y.max(r.y + r.height);
            }

            // Clamp to frame bounds
            max_x = max_x.min(frame.width);
            max_y = max_y.min(frame.height);

            if max_x > min_x && max_y > min_y {
                let w = max_x - min_x;
                let h = max_y - min_y;
                let Some(width) = NonZeroU16::new(w as u16) else { return Ok(None) };
                let Some(height) = NonZeroU16::new(h as u16) else { return Ok(None) };

                // Extract only the dirty region from the full frame buffer
                let bpp = 4usize;
                let dirty_stride = w as usize * bpp;
                let mut dirty_data = Vec::with_capacity(dirty_stride * h as usize);
                for row in min_y..max_y {
                    let src_offset = row as usize * frame.stride + min_x as usize * bpp;
                    let src_end = src_offset + dirty_stride;
                    if src_end <= bgra_bitmap.len() {
                        dirty_data.extend_from_slice(&bgra_bitmap[src_offset..src_end]);
                    }
                }

                let Some(stride) = NonZeroUsize::new(dirty_stride) else { return Ok(None) };

                let update = BitmapUpdate {
                    x: min_x as u16,
                    y: min_y as u16,
                    width,
                    height,
                    format: RdpPixelFormat::BgrA32,
                    data: Bytes::from(dirty_data),
                    stride,
                };

                return Ok(Some(DisplayUpdate::Bitmap(update)));
            }
        }

        // No dirty rects available — send full frame (first frame or fallback)
        self.keyframe_requested = false;
        let Some(width) = NonZeroU16::new(frame.width as u16) else { return Ok(None) };
        let Some(height) = NonZeroU16::new(frame.height as u16) else { return Ok(None) };
        let Some(stride) = NonZeroUsize::new(frame.stride) else { return Ok(None) };

        let update = BitmapUpdate {
            x: 0,
            y: 0,
            width,
            height,
            format: RdpPixelFormat::BgrA32,
            data: bgra_bitmap.clone(),
            stride,
        };

        Ok(Some(DisplayUpdate::Bitmap(update)))
    }
}

#[cfg(test)]
mod tests {
    use super::max_pending_acks;

    #[test]
    fn ack_threshold_tracks_rtt() {
        assert_eq!(max_pending_acks(0.0, 60), 3); // RTT unknown
        assert_eq!(max_pending_acks(5.0, 60), 3); // LAN: floor
        assert_eq!(max_pending_acks(40.0, 60), 4); // ceil(40/16.7)=3, +1
        assert_eq!(max_pending_acks(100.0, 30), 4); // ceil(100/33.3)=3, +1
        assert_eq!(max_pending_acks(500.0, 60), 8); // ceiling
    }
}
