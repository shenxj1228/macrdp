//! OpenH264 software H.264 encoder optimized for screen content

use std::ffi::c_void;

use anyhow::{Context, Result};
use bytes::Bytes;
use openh264::encoder::{Encoder, EncoderConfig, FrameType};
use openh264::formats::YUVSlices;

use crate::color_convert::VImageConverter;
use crate::{Avc444EncodedFrame, EncodedFrame, VideoEncoder};

pub struct OpenH264Encoder {
    encoder: Encoder,
    /// Auxiliary encoder for AVC444 chroma stream
    encoder_aux: Option<Encoder>,
    width: u32,
    height: u32,
    force_keyframe: bool,
    yuv_buf: Vec<u8>,
    /// Current target bitrate
    target_bitrate: u32,
    mode_444: bool,
    /// vImage SIMD accelerated BGRA→I420 converter (macOS Accelerate.framework)
    vimage: Option<VImageConverter>,
    /// Reusable buffers for AVC444 YUV444 split
    yuv444_bufs: Option<Yuv444SplitBufs>,
    /// Reusable Annex B output buffer (avoids per-frame Vec growth)
    nal_buf: Vec<u8>,
    /// Source dims of the previous frame; padding is re-neutralised only when they change
    last_src: (u32, u32),
}

/// Reusable buffers for AVC444 encoding
struct Yuv444SplitBufs {
    y444: Vec<u8>,
    u444: Vec<u8>,
    v444: Vec<u8>,
    main_view: crate::yuv444_split::Yuv420Frame,
    aux_view: crate::yuv444_split::Yuv420Frame,
}

impl Yuv444SplitBufs {
    fn new(width: u32, height: u32) -> Self {
        let full = (width * height) as usize;
        Self {
            y444: vec![0u8; full],
            // Padding columns are never written: keep them neutral (black / no chroma)
            u444: vec![128u8; full],
            v444: vec![128u8; full],
            main_view: crate::yuv444_split::Yuv420Frame::new(width, height),
            aux_view: crate::yuv444_split::Yuv420Frame::new(width, height),
        }
    }
}

fn create_oh264_encoder(fps: f32, bitrate: u32, max_qp: u8) -> Result<Encoder> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8) as u16;
    // Screen/text tuned: low QP floor keeps glyph edges crisp, no AQ (it smears flat UI regions),
    // no background detection (static desktop must not be classified as "background" and starved).
    let config = EncoderConfig::new()
        .bitrate(openh264::encoder::BitRate::from_bps(bitrate))
        .max_frame_rate(openh264::encoder::FrameRate::from_hz(fps))
        .rate_control_mode(openh264::encoder::RateControlMode::Quality)
        .background_detection(false)
        .adaptive_quantization(false)
        .qp(openh264::encoder::QpRange::new(10.min(max_qp), max_qp))
        .skip_frames(false)
        // Window switches must not force an IDR (bitrate spike); GOP/force_keyframe handle refresh.
        .scene_change_detect(false)
        .usage_type(openh264::encoder::UsageType::ScreenContentRealTime)
        .complexity(openh264::encoder::Complexity::Medium)
        .intra_frame_period(openh264::encoder::IntraFramePeriod::from_num_frames(fps as u32 * 10))
        .long_term_reference(true)
        .num_threads(threads);

    Encoder::with_api_config(openh264::OpenH264API::from_source(), config)
        .context("Failed to create OpenH264 encoder")
}

impl OpenH264Encoder {
    pub fn new(width: u32, height: u32, fps: f32, bitrate: u32, mode_444: bool, max_qp: u8) -> Result<Self> {

        let encoder = create_oh264_encoder(fps, bitrate, max_qp)?;

        // AVC444: create auxiliary encoder with 70% bitrate
        let encoder_aux = if mode_444 {
            let aux_bitrate = (bitrate as f64 * 0.7) as u32;
            let aux = create_oh264_encoder(fps, aux_bitrate, max_qp)?;
            tracing::info!(aux_bitrate_mbps = aux_bitrate as f64 / 1_000_000.0, "AVC444 auxiliary OpenH264 encoder created");
            Some(aux)
        } else {
            None
        };

        let vimage = VImageConverter::new()
            .map_err(|e| tracing::warn!("vImage init failed, using scalar fallback: {e}"))
            .ok();

        let yuv_size = (width * height * 3 / 2) as usize;

        let yuv444_bufs = if mode_444 {
            Some(Yuv444SplitBufs::new(width, height))
        } else {
            None
        };

        tracing::info!(
            width, height, fps, mode_444,
            bitrate_mbps = bitrate as f64 / 1_000_000.0,
            "OpenH264 encoder created (screen-optimized)"
        );

        Ok(Self {
            encoder,
            encoder_aux,
            width,
            height,
            force_keyframe: false,
            yuv_buf: vec![0u8; yuv_size],
            target_bitrate: bitrate,
            mode_444,
            vimage,
            yuv444_bufs,
            nal_buf: Vec::new(),
            last_src: (0, 0),
        })
    }
}

