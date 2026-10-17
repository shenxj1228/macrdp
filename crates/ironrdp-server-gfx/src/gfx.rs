//! RDPGFX (Graphics Pipeline) DVC handler

use std::sync::{Arc, Mutex};

use ironrdp_core::{encode_vec, impl_as_any, Encode, WriteCursor, EncodeResult};
use ironrdp_dvc::{DvcEncode, DvcProcessor, DvcServerProcessor};
use ironrdp_graphics::rdp6::{BgrAChannels, BitmapStreamEncoder};
use ironrdp_pdu::geometry::InclusiveRectangle;
use ironrdp_pdu::rdp::vc::dvc::gfx::{
    Avc420BitmapStream, Avc444BitmapStream, CapabilitiesConfirmPdu, CapabilitiesV10Flags,
    CapabilitiesV103Flags, CapabilitiesV104Flags, CapabilitiesV107Flags, CapabilitiesV81Flags,
    CapabilitySet, ClientPdu, Codec1Type, CreateSurfacePdu, EndFramePdu, Encoding,
    MapSurfaceToOutputPdu, PixelFormat as GfxPixelFormat, QuantQuality, QueueDepth, ResetGraphicsPdu,
    ServerPdu, StartFramePdu, Timestamp,
};
use ironrdp_pdu::gcc::{Monitor, MonitorFlags};
use ironrdp_pdu::{decode, encode_err, pdu_other_err, PduResult};

type DvcMessage = ironrdp_dvc::DvcMessage;
use tracing::{debug, info, warn};

use crate::display::{GfxFrameUpdate, GfxUncompressedUpdate, UncompressedRect};

/// GFX channel name as defined by RDP spec
pub const GFX_CHANNEL_NAME: &str = "Microsoft::Windows::RDS::Graphics";

/// Wrapper to make raw PDU bytes usable as DvcMessage
pub struct RawGfxPdu(pub Vec<u8>);

impl Encode for RawGfxPdu {
    fn encode(&self, dst: &mut WriteCursor<'_>) -> EncodeResult<()> {
        dst.write_slice(&self.0);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "GfxServerPdu"
    }

    fn size(&self) -> usize {
        self.0.len()
    }
}

impl DvcEncode for RawGfxPdu {}

// SAFETY: RawGfxPdu only contains Vec<u8> which is Send
unsafe impl Send for RawGfxPdu {}

/// Wrap raw GFX PDU bytes in RDP_SEGMENTED_DATA (ZGFX) format.
/// MS-RDPEGFX Section 2.2.5: ALL GFX PDUs must be ZGFX-wrapped before DVC transport.
/// Using uncompressed mode (descriptor 0xE0/0xE1, compression type 0x04).
const ZGFX_SINGLE: u8 = 0xE0;
const ZGFX_MULTIPART: u8 = 0xE1;
const ZGFX_UNCOMPRESSED: u8 = 0x04;
/// Max data per segment = 65534 bytes. The segmentSize field includes the
/// 1-byte compression type, so segmentSize = data_len + 1 ≤ 65535 (0xFFFF).
const ZGFX_MAX_SEG_DATA: usize = 65534;
/// Bytes reserved at the front of a frame buffer for the single-segment ZGFX header.
const ZGFX_SINGLE_HEADER: usize = 2;

fn wrap_zgfx(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ZGFX_SINGLE_HEADER + data.len());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(data);
    finish_zgfx(out)
}

/// Finish a buffer whose first `ZGFX_SINGLE_HEADER` bytes are reserved.
/// Single-segment: header filled in place, no copy. Multipart (>64 KiB): one copy.
fn finish_zgfx(mut buf: Vec<u8>) -> Vec<u8> {
    let data_len = buf.len() - ZGFX_SINGLE_HEADER;
    if data_len <= ZGFX_MAX_SEG_DATA {
        buf[0] = ZGFX_SINGLE;
        buf[1] = ZGFX_UNCOMPRESSED;
        return buf;
    }
    let data = &buf[ZGFX_SINGLE_HEADER..];
    // Multipart: descriptor(1) + seg_count(2) + uncompressed_size(4) + segments
    let seg_count = data_len.div_ceil(ZGFX_MAX_SEG_DATA);
    let mut out = Vec::with_capacity(7 + data_len + seg_count * 5);
    out.push(ZGFX_MULTIPART);
    out.extend_from_slice(&(seg_count as u16).to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for chunk in data.chunks(ZGFX_MAX_SEG_DATA) {
        // segmentSize = compression_type(1) + chunk data
        let seg_size = (chunk.len() + 1) as u32;
        out.extend_from_slice(&seg_size.to_le_bytes());
        out.push(ZGFX_UNCOMPRESSED);
        out.extend_from_slice(chunk);
    }
    out
}

/// Unwrap ZGFX segmented data from client. Returns None if not ZGFX-wrapped.
fn unwrap_zgfx(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return None;
    }
    match data[0] {
        0xE0 => {
            // Single segment: skip descriptor(1) + compression_type(1)
            if data.len() > 2 && data[1] == 0x04 {
                Some(data[2..].to_vec())
            } else {
                None
            }
        }
        0xE1 => {
            // Multipart: descriptor(1) + segment_count(2) + uncompressed_size(4)
            if data.len() < 7 {
                return None;
            }
            let seg_count = u16::from_le_bytes([data[1], data[2]]) as usize;
            let mut offset = 7;
            let mut result = Vec::new();
            for _ in 0..seg_count {
                if offset + 4 > data.len() {
                    break;
                }
                let seg_size = u32::from_le_bytes([
                    data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
                ]) as usize;
                offset += 4;
                // segmentSize is client-controlled: it covers the compression-type byte,
                // so 0 is malformed, and the segment must fit inside the payload.
                if seg_size == 0 {
                    return None;
                }
                if offset < data.len() && data[offset] == ZGFX_UNCOMPRESSED {
                    // Uncompressed: skip compression_type byte
                    offset += 1;
                    let end = offset.checked_add(seg_size - 1)?;
                    if end < offset || end > data.len() {
                        return None;
                    }
                    result.extend_from_slice(&data[offset..end]);
                    offset = end;
                } else {
                    break;
                }
            }
            Some(result)
        }
        _ => None, // Not ZGFX-wrapped
    }
}

fn make_dvc_message(pdu: &ServerPdu) -> PduResult<DvcMessage> {
    let data = encode_vec(pdu).map_err(|e| encode_err!(e))?;
    let wrapped = wrap_zgfx(&data);
    Ok(Box::new(RawGfxPdu(wrapped)))
}

/// Size of the slot ring used for frame RTT tracking (power of two, indexed by `frame_id & MASK`).
const SEND_TIME_SLOTS: usize = 128;

