use anyhow::Result;
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, GfxFrameUpdate, GfxUncompressedUpdate,
    PixelFormat as RdpPixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
    UncompressedRect, gfx::GfxState,
};
use macrdp_audio::SharedAudioTx;
use macrdp_capture::{CaptureConfig, CapturePixelFormat, CapturedFrame, CgFallbackCapturer, FrameData, SafePixelBuffer, ScreenCapturer};
use macrdp_encode::{self, Quality, VideoEncoder};
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
        {
            let mut gs = self.gfx_state.lock().unwrap();
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
        let enc_params = EncoderParams {
            width: self.width as u32,
            height: self.height as u32,
            fps: self.frame_rate as f32,
            quality: self.quality,
            pref: self.encoder_pref,
            mode_444: self.mode_444,
            bitrate: self.base_bitrate,
        };
        let encoder = enc_params.create();
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

        let peer_addr = {
            let gfx = self.gfx_state.lock().unwrap();
            gfx.peer_addr
        };
        let network = peer_addr
            .map(|ip| NetworkQuality::from_ip(is_private_ip(ip)))
            .unwrap_or(NetworkQuality::from_ip(false));
        let initial_fps = capture_config.frame_rate;
        let bitrate_ctrl = BitrateController::new(self.base_bitrate, initial_fps, network);

        let cursor = (self.show_cursor && self.cursor_pdu)
            .then(|| CursorWatcher::spawn(self.coord_mapper.scale()));

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
            has_pending_submit: false,
            pending_frame_time: None,
            perf_stats: self.perf_stats.clone(),
            session_start: Instant::now(),
            bitmap_fallback: false,
            keyframe_requested: true,
            ack_skip_streak: 0,
            last_full_uncompressed: None,
            last_sck_retry: None,
        }))
    }
}

/// Everything needed to (re)build the H.264 encoder for this stream.
#[derive(Clone, Copy)]
struct EncoderParams {
    width: u32,
    height: u32,
    fps: f32,
    quality: Quality,
    pref: macrdp_encode::EncoderPreference,
    mode_444: bool,
    bitrate: u32,
}

impl EncoderParams {
    fn create(&self) -> Option<Box<dyn VideoEncoder>> {
        macrdp_encode::create_encoder(self.width, self.height, self.fps, self.quality, self.pref, self.mode_444, self.bitrate)
            .map_err(|e| tracing::error!("H.264 encoder creation failed: {e}"))
            .ok()
    }
}

/// Cached frame data for idle IDR re-encoding.
enum CachedFrame {
    Bgra { data: Bytes, width: u32, height: u32, stride: usize },
    PixelBuffer(SafePixelBuffer, u32, u32), // buffer, width, height
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
    enc_params: EncoderParams,
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
    /// Whether a pipelined encode has been submitted and not yet collected.
    has_pending_submit: bool,
    /// Timestamp of the pending pipelined submit (for encode-time measurement).
    pending_frame_time: Option<Instant>,
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
}

