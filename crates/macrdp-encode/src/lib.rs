//! Video encoding abstraction (H.264 via VideoToolbox / OpenH264)

mod openh264_enc;
pub mod yuv444_split;
#[cfg(target_os = "macos")]
pub mod color_convert;
#[cfg(target_os = "macos")]
mod videotoolbox;

use anyhow::Result;
use bytes::Bytes;

pub use openh264_enc::OpenH264Encoder;
#[cfg(target_os = "macos")]
pub use videotoolbox::VtEncoder;

/// Encoded frame output
pub struct EncodedFrame {
    /// H.264 NAL units (Annex B format)
    pub data: Bytes,
    /// Whether this is a key frame (IDR)
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
}

/// AVC444 dual-stream encoded result
pub struct Avc444EncodedFrame {
    /// Stream1: Main YUV420 H.264 (luma + downsampled chroma)
    pub main_view: EncodedFrame,
    /// Stream2: Auxiliary YUV420 H.264 (chroma compensation).
    /// `None` = aux planes identical to the last sent aux view: send LC=LUMA only, the client
    /// keeps its previous chroma. Always `Some` on a keyframe.
    pub aux_view: Option<EncodedFrame>,
}

/// Quality preset
#[derive(Debug, Clone, Copy)]
pub enum Quality {
    LowLatency,
    Balanced,
    HighQuality,
}