/// Lenient RDPGFX_CAPS_ADVERTISE parser (MS-RDPEGFX 2.2.3.1 / 2.2.1.1).
///
/// `CapabilitiesAdvertisePdu::decode` rejects the whole PDU when any
/// RDPGFX_CAPSET carries a version unknown to ironrdp-pdu 0.7 (anything newer
/// than 0xA0702 — e.g. V10_8+ sent by recent mstsc / MRD / Android clients).
/// We walk the wire format ourselves: `capsSetCount: u16`, then per set
/// `version: u32, capsDataLength: u32, capsData[capsDataLength]`, decoding each
/// set on its own and skipping those ironrdp cannot parse.
///
/// `data` starts at the RDPGFX_HEADER. Returns `(known_sets, skipped_count)`.
fn parse_caps_advertise_lenient(data: &[u8]) -> Option<(Vec<CapabilitySet>, usize)> {
    const HEADER: usize = 8; // cmdId u16 + flags u16 + pduLength u32
    const CAPSET_HEADER: usize = 8; // version u32 + capsDataLength u32
    if data.len() < HEADER + 2 || u16::from_le_bytes([data[0], data[1]]) != 0x12 {
        return None;
    }
    let count = u16::from_le_bytes([data[HEADER], data[HEADER + 1]]) as usize;
    let mut off = HEADER + 2;
    let mut known = Vec::with_capacity(count);
    let mut skipped = 0;
    for _ in 0..count {
        if off + CAPSET_HEADER > data.len() {
            break;
        }
        let version = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
        let len = u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]) as usize;
        let end = off.checked_add(CAPSET_HEADER)?.checked_add(len)?;
        if end > data.len() {
            break;
        }
        match decode::<CapabilitySet>(&data[off..end]) {
            Ok(cap) => known.push(cap),
            Err(e) => {
                debug!(version = format_args!("{version:#x}"), len, "GFX: skipping unparseable capset: {e}");
                skipped += 1;
            }
        }
        off = end;
    }
    Some((known, skipped))
}

/// Human-readable `version(flags)` summary of a capability set, for logs.
fn describe_cap(cap: &CapabilitySet) -> String {
    match cap {
        CapabilitySet::V8 { flags } => format!("V8({:#x})", flags.bits()),
        CapabilitySet::V8_1 { flags } => format!("V8_1({:#x})", flags.bits()),
        CapabilitySet::V10 { flags } => format!("V10({:#x})", flags.bits()),
        CapabilitySet::V10_1 => "V10_1".to_owned(),
        CapabilitySet::V10_2 { flags } => format!("V10_2({:#x})", flags.bits()),
        CapabilitySet::V10_3 { flags } => format!("V10_3({:#x})", flags.bits()),
        CapabilitySet::V10_4 { flags } => format!("V10_4({:#x})", flags.bits()),
        CapabilitySet::V10_5 { flags } => format!("V10_5({:#x})", flags.bits()),
        CapabilitySet::V10_6 { flags } => format!("V10_6({:#x})", flags.bits()),
        CapabilitySet::V10_6Err { flags } => format!("V10_6Err({:#x})", flags.bits()),
        CapabilitySet::V10_7 { flags } => format!("V10_7({:#x})", flags.bits()),
        CapabilitySet::Unknown(data) => format!("Unknown({} bytes)", data.len()),
    }
}

/// Shared state between GfxHandler and the server
#[derive(Debug)]
pub struct GfxState {
    pub channel_id: Option<u32>,
    pub surface_created: bool,
    pub caps_confirmed: bool,
    pub frame_id: u32,
    pub avc420_supported: bool,
    /// Whether the client supports AVC444 (V10+ with AVC not disabled)
    pub avc444_supported: bool,
    /// Whether AVC444 is enabled by server config (chroma_mode = "avc444")
    pub avc444_enabled: bool,
    pub confirmed_cap: Option<CapabilitySet>,
    pub width: u16,
    pub height: u16,
    /// Frames sent but not yet acknowledged. Always 0 while `acks_suspended`.
    pub pending_acks: u32,
    /// Client sent `queueDepth == SUSPEND_FRAME_ACKNOWLEDGEMENT` (0xFFFFFFFF): it will not
    /// acknowledge frames until it sends a regular ack again (MS-RDPEGFX 2.2.3.3).
    pub acks_suspended: bool,
    /// Last acknowledged frame ID
    pub last_ack_frame: u32,
    /// Network quality estimate (0.0 = congested, 1.0 = excellent)
    pub network_quality: f32,
    /// Send timestamps for RTT calculation: fixed ring indexed by `frame_id % SEND_TIME_SLOTS`,
    /// each slot stores `(frame_id, send_instant)` so stale entries are detected on lookup.
    pub frame_send_times: [Option<(u32, std::time::Instant)>; SEND_TIME_SLOTS],
    /// Exponentially weighted moving average RTT in ms
    pub rtt_ewma_ms: f64,
    /// Last frame encode time in ms
    pub last_encode_ms: f64,
    /// Last frame size in bytes
    pub last_frame_bytes: u32,
    /// Total bytes sent
    pub total_bytes_sent: u64,
    /// Time of first frame sent
    pub start_time: Option<std::time::Instant>,
    /// Client peer IP address (set after TCP connection is established)
    pub peer_addr: Option<std::net::IpAddr>,
    /// Instantaneous bitrate tracking (per-frame Mbps samples since last log)
    bitrate_samples: Vec<f64>,
    bitrate_max: f64,
    bitrate_min: f64,
    last_frame_time: Option<std::time::Instant>,
    /// Hot-updatable: target bitrate override (set via ConfigUpdate)
    pub target_bitrate: Option<u32>,
    /// Hot-updatable: show cursor flag (set via ConfigUpdate)
    pub show_cursor: Option<bool>,
    /// Hot-updatable: resolution (set via ConfigUpdate)
    pub resolution: Option<String>,
    /// Hot-updatable: encoder preference (set via ConfigUpdate)
    pub encoder_pref: Option<String>,
    /// Hot-updatable: chroma mode (set via ConfigUpdate)
    pub chroma_mode: Option<String>,
}

impl GfxState {
    pub fn new(width: u16, height: u16, avc444_enabled: bool) -> Self {
        Self {
            channel_id: None,
            surface_created: false,
            caps_confirmed: false,
            frame_id: 0,
            avc420_supported: false,
            avc444_supported: false,
            avc444_enabled,
            confirmed_cap: None,
            width,
            height,
            pending_acks: 0,
            acks_suspended: false,
            last_ack_frame: 0,
            network_quality: 1.0,
            frame_send_times: [None; SEND_TIME_SLOTS],
            rtt_ewma_ms: 0.0,
            last_encode_ms: 0.0,
            last_frame_bytes: 0,
            total_bytes_sent: 0,
            start_time: None,
            peer_addr: None,
            bitrate_samples: Vec::new(),
            bitrate_max: 0.0,
            bitrate_min: f64::MAX,
            last_frame_time: None,
            target_bitrate: None,
            show_cursor: None,
            resolution: None,
            encoder_pref: None,
            chroma_mode: None,
        }
    }