/// How long to wait for the client to open and negotiate the GFX channel before falling
/// back to legacy bitmap updates (clients without GFX support would otherwise see black).
const GFX_NEGOTIATE_TIMEOUT: Duration = Duration::from_secs(3);
/// Skip encoding when more than this many GFX frames await client ACK.
const MAX_PENDING_ACKS: u32 = 3;
/// Upper bound on consecutive ACK-backlog skips, so a client that suspends frame
/// acknowledgements (SUSPEND_FRAME_ACKNOWLEDGEMENT) cannot stall the stream.
const MAX_ACK_SKIP_STREAK: u32 = 10;
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
        let frame = loop {
            // Calculate timeout for idle IDR keepalive.
            // If we have a cached frame and are in idle state, wait at most until
            // the next IDR keepalive is due instead of blocking forever.
            let idle_deadline = if self.idle_frame_count > 0 && self.last_frame_cache.is_some() {
                let elapsed = self.last_idr_time.elapsed();
                if elapsed >= IDLE_IDR_INTERVAL {
                    // IDR keepalive is overdue — send it immediately
                    if let Some(update) = self.send_idle_idr().await? {
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
            // Pipeline tail: the last submitted frame has no successor to collect it, and a
            // static desktop yields no SCK event — wait at most one frame interval for it.
            let idle_deadline = if self.has_pending_submit {
                let frame_interval = Duration::from_secs(1) / self.last_applied_fps.max(1);
                Some(idle_deadline.map_or(frame_interval, |d| d.min(frame_interval)))
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
                        // Timeout — collect the pipeline tail if any, else idle IDR keepalive
                        // (send_idle_idr collects a pending submit first and returns it).
                        if let Some(update) = self.send_idle_idr().await? {
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
                        // A still-pending pipelined frame is collected (and returned) by
                        // encode_and_send when it submits this frame — never dropped here.
                        tracing::debug!(
                            idle_frames = self.idle_frame_count,
                            "Desktop active again — forcing keyframe for clean recovery"
                        );
                        if let Some(enc) = &mut self.encoder {
                            enc.force_keyframe();
                        }
                        self.keyframe_requested = true;

                        // Reset BitrateController and restore initial bitrate/fps
                        self.bitrate_ctrl.on_idle_recovery();
                        if let Some(enc) = &mut self.encoder {
                            enc.set_bitrate(self.bitrate_ctrl.current_bitrate());
                        }
                        let target_fps = self.bitrate_ctrl.target_fps();
                        if self.last_applied_fps != target_fps {
                            let _ = self.capturer.set_frame_rate_async(target_fps).await;
                            self.last_applied_fps = target_fps;
                        }
                    }
                    self.idle_frame_count = 0;
                    f
                }
                macrdp_capture::CaptureEvent::Idle => {
                    self.idle_frame_count += 1;
                    // Pipeline tail: the last submitted frame has no successor to collect it.
                    if self.has_pending_submit {
                        let (w, h) = self.cached_frame_size();
                        if let Some(update) = self.collect_and_send_pending(w, h).await? {
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
            break frame;
        };

        self.encode_and_send(frame).await
    }
}

impl MacDisplayUpdates {
    /// SCStream stopped (lock screen?) — serve one CoreGraphics frame, retrying the SCStream
    /// rebuild at most every `SCK_RETRY_INTERVAL`. Returns `None` once SCK has recovered
    /// (caller resumes the normal path), `Some(update)` for a CG frame.
    async fn cg_fallback_update(&mut self) -> Result<Option<DisplayUpdate>> {
        self.idle_frame_count = 0;
        let fallback = CgFallbackCapturer::new(&self.capture_config);
        loop {
            if self.last_sck_retry.is_none_or(|t| t.elapsed() >= SCK_RETRY_INTERVAL) {
                tracing::warn!("SCStream stopped — switching to CoreGraphics fallback (lock screen?)");
                self.last_sck_retry = Some(Instant::now());
                if let Ok(new_capturer) = ScreenCapturer::new(self.capture_config.clone(), None).await {
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
                return r;
            }
            Ok((_, Err(_))) => anyhow::anyhow!("encoder panicked"),
            Err(join) => anyhow::anyhow!("encoder task cancelled: {join}"),
        };
        tracing::error!("{err} — rebuilding H.264 encoder");
        self.encoder = self.enc_params.create();
        self.keyframe_requested = true;
        self.has_pending_submit = false;
        self.pending_frame_time = None;
        Err(err)
    }

    /// Size of the cached last frame (also the size of any pending pipelined submit).
    fn cached_frame_size(&self) -> (u16, u16) {
        match &self.last_frame_cache {
            Some(CachedFrame::Bgra { width, height, .. }) => (*width as u16, *height as u16),
            Some(CachedFrame::PixelBuffer(_, w, h)) => (*w as u16, *h as u16),
            None => (self.capture_config.width as u16, self.capture_config.height as u16),
        }
    }

    /// Re-encode the cached last frame as an IDR keyframe (keepalive during idle).
    /// Returns `None` if no cached frame or the client cannot take H.264 over GFX.
    async fn send_idle_idr(&mut self) -> Result<Option<DisplayUpdate>> {
        // A pipelined frame still in flight is newer than the cache — deliver it first.
        if self.has_pending_submit {
            let (w, h) = self.cached_frame_size();
            if let Some(update) = self.collect_and_send_pending(w, h).await? {
                return Ok(Some(update));
            }
        }
        if !self.gfx_state.lock().unwrap().avc420_supported() {
            return Ok(None);
        }
        let Some(cache) = self.last_frame_cache.as_ref() else { return Ok(None) };
        if self.encoder.is_none() {
            return Ok(None);
        }
        let (vis_w, vis_h) = self.cached_frame_size();
        // Owned copy for the blocking pool: Bytes is refcounted, PixelBuffer gets its own retain.
        let input = match cache {
            CachedFrame::Bgra { data, width, height, stride } => {
                CachedFrame::Bgra { data: data.clone(), width: *width, height: *height, stride: *stride }
            }
            CachedFrame::PixelBuffer(buf, w, h) => CachedFrame::PixelBuffer(buf.clone_ref(), *w, *h),
        };

        let t0 = Instant::now();
        let result = self
            .with_encoder_blocking(move |enc| {
                enc.force_keyframe();
                match &input {
                    CachedFrame::Bgra { data, width, height, stride } => {
                        enc.encode_bgra(data, *width, *height, *stride)
                    }
                    CachedFrame::PixelBuffer(buf, ..) => enc.encode_pixel_buffer(buf.as_ptr(), true),
                }
            })
            .await;

        match result {
            Ok(encoded) if !encoded.data.is_empty() => {
                let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    h264_bytes = encoded.data.len(),
                    encode_ms = format!("{:.1}", encode_ms),
                    idle_frames = self.idle_frame_count,
                    "Sent idle IDR keepalive"
                );
                self.last_idr_time = Instant::now();
                Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                    h264_data: encoded.data,
                    width: vis_w,
                    height: vis_h,
                    enc_width: encoded.width as u16,
                    enc_height: encoded.height as u16,
                    is_keyframe: true,
                    h264_aux: None,
                })))
            }
            Ok(_) => {
                macrdp_encode::warn_limited!("Idle IDR encode returned empty data");
                Ok(None)
            }
            Err(e) => {
                macrdp_encode::warn_limited!("Idle IDR encode failed: {e}");
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

        if let Some(ref mut enc) = self.encoder {
            enc.set_bitrate(decision.bitrate_bps);
        }

        if decision.fps != self.last_applied_fps {
            if let Err(e) = self.capturer.set_frame_rate_async(decision.fps).await {
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

    /// Collect a previously submitted async (pipelined) encode result and build a GFX DisplayUpdate.
    async fn collect_and_send_pending(&mut self, frame_width: u16, frame_height: u16) -> Result<Option<DisplayUpdate>> {
        // Idempotent: nothing in flight (or already collected) → nothing to do.
        if !self.has_pending_submit || !self.encoder.as_ref().is_some_and(|e| e.supports_pipelining()) {
            return Ok(None);
        }

        let collected = match self
            .with_encoder_blocking(|enc| enc.collect_encoded(Duration::from_millis(32)))
            .await
        {
            Ok(c) => c,
            Err(e) => {
                macrdp_encode::warn_limited!("Pipelined collect failed: {e} — frame lost");
                self.has_pending_submit = false;
                self.pending_frame_time = None;
                return Ok(Some(DisplayUpdate::DefaultPointer));
            }
        };
        match collected {
            Some(encoded) if !encoded.data.is_empty() => {
                let encode_ms = self.pending_frame_time
                    .map(|t| t.elapsed().as_secs_f64() * 1000.0)
                    .unwrap_or(0.0);
                let is_keyframe = encoded.is_keyframe;

                tracing::debug!(
                    h264_bytes = encoded.data.len(),
                    is_keyframe,
                    encode_ms = format!("{:.1}", encode_ms),
                    "Display: collected pipelined GFX frame"
                );
                {
                    let mut st = self.gfx_state.lock().unwrap();
                    st.last_encode_ms = encode_ms;
                    st.last_frame_bytes = encoded.data.len() as u32;
                }
                self.bitrate_ctrl.record_frame(crate::bitrate_controller::FrameStats {
                    encode_ms,
                    frame_bytes: encoded.data.len() as u32,
                    is_keyframe,
                });
                if let Some(ps) = &self.perf_stats {
                    ps.lock().unwrap().record_frame(encode_ms, encoded.data.len() as u32, is_keyframe);
                }
                self.apply_adaptive_decision().await;

                if is_keyframe {
                    self.last_idr_time = Instant::now();
                }

                self.has_pending_submit = false;
                self.pending_frame_time = None;

                Ok(Some(DisplayUpdate::GfxFrame(GfxFrameUpdate {
                    h264_data: encoded.data,
                    width: frame_width,
                    height: frame_height,
                    enc_width: encoded.width as u16,
                    enc_height: encoded.height as u16,
                    is_keyframe,
                    h264_aux: None,
                })))
            }
            Some(_) => {
                self.has_pending_submit = false;
                self.pending_frame_time = None;
                Ok(Some(DisplayUpdate::DefaultPointer))
            }
            None => {
                macrdp_encode::warn_limited!("Pipelined collect timed out — frame lost");
                self.has_pending_submit = false;
                self.pending_frame_time = None;
                Ok(Some(DisplayUpdate::DefaultPointer))
            }
        }
    }

    /// GFX uncompressed path: send raw BGRA rects instead of H.264 (zero encode latency).
    ///
    /// `full = false`: only when the total dirty area is small (≤ UNCOMPRESSED_MAX_PIXELS).
    /// `full = true` (client negotiated GFX without AVC420): no area cap; the whole frame is
    /// sent when dirty-rect info is unavailable or this is the first frame of the stream.
    /// Returns `None` for PixelBuffer frames, no/too-large dirty rects, or extraction failure.
    fn try_uncompressed(&mut self, frame: &CapturedFrame, full: bool) -> Option<DisplayUpdate> {
        let whole = macrdp_capture::Rect { x: 0, y: 0, width: frame.width, height: frame.height };
        let whole_frame = full && (frame.dirty_rects.is_empty() || self.display_frame_count == 0);
        if whole_frame {
            if self.last_full_uncompressed.is_some_and(|t| t.elapsed() < FULL_UNCOMPRESSED_MIN_INTERVAL) {
                return Some(DisplayUpdate::DefaultPointer);
            }
            self.last_full_uncompressed = Some(Instant::now());
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
        // Encode overload protection: skip this frame if previous encode took too long
        if self.skip_next_frame {
            self.skip_next_frame = false;
            return Ok(Some(DisplayUpdate::DefaultPointer));
        }

        // Late GFX negotiation: the DVC side still completes caps exchange after our bitmap
        // fallback latched, and a client in GFX mode ignores legacy bitmaps (frozen screen).
        // Switch over with a fresh whole frame / keyframe.
        if self.bitmap_fallback && self.gfx_state.lock().unwrap().is_ready() {
            tracing::info!("GFX channel negotiated after bitmap fallback — switching to GFX");
            self.bitmap_fallback = false;
            self.keyframe_requested = true;
            self.display_frame_count = 0;
            if let Some(enc) = &mut self.encoder {
                enc.force_keyframe();
            }
        }

        // GFX negotiated ≠ H.264 capable: `is_ready()` only means the channel is open and caps
        // confirmed; `GfxFrame` needs `avc420_supported()`, otherwise go uncompressed.
        let (gfx_ready, use_h264, use_444, pending_acks) = {
            let state = self.gfx_state.lock().unwrap();
            (
                state.is_ready() && !self.bitmap_fallback,
                state.avc420_supported() && self.encoder.is_some() && !self.bitmap_fallback,
                self.mode_444 && state.avc444_supported() && state.avc444_enabled,
                state.pending_acks,
            )
        };

        if gfx_ready && !use_h264 {
            // Whole frame over GFX uncompressed (no H.264 on this client / no encoder).
            if let Some(update) = self.try_uncompressed(&frame, true) {
                return Ok(Some(update));
            }
            macrdp_encode::warn_limited!("GFX client without AVC420 and frame is not BGRA — dropping frame");
            return Ok(Some(DisplayUpdate::DefaultPointer));
        }

        if use_h264 {
            // Flow control: skip encoding (not just sending) while the client is behind on
            // ACKs, so the H.264 reference chain stays intact. Keyframes are never skipped,
            // and a client that stopped ACKing cannot stall us past MAX_ACK_SKIP_STREAK.
            if pending_acks > MAX_PENDING_ACKS
                && !self.keyframe_requested
                && self.ack_skip_streak < MAX_ACK_SKIP_STREAK
            {
                self.ack_skip_streak += 1;
                tracing::debug!(pending_acks, streak = self.ack_skip_streak, "ACK backlog — skipping frame before encode");
                return Ok(Some(DisplayUpdate::DefaultPointer));
            }
            self.ack_skip_streak = 0;
            self.keyframe_requested = false;

            // GFX uncompressed path — for small dirty regions, skip H.264 encoding entirely.
            // Not while a pipelined encode is in flight: the raw rects would land on the
            // client's surface before the older pending frame and then be painted over.
            if !self.has_pending_submit {
                if let Some(update) = self.try_uncompressed(&frame, false) {
                    return Ok(Some(update));
                }
            }

            // ── Pipelining fast-path (checked BEFORE borrowing encoder long-term) ──
            // Check pipelining support with a short borrow, then handle collect/submit
            // using &mut self methods that don't conflict with the encoder borrow.
            let supports_pipeline = self.encoder.as_ref().is_some_and(|e| e.supports_pipelining());

            // AVC444 stays synchronous — only pipeline AVC420 (PixelBuffer or BGRA).
            let pipeline_444 = matches!(frame.data, FrameData::Raw(_))
                && use_444
                && self.encoder.as_ref().unwrap().supports_444();
            if supports_pipeline && !pipeline_444 {
                // Collect previous first — `&frame.data` must not be held across this await
                // (SafePixelBuffer is !Sync, so a reference to it is !Send).
                let prev_update = if self.has_pending_submit {
                    self.collect_and_send_pending(frame.width as u16, frame.height as u16).await?
                } else {
                    None
                };

                let t0_pipe = Instant::now();
                let encoder = self.encoder.as_mut().unwrap();
                self.last_frame_cache = Some(match &frame.data {
                    FrameData::PixelBuffer(buf) => {
                        encoder.submit_pixel_buffer(buf.as_ptr())?;
                        CachedFrame::PixelBuffer(buf.clone_ref(), frame.width, frame.height)
                    }
                    FrameData::Raw(bgra) => {
                        encoder.submit_bgra(bgra, frame.width, frame.height, frame.stride)?;
                        CachedFrame::Bgra {
                            data: bgra.clone(), width: frame.width, height: frame.height, stride: frame.stride,
                        }
                    }
                });
                self.has_pending_submit = true;
                self.pending_frame_time = Some(t0_pipe);
                self.display_frame_count += 1;

                // Return previous frame's result (first frame returns DefaultPointer)
                return Ok(prev_update.or(Some(DisplayUpdate::DefaultPointer)));
            }

            // ── Synchronous path (BGRA only, also fallback when !supports_pipeline) ──
            if self.encoder.is_some() {
                self.display_frame_count += 1;
                let t0 = std::time::Instant::now();

                // PixelBuffer frames are only produced for supports_pixel_buffer() encoders (VT),
                // which always pipeline, so they never reach this path.
                let FrameData::Raw(bgra) = &frame.data else {
                    macrdp_encode::warn_limited!("PixelBuffer frame on synchronous encode path — dropping frame");
                    return Ok(Some(DisplayUpdate::DefaultPointer));
                };

                // AVC444 dual-stream path (CPU colour conversion + encode → blocking pool)
                if use_444 && self.encoder.as_ref().is_some_and(|e| e.supports_444()) {
                    let (data, w, h, stride) = (bgra.clone(), frame.width, frame.height, frame.stride);
                    match self.with_encoder_blocking(move |enc| enc.encode_bgra_444(&data, w, h, stride)).await {
                        Ok(encoded) if !encoded.main_view.data.is_empty() => {
                            let encode_ms = t0.elapsed().as_secs_f64() * 1000.0;
                            let total_bytes = encoded.main_view.data.len() + encoded.aux_view.data.len();
                            let is_keyframe = encoded.main_view.is_keyframe;
                            tracing::debug!(
                                display_frame = self.display_frame_count,
                                main_bytes = encoded.main_view.data.len(),
                                aux_bytes = encoded.aux_view.data.len(),
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
                            self.last_frame_cache = Some(CachedFrame::Bgra {
                                data: bgra.clone(), width: frame.width, height: frame.height, stride: frame.stride,
                            });
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
                                h264_aux: Some(encoded.aux_view.data),
                            })));
                        }
                        Ok(_) => {
                            macrdp_encode::warn_limited!(
                                display_frame = self.display_frame_count,
                                "AVC444 encode returned EMPTY data — frame dropped!"
                            );
                            return Ok(Some(DisplayUpdate::DefaultPointer));
                        }
                        Err(e) => {
                            macrdp_encode::warn_limited!(display_frame = self.display_frame_count, "AVC444 encode failed: {e}, falling back to AVC420");
                            // Fall through to AVC420 path below
                        }
                    }
                }

                // AVC420 path (default or fallback from AVC444 failure).
                // OpenH264 is a synchronous CPU encode (~tens of ms); run it on the blocking
                // pool so the single-threaded server runtime keeps handling input/ACKs meanwhile.
                let encoded = {
                    let (data, w, h, stride) = (bgra.clone(), frame.width, frame.height, frame.stride);
                    self.with_encoder_blocking(move |enc| enc.encode_bgra(&data, w, h, stride)).await
                };
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
                        self.last_frame_cache = Some(CachedFrame::Bgra {
                            data: bgra.clone(), width: frame.width, height: frame.height, stride: frame.stride,
                        });
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
                            h264_aux: None,
                        })));
                    }
                    Ok(_) => {
                        macrdp_encode::warn_limited!(
                            display_frame = self.display_frame_count,
                            "H.264 encode returned EMPTY data — frame dropped!"
                        );
                        return Ok(Some(DisplayUpdate::DefaultPointer));
                    }
                    Err(e) => {
                        macrdp_encode::warn_limited!(display_frame = self.display_frame_count, "H.264 encode failed: {e}");
                        return Ok(Some(DisplayUpdate::DefaultPointer));
                    }
                }
            }
        } else if self.encoder.is_some() && !self.bitmap_fallback {
            // H.264 encoder exists — give the client a grace period to open and negotiate
            // the GFX channel. Mixing bitmap and GFX in one session causes 0xd06
            // DECOMPRESSION_FAILED, so once we fall back we latch to bitmap for this stream.
            if self.session_start.elapsed() < GFX_NEGOTIATE_TIMEOUT {
                return Ok(Some(DisplayUpdate::DefaultPointer));
            }
            tracing::warn!(
                timeout_secs = GFX_NEGOTIATE_TIMEOUT.as_secs(),
                "GFX channel not negotiated — falling back to legacy bitmap updates"
            );
            self.bitmap_fallback = true;
        }

        // Bitmap path (only when GFX is not available at all)
        // Requires BGRA raw bytes — PixelBuffer frames should not reach here
        let bgra_bitmap = match &frame.data {
            FrameData::Raw(bytes) => bytes,
            FrameData::PixelBuffer(_) => {
                macrdp_encode::warn_limited!("PixelBuffer frame in bitmap path — should not happen");
                return Ok(Some(DisplayUpdate::DefaultPointer));
            }
        };

        if !frame.dirty_rects.is_empty() {
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