/// Convert BGRA to YUV420 into an encoder-sized buffer (handles padding).
fn bgra_to_yuv420_padded(
    bgra: &[u8], src_w: u32, src_h: u32, stride: usize,
    enc_w: u32, enc_h: u32, yuv: &mut [u8],
) {
    let ew = enc_w as usize;
    let sw = src_w as usize;
    let sh = src_h as usize;
    let y_plane_size = ew * enc_h as usize;
    let uv_w = ew / 2;

    let (y_plane, uv_planes) = yuv.split_at_mut(y_plane_size);
    let uv_plane_size = uv_w * (enc_h as usize / 2);
    let (u_plane, v_plane) = uv_planes.split_at_mut(uv_plane_size);

    for row in 0..sh {
        for col in 0..sw {
            let bgra_offset = row * stride + col * 4;
            if bgra_offset + 2 >= bgra.len() { continue; }
            let b = bgra[bgra_offset] as i32;
            let g = bgra[bgra_offset + 1] as i32;
            let r = bgra[bgra_offset + 2] as i32;

            y_plane[row * ew + col] = crate::bt601_y(r, g, b);

            if row.is_multiple_of(2) && col.is_multiple_of(2) {
                let (u, v) = crate::bt601_uv(r, g, b);
                let uv_idx = (row / 2) * uv_w + (col / 2);
                u_plane[uv_idx] = u;
                v_plane[uv_idx] = v;
            }
        }
    }
}

/// Borrow a packed I420 buffer as `YUVSlices` (no copy).
fn i420_slices(buf: &[u8], w: u32, h: u32) -> YUVSlices<'_> {
    let (w, h) = (w as usize, h as usize);
    let y_size = w * h;
    let uv_size = (w / 2) * (h / 2);
    let (y, rest) = buf.split_at(y_size);
    let (u, v) = rest.split_at(uv_size);
    YUVSlices::new((y, u, &v[..uv_size]), (w, h), (w, w / 2, w / 2))
}

/// Borrow a planar `Yuv420Frame` as `YUVSlices` (no copy).
fn frame_slices(f: &crate::yuv444_split::Yuv420Frame) -> YUVSlices<'_> {
    let (w, h) = (f.width as usize, f.height as usize);
    let uv_size = (w / 2) * (h / 2);
    YUVSlices::new((&f.y[..w * h], &f.u[..uv_size], &f.v[..uv_size]), (w, h), (w, w / 2, w / 2))
}

impl VideoEncoder for OpenH264Encoder {
    fn encode_bgra(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<EncodedFrame> {
        if width > self.width || height > self.height {
            anyhow::bail!(
                "Frame too large: encoder {}x{}, got {}x{}",
                self.width, self.height, width, height
            );
        }

        // Neutralise the padding area only when the visible size changes (the converters
        // below overwrite the visible region every frame; padding is otherwise untouched).
        let padded = width != self.width || height != self.height;
        if padded && self.last_src != (width, height) {
            let y_size = (self.width * self.height) as usize;
            self.yuv_buf[..y_size].fill(0);
            self.yuv_buf[y_size..].fill(128);
        }
        self.last_src = (width, height);

        // Convert visible BGRA area to YUV420
        match (&self.vimage, padded) {
            (Some(converter), false) => {
                converter.bgra_to_i420(data, width, height, stride, &mut self.yuv_buf)
                    .map_err(|e| anyhow::anyhow!("vImage BGRA->I420 failed: {e}"))?;
            }
            // ponytail: scalar path for padded frames; vImage into a strided dest if this shows up in profiles
            _ => bgra_to_yuv420_padded(data, width, height, stride, self.width, self.height, &mut self.yuv_buf),
        }

        // Force IDR on next frame if requested
        if self.force_keyframe {
            self.encoder.force_intra_frame();
            self.force_keyframe = false;
        }

        let yuv = i420_slices(&self.yuv_buf, self.width, self.height);
        let bitstream = self.encoder.encode(&yuv)
            .context("OpenH264 encode failed")?;

        self.nal_buf.clear();
        bitstream.write_vec(&mut self.nal_buf);

        let is_keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);

        Ok(EncodedFrame {
            data: Bytes::copy_from_slice(&self.nal_buf),
            is_keyframe,
            width: self.width,
            height: self.height,
        })
    }