/// Video encoder trait
pub trait VideoEncoder: Send {
    /// AVC420 encode (existing)
    fn encode_bgra(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<EncodedFrame>;

    /// AVC444 dual-stream encode.
    /// Internally performs BGRA -> YUV444 -> B-area split -> dual session encode.
    fn encode_bgra_444(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<Avc444EncodedFrame>;

    /// Zero-copy encode from CVPixelBuffer (NV12). Default returns error (unsupported).
    /// Only VtEncoder overrides this.
    fn encode_pixel_buffer(&mut self, _ptr: *mut std::ffi::c_void, _force_keyframe: bool) -> Result<EncodedFrame> {
        anyhow::bail!("encode_pixel_buffer not supported by this encoder")
    }

    fn set_bitrate(&mut self, bitrate_bps: u32);
    fn force_keyframe(&mut self);

    /// Whether this encoder supports AVC444 dual-stream encoding
    fn supports_444(&self) -> bool;

    /// Whether `encode_pixel_buffer` (zero-copy NV12 CVPixelBuffer input) is implemented.
    /// Callers use this to pick the capture pixel format *after* the encoder was actually
    /// created, so a VT→OpenH264 fallback never ends up fed NV12 frames.
    fn supports_pixel_buffer(&self) -> bool { false }
}

/// Per-call-site 5-second rate limiter for `warn!` on per-frame failure paths.
pub struct LogLimiter(std::sync::Mutex<Option<std::time::Instant>>);

impl LogLimiter {
    pub const fn new() -> Self {
        Self(std::sync::Mutex::new(None))
    }

    /// True at most once per 5 seconds.
    pub fn allow(&self) -> bool {
        let mut last = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(t) if t.elapsed() < std::time::Duration::from_secs(5) => false,
            _ => {
                *last = Some(std::time::Instant::now());
                true
            }
        }
    }
}

impl Default for LogLimiter {
    fn default() -> Self { Self::new() }
}

/// `tracing::warn!` that fires at most once per 5 s per call site.
#[macro_export]
macro_rules! warn_limited {
    ($($arg:tt)*) => {{
        static LIMITER: $crate::LogLimiter = $crate::LogLimiter::new();
        if LIMITER.allow() {
            tracing::warn!($($arg)*);
        }
    }};
}

/// Align a dimension up to the nearest multiple of 16 (H.264 macroblock size)
pub fn align16(v: u32) -> u32 {
    (v + 15) & !15
}

/// Target bitrate for screen content, derived from bits per pixel per frame:
/// LowLatency 0.10 | Balanced 0.20 | HighQuality 0.35, clamped to [2, 80] Mbps.
///
/// 1920×1080@60 → 12.4 / 24.9 / 43.5 Mbps;  2880×1800@60 → 31.1 / 62.2 / 80 (cap, raw 108.9) Mbps.
pub fn screen_bitrate(width: u32, height: u32, fps: f32, quality: Quality) -> u32 {
    let bits_per_pixel = match quality {
        Quality::LowLatency => 0.10,
        Quality::Balanced => 0.20,
        Quality::HighQuality => 0.35,
    };
    let bps = width as f64 * height as f64 * fps.max(1.0) as f64 * bits_per_pixel;
    bps.clamp(2_000_000.0, 80_000_000.0) as u32
}

/// Y'CbCr range used by every BGRA→YUV conversion in this crate (vImage, scalar fallbacks,
/// VideoToolbox session). All sites key off this one constant so they can never drift apart.
///
/// `true`  = BT.601 limited/video range (Y 16–235, CbCr 16–240), NV12 '420v', VUI full_range=0.
/// `false` = BT.601 full range (Y/CbCr 0–255), NV12 '420f', VUI full_range=1.
///
/// Full range: limited range was measured to render washed-out/grey on real clients, and
/// FreeRDP converts AVC420 output as full range regardless of VUI.
pub const LIMITED_RANGE: bool = false;

/// BT.601 luma for one pixel (scalar reference used by all non-vImage paths).
#[inline(always)]
pub fn bt601_y(r: i32, g: i32, b: i32) -> u8 {
    if LIMITED_RANGE {
        // 16 ..= 235, provably in range for 8-bit input
        (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
    } else {
        ((77 * r + 150 * g + 29 * b + 128) >> 8) as u8
    }
}

/// BT.601 chroma (Cb, Cr) for one (possibly 2×2-averaged) RGB sample.
#[inline(always)]
pub fn bt601_uv(r: i32, g: i32, b: i32) -> (u8, u8) {
    let (u, v) = if LIMITED_RANGE {
        (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128, ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
    } else {
        (((-43 * r - 85 * g + 128 * b + 128) >> 8) + 128, ((128 * r - 107 * g - 21 * b + 128) >> 8) + 128)
    };
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

/// Encoder preference
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EncoderPreference {
    /// OpenH264 CPU encoder — full P-frame support, higher latency (~40ms)
    Software,
    /// VideoToolbox GPU encoder — low latency (~6ms), supports P-frames
    Hardware,
    /// VideoToolbox when available, otherwise OpenH264 (recommended default)
    Auto,
}

impl EncoderPreference {
    pub fn from_str_opt(s: Option<&str>) -> Self {
        match s.map(|s| s.to_lowercase()).as_deref() {
            Some("hardware") | Some("gpu") | Some("videotoolbox") | Some("vt") => Self::Hardware,
            Some("software") | Some("cpu") | Some("openh264") | Some("oh264") => Self::Software,
            _ => Self::Auto,
        }
    }
}

/// Default per-frame QP ceiling (see `EncoderConfig::max_qp`).
pub const DEFAULT_MAX_QP: u8 = 36;

/// Everything needed to (re)build an H.264 encoder.
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    /// Visible frame size (encoders pad/align internally).
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub quality: Quality,
    pub pref: EncoderPreference,
    /// Dual-session AVC444 (BGRA input only).
    pub mode_444: bool,
    pub bitrate: u32,
    /// Per-frame QP ceiling (1–51): VT `MaxAllowedFrameQP`, OpenH264 `iMaxQp`. Lower = sharper
    /// static text at higher bitrate; 36 default, 30–32 for LAN high-clarity.
    pub max_qp: u8,
}

/// Create an H.264 encoder based on `cfg.pref`.
/// When `cfg.mode_444` is true, the encoder will initialize dual sessions for AVC444 support.
pub fn create_encoder(cfg: EncoderConfig) -> Result<Box<dyn VideoEncoder>> {
    let EncoderConfig { width, height, fps, quality, pref: preference, mode_444, bitrate, max_qp } = cfg;
    let max_qp = max_qp.clamp(1, 51);
    let enc_w = align16(width);
    let enc_h = align16(height);
    tracing::info!(width, height, enc_w, enc_h, fps, ?quality, ?preference, mode_444, max_qp,
        bitrate_mbps = bitrate as f64 / 1_000_000.0, "Creating H.264 encoder");

    // Auto / Hardware: VideoToolbox GPU encoder. The session takes the *visible* (even) size —
    // VT does macroblock alignment and SPS cropping itself. A 16-aligned session instead makes VT
    // *scale* every zero-copy capture buffer up to it (1080→1088 measured by
    // `vt_zero_copy_1080_pixel_buffer_decodes_to_visible_size`), which the client then crops.
    // AVC444 keeps align16: the YUV444 split fills planes sized to the encoder.
    #[cfg(target_os = "macos")]
    if preference != EncoderPreference::Software {
        let (vt_w, vt_h) = if mode_444 { (enc_w, enc_h) } else { ((width + 1) & !1, (height + 1) & !1) };
        match VtEncoder::new(vt_w, vt_h, fps, bitrate, mode_444, max_qp) {
            Ok(encoder) => {
                tracing::info!(vt_w, vt_h, mode_444, "Using VideoToolbox hardware encoder (GPU)");
                return Ok(Box::new(encoder));
            }
            Err(e) => {
                tracing::warn!("VideoToolbox unavailable: {e}, falling back to OpenH264");
            }
        }
    }

    // Software, or VT fallback: OpenH264 CPU encoder (full P-frame support)
    tracing::info!(enc_w, enc_h, mode_444, "Using OpenH264 software encoder (CPU)");
    let encoder = OpenH264Encoder::new(enc_w, enc_h, fps, bitrate, mode_444, max_qp)?;
    Ok(Box::new(encoder))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_bitrate_presets_and_clamps() {
        assert_eq!(screen_bitrate(1920, 1080, 60.0, Quality::LowLatency), 12_441_600);
        assert_eq!(screen_bitrate(1920, 1080, 60.0, Quality::Balanced), 24_883_200);
        assert_eq!(screen_bitrate(1920, 1080, 60.0, Quality::HighQuality), 43_545_600);
        assert_eq!(screen_bitrate(2880, 1800, 60.0, Quality::LowLatency), 31_104_000);
        assert_eq!(screen_bitrate(2880, 1800, 60.0, Quality::Balanced), 62_208_000);
        // 108.9 Mbps raw → capped
        assert_eq!(screen_bitrate(2880, 1800, 60.0, Quality::HighQuality), 80_000_000);
        // tiny screens never starve the encoder
        assert_eq!(screen_bitrate(640, 480, 30.0, Quality::LowLatency), 2_000_000);
    }

    #[test]
    fn bt601_scalar_black_white_gray() {
        let (lo, hi) = if LIMITED_RANGE { (16, 235) } else { (0, 255) };
        assert_eq!(bt601_y(0, 0, 0), lo);
        assert_eq!(bt601_y(255, 255, 255), hi);
        assert_eq!(bt601_uv(128, 128, 128), (128, 128));
        let (u, v) = bt601_uv(0, 0, 255); // pure blue → Cb high
        assert!(u > 200 && v < 128, "blue: u={u} v={v}");
    }
}