    pub fn next_frame_id(&mut self) -> u32 {
        self.frame_id += 1;
        if !self.acks_suspended {
            self.pending_acks += 1;
        }
        self.total_bytes_sent += self.last_frame_bytes as u64;
        if self.start_time.is_none() {
            self.start_time = Some(std::time::Instant::now());
        }
        // Track instantaneous bitrate per frame
        if let Some(prev) = self.last_frame_time {
            let dt = prev.elapsed().as_secs_f64().max(0.0001);
            let mbps = self.last_frame_bytes as f64 * 8.0 / dt / 1_000_000.0;
            self.bitrate_samples.push(mbps);
            if mbps > self.bitrate_max { self.bitrate_max = mbps; }
            if mbps < self.bitrate_min { self.bitrate_min = mbps; }
        }
        let now = std::time::Instant::now();
        self.last_frame_time = Some(now);
        self.frame_send_times[self.frame_id as usize % SEND_TIME_SLOTS] = Some((self.frame_id, now));
        self.frame_id
    }

    /// Client stopped acknowledging frames: nothing is outstanding from its point of view, so
    /// the backlog must not keep growing (display skips frames on `pending_acks`).
    pub fn suspend_acks(&mut self) {
        self.acks_suspended = true;
        self.pending_acks = 0;
    }

    /// Update network quality and RTT based on frame acknowledgment
    pub fn ack_frame(&mut self, ack_frame_id: u32) {
        self.acks_suspended = false;
        self.last_ack_frame = ack_frame_id;
        if self.pending_acks > 0 {
            self.pending_acks -= 1;
        }

        // Calculate RTT from send timestamp
        let slot = &mut self.frame_send_times[ack_frame_id as usize % SEND_TIME_SLOTS];
        let send_time = match *slot {
            Some((id, t)) if id == ack_frame_id => {
                *slot = None;
                Some(t)
            }
            _ => None,
        };
        if let Some(send_time) = send_time {
            let rtt_ms = send_time.elapsed().as_secs_f64() * 1000.0;
            // EWMA with alpha=0.2 for smooth averaging
            if self.rtt_ewma_ms == 0.0 {
                self.rtt_ewma_ms = rtt_ms;
            } else {
                self.rtt_ewma_ms = self.rtt_ewma_ms * 0.8 + rtt_ms * 0.2;
            }
        }

        // Network quality based on RTT and ack backlog
        self.network_quality = if self.rtt_ewma_ms < 15.0 && self.pending_acks <= 2 {
            1.0 // Excellent: max bitrate
        } else if self.rtt_ewma_ms < 30.0 && self.pending_acks <= 5 {
            0.8 // Good
        } else if self.rtt_ewma_ms < 60.0 && self.pending_acks <= 10 {
            0.5 // Fair
        } else if self.rtt_ewma_ms < 100.0 {
            0.3 // Poor
        } else {
            0.15 // Congested
        };
    }

    /// Get recommended bitrate based on network quality and base bitrate
    pub fn adaptive_bitrate(&self, base_bitrate: u32) -> u32 {
        (base_bitrate as f32 * self.network_quality) as u32
    }

    /// GFX channel is open and capabilities have been negotiated.
    /// Does NOT imply H.264 support — check [`Self::avc420_supported`] before
    /// sending `GfxFrame`; otherwise use the uncompressed path.
    pub fn is_ready(&self) -> bool {
        self.negotiated()
    }

    /// Channel open + CapabilitiesConfirm sent, regardless of codec support.
    pub fn negotiated(&self) -> bool {
        self.channel_id.is_some() && self.caps_confirmed
    }

    /// Client accepted a capset without AVC_DISABLED (or V8.1 with AVC420_ENABLED).
    pub fn avc420_supported(&self) -> bool {
        self.negotiated() && self.avc420_supported
    }

    /// Client accepted a V10+ capset without AVC_DISABLED.
    pub fn avc444_supported(&self) -> bool {
        self.negotiated() && self.avc444_supported
    }

    /// Clear per-connection session state (channel, caps, frame counters, stats)
    /// while keeping configuration: size, `avc444_enabled`, `peer_addr`, and the
    /// hot-updated `target_bitrate` / `show_cursor` / `resolution` /
    /// `encoder_pref` / `chroma_mode` values.
    pub fn reset_session(&mut self) {
        let mut fresh = Self::new(self.width, self.height, self.avc444_enabled);
        fresh.peer_addr = self.peer_addr;
        fresh.target_bitrate = self.target_bitrate;
        fresh.show_cursor = self.show_cursor;
        fresh.resolution = self.resolution.take();
        fresh.encoder_pref = self.encoder_pref.take();
        fresh.chroma_mode = self.chroma_mode.take();
        *self = fresh;
    }
}

/// GFX DVC processor
pub struct GfxHandler {
    pub state: Arc<Mutex<GfxState>>,
}

impl_as_any!(GfxHandler);

impl GfxHandler {
    pub fn new(state: Arc<Mutex<GfxState>>) -> Self {
        Self { state }
    }

