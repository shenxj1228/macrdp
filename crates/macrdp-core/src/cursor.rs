//! System cursor → RDP pointer PDU.
//!
//! Polls `NSCursor.currentSystemCursor` on a std thread and emits an `RGBAPointer` whenever the
//! shape/hotspot changes, so the client draws the cursor locally instead of the server
//! compositing it into every video frame.

use ironrdp_server::RGBAPointer;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::mpsc;

/// Server-side cap advertised via `LargePointer::UP_TO_96X96_PIXELS`. The per-session limit
/// passed to [`CursorWatcher::spawn`] comes from the client's own LargePointer capset
/// (32 when absent) and may be lower.
pub const MAX_POINTER_PX: usize = 96;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Convert a premultiplied, top-down BGRA bitmap (CGBitmapContext output) into the RDP
/// NewPointer layout: straight-alpha BGRA, rows bottom-up, tightly packed.
pub(crate) fn premul_topdown_to_rdp(src: &[u8], width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height * 4);
    for row in (0..height).rev() {
        let line = &src[row * stride..row * stride + width * 4];
        for px in line.chunks_exact(4) {
            let a = px[3] as u32;
            if a == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let un = |c: u8| ((c as u32 * 255 + a / 2) / a).min(255) as u8;
                out.extend_from_slice(&[un(px[0]), un(px[1]), un(px[2]), a as u8]);
            }
        }
    }
    out
}

fn pointer_hash(p: &RGBAPointer, scale: f64) -> u64 {
    let mut h = DefaultHasher::new();
    (p.width, p.height, p.hot_x, p.hot_y).hash(&mut h);
    p.data.hash(&mut h);
    scale.to_bits().hash(&mut h);
    h.finish()
}