    fn encode_bgra_444(&mut self, data: &[u8], width: u32, height: u32, stride: usize) -> Result<Avc444EncodedFrame> {
        let encoder_aux = self.encoder_aux.as_mut()
            .ok_or_else(|| anyhow::anyhow!("AVC444 not enabled: no auxiliary encoder"))?;
        let bufs = self.yuv444_bufs.as_mut()
            .ok_or_else(|| anyhow::anyhow!("AVC444 not enabled: no YUV444 buffers"))?;

        let w = self.width;
        let h = self.height;

        // Step 1: BGRA -> YUV444, written with the encoder (align16) row stride so that the
        // split below — which reads with that stride — sees rows where it expects them.
        crate::yuv444_split::bgra_to_yuv444_strided(
            data, width, height, stride, w as usize,
            &mut bufs.y444, &mut bufs.u444, &mut bufs.v444,
        );

        // Step 2: YUV444 -> Main YUV420 + Aux YUV420 (B-area split)
        crate::yuv444_split::yuv444_split_to_yuv420(
            &bufs.y444, &bufs.u444, &bufs.v444,
            w, h,
            &mut bufs.main_view, &mut bufs.aux_view,
        );

        if self.force_keyframe {
            self.encoder.force_intra_frame();
            encoder_aux.force_intra_frame();
            self.force_keyframe = false;
        }

        // Step 3: encode both streams straight from the planar views (no packing / cloning)
        let main_bitstream = self.encoder.encode(&frame_slices(&bufs.main_view))
            .context("OpenH264 main encode failed")?;
        let main_keyframe = matches!(main_bitstream.frame_type(), FrameType::IDR | FrameType::I);
        self.nal_buf.clear();
        main_bitstream.write_vec(&mut self.nal_buf);
        let main_nal = Bytes::copy_from_slice(&self.nal_buf);

        let aux_bitstream = encoder_aux.encode(&frame_slices(&bufs.aux_view))
            .context("OpenH264 aux encode failed")?;
        let aux_keyframe = matches!(aux_bitstream.frame_type(), FrameType::IDR | FrameType::I);
        self.nal_buf.clear();
        aux_bitstream.write_vec(&mut self.nal_buf);
        let aux_nal = Bytes::copy_from_slice(&self.nal_buf);

        tracing::debug!(
            main_bytes = main_nal.len(),
            aux_bytes = aux_nal.len(),
            main_keyframe,
            aux_keyframe,
            "AVC444 OpenH264 dual-stream encode complete"
        );

        Ok(Avc444EncodedFrame {
            main_view: EncodedFrame {
                data: main_nal,
                is_keyframe: main_keyframe,
                width: w,
                height: h,
            },
            aux_view: Some(EncodedFrame {
                data: aux_nal,
                is_keyframe: aux_keyframe,
                width: w,
                height: h,
            }),
        })
    }

    fn set_bitrate(&mut self, bitrate_bps: u32) {
        if self.target_bitrate == bitrate_bps {
            return;
        }
        self.target_bitrate = bitrate_bps;
        unsafe {
            let raw = self.encoder.raw_api();
            let mut bitrate_info = openh264_sys2::SBitrateInfo {
                iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                iBitrate: bitrate_bps as i32,
            };
            let ret = raw.set_option(
                openh264_sys2::ENCODER_OPTION_BITRATE,
                &mut bitrate_info as *mut _ as *mut c_void,
            );
            if ret != 0 {
                tracing::warn!(ret, "OpenH264 set_option(BITRATE) failed, bitrate change deferred");
            } else {
                tracing::info!(bitrate_mbps = bitrate_bps as f64 / 1_000_000.0, "Bitrate updated via SetOption");
            }
        }
        if let Some(ref mut aux) = self.encoder_aux {
            let aux_bitrate = (bitrate_bps as f64 * 0.7) as u32;
            unsafe {
                let raw = aux.raw_api();
                let mut bitrate_info = openh264_sys2::SBitrateInfo {
                    iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                    iBitrate: aux_bitrate as i32,
                };
                let _ = raw.set_option(
                    openh264_sys2::ENCODER_OPTION_BITRATE,
                    &mut bitrate_info as *mut _ as *mut c_void,
                );
            }
        }
    }

    fn force_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn supports_444(&self) -> bool {
        self.mode_444
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_frame() {
        let mut encoder = OpenH264Encoder::new(64, 64, 30.0, 1_000_000, false, crate::DEFAULT_MAX_QP).unwrap();
        let bgra = vec![128u8; 64 * 64 * 4];
        let frame = encoder.encode_bgra(&bgra, 64, 64, 64 * 4).unwrap();
        assert!(!frame.data.is_empty());
        assert!(frame.is_keyframe);
    }

    #[test]
    fn test_encode_padded_frame_non_multiple_of_16() {
        // 1366x768 frame into a 1376x768 encoder, both AVC420 and AVC444 paths
        let (w, h) = (1366u32, 768u32);
        let (ew, eh) = (crate::align16(w), crate::align16(h));
        let mut encoder = OpenH264Encoder::new(ew, eh, 30.0, 4_000_000, true, crate::DEFAULT_MAX_QP).unwrap();
        let stride = w as usize * 4;
        let mut bgra = vec![0u8; stride * h as usize];
        for (i, px) in bgra.chunks_mut(4).enumerate() {
            let v = (i % 251) as u8;
            px.copy_from_slice(&[v, v.wrapping_mul(3), v.wrapping_mul(7), 255]);
        }
        let f = encoder.encode_bgra(&bgra, w, h, stride).unwrap();
        assert!(f.is_keyframe && !f.data.is_empty());
        assert_eq!((f.width, f.height), (ew, eh));
        let f2 = encoder.encode_bgra(&bgra, w, h, stride).unwrap();
        assert!(!f2.data.is_empty());

        let f444 = encoder.encode_bgra_444(&bgra, w, h, stride).unwrap();
        assert!(!f444.main_view.data.is_empty() && !f444.aux_view.unwrap().data.is_empty());
    }
}