    /// Create a single ZGFX-wrapped buffer containing all GFX PDUs for an H.264 frame.
    /// Per MS-RDPEGFX Section 2.2.5: "The server SHOULD combine multiple RDPGFX commands
    /// into a single RDP_SEGMENTED_DATA structure."
    /// First call also includes surface setup PDUs (ResetGraphics + CreateSurface + MapSurfaceToOutput).
    ///
    /// Returns `Err` when the client did not negotiate AVC420 (every advertised capset
    /// had `AVC_DISABLED`); callers must use [`Self::create_uncompressed_pdu`] instead.
    pub fn create_frame_pdu(state: &mut GfxState, frame: &GfxFrameUpdate) -> PduResult<Vec<u8>> {
        if !state.avc420_supported {
            return Err(pdu_other_err!(
                "GfxFrame",
                "client did not negotiate AVC420 (all capsets AVC_DISABLED); use the uncompressed path"
            ));
        }

        let aux_len = frame.h264_aux.as_ref().map_or(0, |a| a.len());
        // One allocation for the whole segmented frame: ZGFX header slot + setup/frame PDUs + payload.
        let mut out = Vec::with_capacity(ZGFX_SINGLE_HEADER + frame.h264_data.len() + aux_len + 256);
        out.extend_from_slice(&[0; ZGFX_SINGLE_HEADER]);

        if Self::write_surface_setup(state, &mut out, frame.width, frame.height)? {
            info!(
                "GFX surface created: {}x{} (enc: {}x{})",
                state.width, state.height, frame.enc_width, frame.enc_height
            );
        }

        let frame_id = state.next_frame_id();
        push_pdu(&mut out, &ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp { milliseconds: 0, seconds: 0, minutes: 0, hours: 0 },
            frame_id,
        }))?;

        // RDPGFX_RECT16 exclusive bound = visible crop (also used as destination rect)
        let rect = InclusiveRectangle { left: 0, top: 0, right: frame.width, bottom: frame.height };
        let qq = QuantQuality { quantization_parameter: 22, progressive: false, quality: 100 };

        // Choose AVC444 or AVC420 path based on available data and negotiated caps
        match frame.h264_aux.as_ref().filter(|_| state.avc444_supported && state.avc444_enabled) {
            Some(aux) => {
                let stream = Avc444BitmapStream {
                    encoding: Encoding::LUMA_AND_CHROMA,
                    stream1: Avc420BitmapStream {
                        rectangles: vec![rect.clone()],
                        quant_qual_vals: vec![qq.clone()],
                        data: &frame.h264_data,
                    },
                    stream2: Some(Avc420BitmapStream {
                        rectangles: vec![rect.clone()],
                        quant_qual_vals: vec![qq],
                        data: aux,
                    }),
                };
                push_wire_to_surface1(&mut out, Codec1Type::Avc444, &rect, stream.size(), |c| stream.encode(c))?;
            }
            None => {
                let stream = Avc420BitmapStream {
                    rectangles: vec![rect.clone()],
                    quant_qual_vals: vec![qq],
                    data: &frame.h264_data,
                };
                push_wire_to_surface1(&mut out, Codec1Type::Avc420, &rect, stream.size(), |c| stream.encode(c))?;
            }
        }

        push_pdu(&mut out, &ServerPdu::EndFrame(EndFramePdu { frame_id }))?;

        debug!(
            frame_id,
            raw_bytes = out.len() - ZGFX_SINGLE_HEADER,
            h264_bytes = frame.h264_data.len(),
            "GFX frame PDU created",
        );

        Ok(finish_zgfx(out))
    }

    /// Create a ZGFX-wrapped buffer for dirty rect updates on clients without AVC420.
    /// Each rect goes out as WireToSurface1 with RDP6 planar (RLE) when that is smaller than the
    /// raw BGRA, else Codec1Type::Uncompressed. First call also includes surface setup PDUs.
    pub fn create_uncompressed_pdu(state: &mut GfxState, update: &GfxUncompressedUpdate) -> Vec<u8> {
        let total_bytes: usize = update.rects.iter().map(|r| r.pixel_data.len()).sum();
        let mut out = Vec::with_capacity(ZGFX_SINGLE_HEADER + total_bytes + 64 + update.rects.len() * 32);
        out.extend_from_slice(&[0; ZGFX_SINGLE_HEADER]);

        // Fixed-size PDUs cannot fail to encode into a correctly sized buffer; log and keep going
        // so a transient issue does not tear down the connection (matches previous behaviour).
        let log_err = |e: ironrdp_pdu::PduError| warn!("GFX uncompressed PDU encode failed: {e}");

        match Self::write_surface_setup(state, &mut out, update.width, update.height) {
            Ok(true) => info!("GFX surface created (uncompressed): {}x{}", update.width, update.height),
            Ok(false) => {}
            Err(e) => log_err(e),
        }

        // Committed by `next_frame_id()` below, once the on-wire bitmap size is known.
        let frame_id = state.frame_id + 1;
        if let Err(e) = push_pdu(&mut out, &ServerPdu::StartFrame(StartFramePdu {
            timestamp: Timestamp { milliseconds: 0, seconds: 0, minutes: 0, hours: 0 },
            frame_id,
        })) {
            log_err(e);
        }

        // WireToSurface1 per dirty rect: RDP6 planar when RLE wins, else raw BGRA.
        let mut planar = Vec::new();
        let mut bitmap_bytes = 0usize;
        for rect in &update.rects {
            let dest = InclusiveRectangle {
                left: rect.x,
                top: rect.y,
                right: rect.x + rect.width,
                bottom: rect.y + rect.height,
            };
            let res = match planar_encode(rect, &mut planar) {
                Some(n) => {
                    bitmap_bytes += n;
                    push_wire_to_surface1(&mut out, Codec1Type::Planar, &dest, n, |c| {
                        c.write_slice(&planar[..n]);
                        Ok(())
                    })
                }
                None => {
                    bitmap_bytes += rect.pixel_data.len();
                    push_wire_to_surface1(&mut out, Codec1Type::Uncompressed, &dest, rect.pixel_data.len(), |c| {
                        c.write_slice(&rect.pixel_data);
                        Ok(())
                    })
                }
            };
            if let Err(e) = res {
                log_err(e);
            }
        }

        if let Err(e) = push_pdu(&mut out, &ServerPdu::EndFrame(EndFramePdu { frame_id })) {
            log_err(e);
        }

        state.last_frame_bytes = bitmap_bytes as u32;
        let committed = state.next_frame_id();
        debug_assert_eq!(committed, frame_id);

        debug!(
            frame_id,
            rects = update.rects.len(),
            bitmap_bytes,
            "GFX uncompressed frame PDU created",
        );

        finish_zgfx(out)
    }

    /// Pick the best capset the client advertised, record AVC support, and build the
    /// CapabilitiesConfirm reply. `skipped` = capsets dropped by the lenient parser (log only).
    fn negotiate_caps(&self, cap_sets: &[CapabilitySet], skipped: usize) -> PduResult<Vec<DvcMessage>> {
        let mut state = self.state.lock().unwrap();

        // If already confirmed, ignore duplicate CapabilitiesAdvertise
        if state.caps_confirmed {
            info!("GFX caps already confirmed, ignoring duplicate CapabilitiesAdvertise");
            return Ok(Vec::new());
        }

        let mut best_cap = None;

        for cap in cap_sets {
            match cap {
                CapabilitySet::V10_7 { flags } if !flags.contains(CapabilitiesV107Flags::AVC_DISABLED) => {
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    best_cap = Some(cap.clone());
                    break;
                }
                CapabilitySet::V10_6 { flags } | CapabilitySet::V10_6Err { flags }
                    if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
                {
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    best_cap = Some(cap.clone());
                }
                CapabilitySet::V10_5 { flags } | CapabilitySet::V10_4 { flags }
                    if !flags.contains(CapabilitiesV104Flags::AVC_DISABLED) =>
                {
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
                CapabilitySet::V10_3 { flags }
                    if !flags.contains(CapabilitiesV103Flags::AVC_DISABLED) =>
                {
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
                CapabilitySet::V10_2 { flags } | CapabilitySet::V10 { flags }
                    if !flags.contains(CapabilitiesV10Flags::AVC_DISABLED) =>
                {
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
                CapabilitySet::V10_1 => {
                    // V10_1 has no AVC_DISABLED flag — always supports AVC
                    state.avc420_supported = true;
                    state.avc444_supported = true;
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
                CapabilitySet::V8_1 { flags }
                    if flags.contains(CapabilitiesV81Flags::AVC420_ENABLED) =>
                {
                    state.avc420_supported = true;
                    // V8.1 only supports AVC420, not AVC444
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
                _ => {
                    if best_cap.is_none() { best_cap = Some(cap.clone()); }
                }
            }
        }

        let confirmed = best_cap.unwrap_or(CapabilitySet::V8 {
            flags: ironrdp_pdu::rdp::vc::dvc::gfx::CapabilitiesV8Flags::empty(),
        });

        // One line with everything an issue reporter needs: every advertised capset
        // with raw flags, how many were skipped, what we confirmed, and codec support.
        let advertised: Vec<String> = cap_sets.iter().map(describe_cap).collect();
        info!(
            advertised = %advertised.join(","),
            skipped_unknown = skipped,
            confirmed = %describe_cap(&confirmed),
            avc420 = state.avc420_supported,
            avc444_client = state.avc444_supported,
            avc444_enabled = state.avc444_enabled,
            "GFX capabilities negotiated"
        );
        if !state.avc420_supported {
            warn!(
                "GFX: client advertised no AVC420-capable capset (AVC_DISABLED everywhere); \
                 H.264 frames will be rejected, only the uncompressed GFX path is usable"
            );
        }

        // Send CapabilitiesConfirm from the DVC handler so it goes through
        // DrdynvcServer's proper encoding path. Bitmaps are suppressed once
        // GFX channel is open, so no bitmap/GFX mixing will occur.
        state.confirmed_cap = Some(confirmed.clone());
        state.caps_confirmed = true;

        let confirm_pdu = ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(confirmed));
        let msg = make_dvc_message(&confirm_pdu)?;
        info!("GFX CapabilitiesConfirm sent via DVC handler");
        Ok(vec![msg])
    }

    /// Emit ResetGraphics + CreateSurface + MapSurfaceToOutput when no surface exists or the
    /// desktop size changed. Returns `Ok(true)` when setup PDUs were written.
    fn write_surface_setup(state: &mut GfxState, out: &mut Vec<u8>, width: u16, height: u16) -> PduResult<bool> {
        if state.surface_created && (state.width != width || state.height != height) {
            info!(
                old_w = state.width, old_h = state.height, new_w = width, new_h = height,
                "GFX resolution changed, rebuilding surface"
            );
            state.surface_created = false;
        }
        if state.surface_created {
            return Ok(false);
        }
        state.width = width;
        state.height = height;

        push_pdu(out, &ServerPdu::ResetGraphics(ResetGraphicsPdu {
            width: u32::from(width),
            height: u32::from(height),
            monitors: vec![Monitor {
                left: 0,
                top: 0,
                right: i32::from(width) - 1,
                bottom: i32::from(height) - 1,
                flags: MonitorFlags::PRIMARY,
            }],
        }))?;
        push_pdu(out, &ServerPdu::CreateSurface(CreateSurfacePdu {
            surface_id: 0,
            width,
            height,
            pixel_format: GfxPixelFormat::XRgb,
        }))?;
        push_pdu(out, &ServerPdu::MapSurfaceToOutput(MapSurfaceToOutputPdu {
            surface_id: 0,
            output_origin_x: 0,
            output_origin_y: 0,
        }))?;

        state.surface_created = true;
        Ok(true)
    }
}

/// Encode a server PDU straight into `out` (no intermediate Vec).
fn push_pdu(out: &mut Vec<u8>, pdu: &ServerPdu) -> PduResult<()> {
    let start = out.len();
    out.resize(start + pdu.size(), 0);
    let mut cur = WriteCursor::new(&mut out[start..]);
    pdu.encode(&mut cur).map_err(|e| encode_err!(e))
}

/// RDP6 planar (RLE, no alpha) encode of a tightly packed BGRA rect into `buf`. Every GFX client
/// must support Codec1Type::Planar (MS-RDPEGFX 2.2.4.4), so no capability check is needed.
/// `buf` is capped one byte below the raw size so the encoder errors out instead of producing a
/// stream that is not strictly smaller; `None` means "send it uncompressed".
fn planar_encode(rect: &UncompressedRect, buf: &mut Vec<u8>) -> Option<usize> {
    buf.resize(rect.pixel_data.len().saturating_sub(1), 0);
    BitmapStreamEncoder::new(usize::from(rect.width), usize::from(rect.height))
        .encode_bitmap::<BgrAChannels>(&rect.pixel_data, buf, true)
        .ok()
}

/// Encode RDPGFX_WIRE_TO_SURFACE_PDU_1 straight into `out`, letting `write_bitmap` stream the
/// codec payload into place. Avoids the `bitmap_data: Vec<u8>` copy that `WireToSurface1Pdu` requires.
fn push_wire_to_surface1(
    out: &mut Vec<u8>,
    codec_id: Codec1Type,
    dest: &InclusiveRectangle,
    bitmap_len: usize,
    write_bitmap: impl FnOnce(&mut WriteCursor<'_>) -> EncodeResult<()>,
) -> PduResult<()> {
    const RDPGFX_HEADER: usize = 8;
    const W2S1_FIXED: usize = 2 /* surfaceId */ + 2 /* codecId */ + 1 /* pixelFormat */ + 8 /* destRect */ + 4 /* bitmapDataLength */;
    let pdu_len = RDPGFX_HEADER + W2S1_FIXED + bitmap_len;
    let pdu_len_u32 = u32::try_from(pdu_len).map_err(|_| pdu_other_err!("GfxFrame", "WireToSurface1 too large"))?;
    let start = out.len();
    out.resize(start + pdu_len, 0);
    let mut cur = WriteCursor::new(&mut out[start..]);
    cur.write_u16(0x0001); // RDPGFX_CMDID_WIRETOSURFACE_1
    cur.write_u16(0); // flags
    cur.write_u32(pdu_len_u32);
    cur.write_u16(0); // surfaceId
    cur.write_u16(codec_id as u16);
    cur.write_u8(GfxPixelFormat::XRgb as u8);
    dest.encode(&mut cur).map_err(|e| encode_err!(e))?;
    cur.write_u32(pdu_len_u32 - (RDPGFX_HEADER + W2S1_FIXED) as u32);
    write_bitmap(&mut cur).map_err(|e| encode_err!(e))?;
    Ok(())
}

impl DvcProcessor for GfxHandler {
    fn channel_name(&self) -> &str {
        GFX_CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        info!(channel_id, "GFX channel opened");
        let mut state = self.state.lock().unwrap();
        state.channel_id = Some(channel_id);
        Ok(Vec::new())
    }

    fn process(&mut self, _channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        // Client GFX data is also ZGFX-wrapped. Unwrap the ZGFX layer first.
        let raw_data = unwrap_zgfx(payload);
        let data = raw_data.as_deref().unwrap_or(payload);

        let client_pdu: ClientPdu = match decode(data) {
            Ok(pdu) => pdu,
            Err(e) => {
                // ironrdp-pdu 0.7 rejects the whole CapabilitiesAdvertise when any capset has a
                // version it does not know (V10_8+ from recent mstsc / MRD / Android clients).
                // Re-parse leniently so negotiation still succeeds on the versions we do know.
                if let Some((caps, skipped)) = parse_caps_advertise_lenient(data) {
                    warn!(
                        skipped,
                        known = caps.len(),
                        "GFX: CapabilitiesAdvertise contained capsets ironrdp cannot decode ({e}); negotiating on the known ones"
                    );
                    return self.negotiate_caps(&caps, skipped);
                }
                // Unknown PDU type (e.g., QoE FrameAcknowledge 0x16, CacheImportOffer 0x10)
                // Log and ignore rather than crashing the connection
                debug!(
                    payload_len = data.len(),
                    first_bytes = ?&data[..data.len().min(8)],
                    "GFX: ignoring unknown client PDU: {e}"
                );
                return Ok(Vec::new());
            }
        };

        match client_pdu {
            ClientPdu::CapabilitiesAdvertise(caps) => self.negotiate_caps(&caps.0, 0),

            ClientPdu::FrameAcknowledge(ack) => {
                let mut state = self.state.lock().unwrap();
                if ack.queue_depth == QueueDepth::Suspend {
                    debug!(frame_id = ack.frame_id, "GFX: client suspended frame acknowledgements");
                    state.suspend_acks();
                    return Ok(Vec::new());
                }
                state.ack_frame(ack.frame_id);

                // Log stats every 60 acked frames (~1 second at 60fps)
                if ack.frame_id % 60 == 0 {
                    let net_ms = (state.rtt_ewma_ms - state.last_encode_ms).max(0.0);

                    // Compute instant bitrate stats from samples since last log
                    let n = state.bitrate_samples.len() as f64;
                    let (inst_avg, inst_std) = if n > 0.0 {
                        let sum: f64 = state.bitrate_samples.iter().sum();
                        let avg = sum / n;
                        let var: f64 = state.bitrate_samples.iter().map(|x| (x - avg).powi(2)).sum::<f64>() / n;
                        (avg, var.sqrt())
                    } else {
                        (0.0, 0.0)
                    };
                    let inst_max = if state.bitrate_max > 0.0 { state.bitrate_max } else { 0.0 };
                    let inst_min = if state.bitrate_min < f64::MAX { state.bitrate_min } else { 0.0 };

                    info!(
                        "RTT {:.1}ms | encode {:.1}ms | net {:.1}ms | instant {:.1}/{:.1}/{:.1} Mbps (avg/max/min) | std {:.1} | {}KB/f | {} pending",
                        state.rtt_ewma_ms,
                        state.last_encode_ms,
                        net_ms,
                        inst_avg,
                        inst_max,
                        inst_min,
                        inst_std,
                        state.last_frame_bytes / 1024,
                        state.pending_acks,
                    );

                    // Reset for next window
                    state.bitrate_samples.clear();
                    state.bitrate_max = 0.0;
                    state.bitrate_min = f64::MAX;
                }
                Ok(Vec::new())
            }
        }
    }

    fn close(&mut self, channel_id: u32) {
        info!(channel_id, "GFX channel closed");
        let mut state = self.state.lock().unwrap();
        state.channel_id = None;
        state.surface_created = false;
        state.caps_confirmed = false;
        state.avc420_supported = false;
        state.avc444_supported = false;
        state.confirmed_cap = None;
    }
}

impl DvcServerProcessor for GfxHandler {}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use ironrdp_core::{Decode as _, ReadCursor};
    use ironrdp_pdu::rdp::vc::dvc::gfx::{CapabilitiesV8Flags, CapabilitiesV107Flags};

    use super::*;

    /// Build a raw RDPGFX_CAPS_ADVERTISE (cmdId 0x12) from `(version, flags)` pairs.
    fn caps_advertise(sets: &[(u32, u32)]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&(sets.len() as u16).to_le_bytes());
        for (version, flags) in sets {
            body.extend_from_slice(&version.to_le_bytes());
            body.extend_from_slice(&4u32.to_le_bytes());
            body.extend_from_slice(&flags.to_le_bytes());
        }
        let mut pdu = vec![0x12, 0x00, 0x00, 0x00];
        pdu.extend_from_slice(&((8 + body.len()) as u32).to_le_bytes());
        pdu.extend_from_slice(&body);
        pdu
    }

    fn handler() -> (GfxHandler, Arc<Mutex<GfxState>>) {
        let state = Arc::new(Mutex::new(GfxState::new(1920, 1080, false)));
        let mut h = GfxHandler::new(Arc::clone(&state));
        h.start(7).unwrap();
        (h, state)
    }

    fn decode_all(mut data: &[u8]) -> Vec<ServerPdu> {
        let mut out = Vec::new();
        while !data.is_empty() {
            let mut cur = ReadCursor::new(data);
            out.push(ServerPdu::decode(&mut cur).unwrap());
            data = &data[cur.pos()..];
        }
        out
    }

    #[test]
    fn unknown_capset_version_still_negotiates_known_v10_7() {
        const V10_8_LIKE: u32 = 0xA_0800;
        const V10_7: u32 = 0xA_0701;
        let pdu = caps_advertise(&[(V10_8_LIKE, 0), (V10_7, 0)]);

        // Document the ironrdp-pdu 0.7 behaviour we are working around.
        assert!(decode::<ClientPdu>(&pdu).is_err(), "ironrdp-pdu now decodes V10_8; fallback can go");

        let (mut h, state) = handler();
        let reply = h.process(7, &wrap_zgfx(&pdu)).unwrap();
        assert_eq!(reply.len(), 1, "exactly one CapabilitiesConfirm");

        let s = state.lock().unwrap();
        assert!(s.negotiated());
        assert!(s.avc420_supported());
        assert!(s.avc444_supported());
        assert_eq!(
            s.confirmed_cap,
            Some(CapabilitySet::V10_7 { flags: CapabilitiesV107Flags::empty() })
        );

        // The reply really is a ZGFX-wrapped CapabilitiesConfirm carrying V10_7.
        let bytes = encode_vec(reply[0].as_ref()).unwrap();
        let raw = unwrap_zgfx(&bytes).unwrap();
        match decode::<ServerPdu>(&raw).unwrap() {
            ServerPdu::CapabilitiesConfirm(CapabilitiesConfirmPdu(cap)) => {
                assert_eq!(cap, CapabilitySet::V10_7 { flags: CapabilitiesV107Flags::empty() });
            }
            other => panic!("unexpected reply {other:?}"),
        }
    }

    #[test]
    fn only_unknown_capsets_fall_back_to_v8() {
        let pdu = caps_advertise(&[(0xA_0800, 0), (0xA_0900, 0)]);
        let (mut h, state) = handler();
        assert_eq!(h.process(7, &wrap_zgfx(&pdu)).unwrap().len(), 1);
        let s = state.lock().unwrap();
        assert!(s.negotiated());
        assert!(!s.avc420_supported());
        assert_eq!(s.confirmed_cap, Some(CapabilitySet::V8 { flags: CapabilitiesV8Flags::empty() }));
    }

    #[test]
    fn avc_disabled_everywhere_is_ready_but_rejects_h264() {
        let pdu = caps_advertise(&[(0xA_0701, CapabilitiesV107Flags::AVC_DISABLED.bits())]);
        let (mut h, state) = handler();
        h.process(7, &wrap_zgfx(&pdu)).unwrap();

        let mut s = state.lock().unwrap();
        assert!(s.is_ready(), "negotiated => ready, even without AVC");
        assert!(!s.avc420_supported());

        let frame = GfxFrameUpdate {
            h264_data: Bytes::from_static(&[0, 0, 0, 1, 0x65]),
            width: 1920,
            height: 1080,
            enc_width: 1920,
            enc_height: 1088,
            is_keyframe: true,
            h264_aux: None,
        };
        assert!(GfxHandler::create_frame_pdu(&mut s, &frame).is_err());
        assert_eq!(s.frame_id, 0, "rejected frame must not consume a frame id");

        // Uncompressed path still works and performs surface setup.
        let update = GfxUncompressedUpdate {
            rects: vec![UncompressedRect { x: 1, y: 2, width: 2, height: 1, pixel_data: Bytes::from_static(&[9; 8]) }],
            width: 1920,
            height: 1080,
        };
        let data = GfxHandler::create_uncompressed_pdu(&mut s, &update);
        let pdus = decode_all(&unwrap_zgfx(&data).unwrap());
        assert!(matches!(pdus[0], ServerPdu::ResetGraphics(_)));
        assert!(matches!(pdus[1], ServerPdu::CreateSurface(_)));
        assert!(matches!(pdus[2], ServerPdu::MapSurfaceToOutput(_)));
        assert!(matches!(pdus[3], ServerPdu::StartFrame(_)));
        match &pdus[4] {
            ServerPdu::WireToSurface1(w) => {
                assert_eq!(w.codec_id, Codec1Type::Uncompressed);
                assert_eq!(w.destination_rectangle, InclusiveRectangle { left: 1, top: 2, right: 3, bottom: 3 });
                assert_eq!(w.bitmap_data, vec![9; 8]);
            }
            other => panic!("expected WireToSurface1, got {other:?}"),
        }
        assert!(matches!(pdus[5], ServerPdu::EndFrame(_)));
    }

    /// Deterministic pseudo-random BGRA (xorshift32) so the planar tests need no rand dep.
    fn noise_bgra(pixels: usize, mut seed: u32) -> Vec<u8> {
        (0..pixels * 4)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect()
    }

    fn rect_64x48(pixel_data: Vec<u8>) -> UncompressedRect {
        UncompressedRect { x: 3, y: 5, width: 64, height: 48, pixel_data: Bytes::from(pixel_data) }
    }

    /// Decode a planar stream and compare RGB against the BGRA source (alpha ignored).
    fn assert_planar_matches(planar: &[u8], bgra: &[u8], width: usize, height: usize) {
        let mut rgb = Vec::new();
        ironrdp_graphics::rdp6::BitmapStreamDecoder::default()
            .decode_bitmap_stream_to_rgb24(planar, &mut rgb, width, height)
            .unwrap();
        assert_eq!(rgb.len(), width * height * 3);
        for (i, px) in bgra.chunks_exact(4).enumerate() {
            assert_eq!(&rgb[i * 3..i * 3 + 3], &[px[2], px[1], px[0]], "pixel {i}");
        }
    }

    #[test]
    fn planar_rect_round_trips_through_ironrdp_decoder() {
        let bgra = noise_bgra(64 * 48, 0x9E37_79B9);
        let rect = rect_64x48(bgra.clone());
        let mut buf = Vec::new();
        let n = planar_encode(&rect, &mut buf).expect("planar must beat raw BGRA (drops alpha)");
        assert!(n < bgra.len());
        assert_planar_matches(&buf[..n], &bgra, 64, 48);
    }

    #[test]
    fn uncompressed_pdu_uses_planar_when_smaller() {
        let pdu = caps_advertise(&[(0xA_0701, CapabilitiesV107Flags::AVC_DISABLED.bits())]);
        let (mut h, state) = handler();
        h.process(7, &wrap_zgfx(&pdu)).unwrap();
        let mut s = state.lock().unwrap();

        let bgra = noise_bgra(64 * 48, 0x1234_5678);
        let update = GfxUncompressedUpdate { rects: vec![rect_64x48(bgra.clone())], width: 1920, height: 1080 };
        let data = GfxHandler::create_uncompressed_pdu(&mut s, &update);
        let pdus = decode_all(&unwrap_zgfx(&data).unwrap());
        match &pdus[4] {
            ServerPdu::WireToSurface1(w) => {
                assert_eq!(w.codec_id, Codec1Type::Planar);
                assert_eq!(w.destination_rectangle, InclusiveRectangle { left: 3, top: 5, right: 67, bottom: 53 });
                assert!(w.bitmap_data.len() < bgra.len());
                assert_eq!(s.last_frame_bytes as usize, w.bitmap_data.len(), "stats must see on-wire bytes");
                assert_planar_matches(&w.bitmap_data, &bgra, 64, 48);
            }
            other => panic!("expected WireToSurface1, got {other:?}"),
        }
        assert_eq!(s.frame_id, 1);
    }

    #[test]
    fn planar_solid_rect_is_much_smaller_than_raw() {
        let bgra: Vec<u8> = [0x10, 0x80, 0xF0, 0xFF].repeat(64 * 48);
        let rect = rect_64x48(bgra.clone());
        let mut buf = Vec::new();
        let n = planar_encode(&rect, &mut buf).unwrap();
        assert!(n * 8 < bgra.len(), "planar {n} B vs raw {} B", bgra.len());
        assert_planar_matches(&buf[..n], &bgra, 64, 48);
    }

    #[test]
    fn h264_frame_pdu_round_trips_through_ironrdp_decoder() {
        let pdu = caps_advertise(&[(0xA_0701, 0)]);
        let (mut h, state) = handler();
        h.process(7, &wrap_zgfx(&pdu)).unwrap();
        let mut s = state.lock().unwrap();

        let h264 = vec![0xAB; 70_000]; // > one ZGFX segment → exercises multipart
        let frame = GfxFrameUpdate {
            h264_data: Bytes::from(h264.clone()),
            width: 1272,
            height: 2160,
            enc_width: 1280,
            enc_height: 2160,
            is_keyframe: true,
            h264_aux: None,
        };
        let data = GfxHandler::create_frame_pdu(&mut s, &frame).unwrap();
        assert_eq!(data[0], ZGFX_MULTIPART);
        let pdus = decode_all(&unwrap_zgfx(&data).unwrap());
        assert_eq!(pdus.len(), 6);
        match &pdus[4] {
            ServerPdu::WireToSurface1(w) => {
                assert_eq!(w.codec_id, Codec1Type::Avc420);
                assert_eq!(w.destination_rectangle, InclusiveRectangle { left: 0, top: 0, right: 1272, bottom: 2160 });
                let stream = decode::<Avc420BitmapStream<'_>>(&w.bitmap_data).unwrap();
                assert_eq!(stream.rectangles, vec![InclusiveRectangle { left: 0, top: 0, right: 1272, bottom: 2160 }]);
                assert_eq!(stream.data, &h264[..]);
            }
            other => panic!("expected WireToSurface1, got {other:?}"),
        }
        assert_eq!(s.frame_id, 1);
        assert!(s.surface_created);

        // Second frame: no surface setup, single segment.
        let small = GfxFrameUpdate { h264_data: Bytes::from_static(&[1, 2, 3]), is_keyframe: false, ..frame };
        let data = GfxHandler::create_frame_pdu(&mut s, &small).unwrap();
        assert_eq!(&data[..2], &[ZGFX_SINGLE, ZGFX_UNCOMPRESSED]);
        assert_eq!(decode_all(&data[2..]).len(), 3);
    }

    #[test]
    fn unwrap_zgfx_rejects_malformed_multipart_without_panicking() {
        // seg_count=1, uncompressed_size=0, segmentSize=0 (client-controlled, was a slice panic)
        let zero = [0xE1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x04];
        assert_eq!(unwrap_zgfx(&zero), None);
        // segmentSize claims 100 bytes but only 2 follow
        let truncated = [0xE1, 1, 0, 0, 0, 0, 0, 100, 0, 0, 0, 0x04, 1, 2];
        assert_eq!(unwrap_zgfx(&truncated), None);
        // segmentSize = u32::MAX must not overflow the offset arithmetic
        let huge = [0xE1, 1, 0, 0, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0x04, 1];
        assert_eq!(unwrap_zgfx(&huge), None);
        // Valid single and multipart round-trips
        assert_eq!(unwrap_zgfx(&wrap_zgfx(b"hello")).unwrap(), b"hello");
        let big = vec![7u8; 150_000];
        assert_eq!(unwrap_zgfx(&wrap_zgfx(&big)).unwrap(), big);
    }

    #[test]
    fn reset_session_keeps_hot_updated_fields() {
        let mut s = GfxState::new(800, 600, true);
        s.channel_id = Some(3);
        s.caps_confirmed = true;
        s.avc420_supported = true;
        s.frame_id = 42;
        s.target_bitrate = Some(5_000_000);
        s.show_cursor = Some(false);
        s.encoder_pref = Some("software".into());
        s.chroma_mode = Some("avc444".into());
        s.resolution = Some("1280x720".into());
        s.peer_addr = Some(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

        s.reset_session();
        assert!(!s.negotiated());
        assert_eq!(s.frame_id, 0);
        assert_eq!((s.width, s.height, s.avc444_enabled), (800, 600, true));
        assert_eq!(s.target_bitrate, Some(5_000_000));
        assert_eq!(s.show_cursor, Some(false));
        assert_eq!(s.encoder_pref.as_deref(), Some("software"));
        assert_eq!(s.chroma_mode.as_deref(), Some("avc444"));
        assert_eq!(s.resolution.as_deref(), Some("1280x720"));
        assert!(s.peer_addr.is_some());
    }

    #[test]
    fn frame_ack_ring_tracks_rtt_only_for_matching_ids() {
        let mut s = GfxState::new(1, 1, false);
        for _ in 0..300 {
            s.next_frame_id();
        }
        assert_eq!(s.pending_acks, 300);
        s.ack_frame(300);
        assert!(s.rtt_ewma_ms >= 0.0);
        assert_eq!(s.pending_acks, 299);
        // Slot for id 44 was overwritten by 172 and 300; a stale ack must not read it.
        let before = s.rtt_ewma_ms;
        s.ack_frame(44);
        assert_eq!(s.rtt_ewma_ms, before);
    }

    #[test]
    fn suspend_ack_clears_backlog_and_stops_counting_until_resume() {
        let mut s = GfxState::new(1, 1, false);
        for _ in 0..5 {
            s.next_frame_id();
        }
        assert_eq!(s.pending_acks, 5);
        s.suspend_acks();
        assert!(s.acks_suspended);
        assert_eq!(s.pending_acks, 0);
        for _ in 0..100 {
            s.next_frame_id();
        }
        assert_eq!(s.pending_acks, 0);
        // A regular ack resumes counting.
        s.ack_frame(105);
        assert!(!s.acks_suspended);
        s.next_frame_id();
        assert_eq!(s.pending_acks, 1);
    }

    #[test]
    fn suspend_pdu_through_handler() {
        let (mut h, state) = handler();
        state.lock().unwrap().next_frame_id();
        // RDPGFX_FRAME_ACKNOWLEDGE_PDU: cmdId 0x0D, flags, pduLength 20, queueDepth, frameId, totalFramesDecoded
        let mut pdu = vec![0x0D, 0x00, 0x00, 0x00, 20, 0, 0, 0];
        pdu.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        pdu.extend_from_slice(&1u32.to_le_bytes());
        pdu.extend_from_slice(&1u32.to_le_bytes());
        h.process(7, &pdu).unwrap();
        let s = state.lock().unwrap();
        assert!(s.acks_suspended);
        assert_eq!(s.pending_acks, 0);
    }
}