/// Background poller for system cursor changes.
pub struct CursorWatcher {
    rx: mpsc::Receiver<RGBAPointer>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CursorWatcher {
    /// `scale` = RDP pixels per macOS logical point; `max_size` = largest side the client
    /// accepts (`GfxState::max_pointer_size`). The current cursor is sent immediately.
    pub fn spawn(scale: f64, max_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("cursor-watch".into())
            .spawn(move || {
                let mut last = 0u64;
                // Exits only on stop or once the receiver is gone; a nil cursor (lock screen,
                // hidden cursor) just skips this poll.
                while !stop_t.load(Ordering::Relaxed) && !tx.is_closed() {
                    // AppKit autoreleases the image/rep objects; drain them every poll.
                    let ptr = objc2::rc::autoreleasepool(|_| grab_system_cursor(scale, max_size));
                    if let Some(ptr) = ptr {
                        let h = pointer_hash(&ptr, scale);
                        if h != last {
                            last = h;
                            tracing::debug!(w = ptr.width, h = ptr.height, hot_x = ptr.hot_x, hot_y = ptr.hot_y, "cursor changed");
                            // Full channel = consumer still holds an older shape; the newest wins
                            // on the next poll since `last` is only advanced here.
                            if tx.try_send(ptr).is_err() {
                                last = 0;
                            }
                        }
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            })
            .expect("spawn cursor-watch thread");
        Self { rx, stop, thread: Some(thread) }
    }

    /// Next changed pointer; `None` once the poller has exited.
    pub async fn changed(&mut self) -> Option<RGBAPointer> {
        self.rx.recv().await
    }
}

impl Drop for CursorWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Shrink `w`×`h` uniformly so neither side exceeds `max_size`; returns the integer size and
/// the applied factor (1.0 when it already fits).
fn fit_pointer(w: f64, h: f64, max_size: usize) -> (usize, usize, f64) {
    let max = max_size.max(1) as f64;
    let fit = (max / w).min(max / h).min(1.0);
    ((w * fit).round().max(1.0) as usize, (h * fit).round().max(1.0) as usize, fit)
}

/// Snapshot the current system cursor as an RDP pointer, rasterised at `scale` and clamped to
/// `max_size` on each side (hotspot scaled alongside).
#[allow(deprecated)]
fn grab_system_cursor(scale: f64, max_size: usize) -> Option<RGBAPointer> {
    use core_graphics::base::{kCGBitmapByteOrder32Little, kCGImageAlphaPremultipliedFirst};
    use core_graphics::color_space::CGColorSpace;
    use core_graphics::context::{CGContext, CGInterpolationQuality};
    use core_graphics::geometry::{CGPoint, CGRect, CGSize};
    use foreign_types::ForeignType;
    use objc2::rc::Retained;
    use objc2_app_kit::NSCursor;

    let cursor = NSCursor::currentSystemCursor()?;
    let image = cursor.image();
    let size = image.size();
    let hot = cursor.hotSpot();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let want_w = size.width * scale;

    // Pick the representation whose pixel width is closest to the requested backing size.
    let reps = image.representations();
    let rep = reps
        .iter()
        .min_by_key(|r| (r.pixelsWide() as f64 - want_w).abs() as i64)?;
    // SAFETY: null dest rect and no context/hints are documented as valid.
    let cg = unsafe { rep.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    // Retained::into_raw hands over the +1 the Retained held; core_graphics::CGImage releases on drop.
    let cg = unsafe {
        core_graphics::image::CGImage::from_ptr(Retained::into_raw(cg) as *mut core_graphics::sys::CGImage)
    };

    // Target size in RDP pixels, shrunk uniformly if it exceeds the client's cap.
    let (wi, hi, fit) = fit_pointer(want_w.round().max(1.0), (size.height * scale).round().max(1.0), max_size);
    let (w, h) = (wi as f64, hi as f64);
    let stride = wi * 4;
    let mut buf = vec![0u8; stride * hi];
    {
        let ctx = CGContext::create_bitmap_context(
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            wi,
            hi,
            8,
            stride,
            &CGColorSpace::create_device_rgb(),
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little, // BGRA in memory
        );
        ctx.set_interpolation_quality(CGInterpolationQuality::CGInterpolationQualityHigh);
        ctx.draw_image(CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(w, h)), &cg);
        ctx.flush();
    }
    // CGBitmapContext memory is top-down (first row = top scanline); RDP wants bottom-up.
    let data = premul_topdown_to_rdp(&buf, wi, hi, stride);

    let hs = scale * fit;
    Some(RGBAPointer {
        width: wi as u16,
        height: hi as u16,
        hot_x: ((hot.x * hs).round().max(0.0) as usize).min(wi - 1) as u16,
        hot_y: ((hot.y * hs).round().max(0.0) as usize).min(hi - 1) as u16,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpremultiply_and_flip() {
        // 2x2, stride 8, top-down. Row 0: (a=128,b=64) then opaque white. Row 1: a=0 then red.
        let src = [
            64, 0, 0, 128,   255, 255, 255, 255,
            9, 9, 9, 0,      0, 0, 255, 255,
        ];
        let out = premul_topdown_to_rdp(&src, 2, 2, 8);
        assert_eq!(out.len(), 2 * 2 * 4);
        // first output row is source row 1 (flipped)
        assert_eq!(&out[0..4], &[0, 0, 0, 0], "a=0 pixel must be zeroed");
        assert_eq!(&out[4..8], &[0, 0, 255, 255]);
        let b = out[8];
        assert!((127..=128).contains(&b), "un-premultiplied b={b}");
        assert_eq!(&out[9..12], &[0, 0, 128]);
        assert_eq!(&out[12..16], &[255, 255, 255, 255]);
    }

    #[test]
    fn fit_to_client_max_pointer_size() {
        // Retina arrow (34x46) vs a client without LargePointer: must shrink to <= 32 per side.
        let (w, h, fit) = fit_pointer(34.0, 46.0, 32);
        assert!(w <= 32 && h <= 32, "{w}x{h}");
        assert_eq!(h, 32);
        assert!((fit - 32.0 / 46.0).abs() < 1e-9);
        // The rasterised buffer feeds straight into the xor mask: tightly packed w*h*4.
        let xor = premul_topdown_to_rdp(&vec![0u8; w * h * 4], w, h, w * 4);
        assert_eq!(xor.len(), w * h * 4);
        // Fits under 96: untouched.
        assert_eq!(fit_pointer(34.0, 46.0, 96), (34, 46, 1.0));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn grab_live_cursor() {
        match grab_system_cursor(2.0, MAX_POINTER_PX) {
            Some(p) => {
                println!("cursor {}x{} hot=({},{}) bytes={}", p.width, p.height, p.hot_x, p.hot_y, p.data.len());
                assert_eq!(p.data.len(), p.width as usize * 4 * p.height as usize);
                assert!(p.width as usize <= MAX_POINTER_PX && p.height as usize <= MAX_POINTER_PX);
                assert!(p.hot_x < p.width && p.hot_y < p.height);
                assert!(p.data.iter().skip(3).step_by(4).any(|&a| a > 0), "cursor fully transparent");
            }
            None => println!("skip: no system cursor available"),
        }
    }
}
