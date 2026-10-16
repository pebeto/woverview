//! Shrinks live frames to roughly the size they're shown at.
//!
//! A 4K window updating 30 times a second is a lot of pixels to hand back to
//! the compositor, and the thumbnail shows maybe a third of them; scaled
//! down by the compositor's bilinear filter, small text also comes out
//! jagged. So the capture thread averages each `factor` x `factor` block
//! into one pixel, redoing only the blocks the compositor says changed.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use wayland_client::protocol::wl_shm;

use crate::capture::Damage;

/// How much the overview shrinks windows, shared by the UI (which sets it
/// on every layout) and the capture thread (which shrinks frames to match).
#[derive(Clone, Default)]
pub struct Zoom(Arc<AtomicU32>);

impl Zoom {
    pub fn set(&self, scale: f64) {
        self.0.store((scale as f32).to_bits(), Ordering::Relaxed);
    }

    /// The largest power of two that still leaves at least one captured
    /// pixel per pixel on screen. Capped, so tiny thumbnails don't turn to
    /// mush. `None` until the UI has laid the thumbnails out.
    pub fn factor(&self) -> Option<u32> {
        let scale = f32::from_bits(self.0.load(Ordering::Relaxed));
        if scale <= 0.0 {
            return None;
        }
        let mut factor = 1;
        while factor < 8 && (factor * 2) as f32 * scale <= 1.0 {
            factor *= 2;
        }
        Some(factor)
    }
}

/// Stands in for the alpha position of formats that have none.
const NO_ALPHA: usize = usize::MAX;

fn has_alpha(format: wl_shm::Format) -> bool {
    matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Abgr8888)
}

/// A window's picture at 1/`factor` of its size, kept up to date frame by
/// frame. Bytes are B, G, R, A: `Argb8888`, or `Xrgb8888` without alpha.
pub struct Shrunk {
    pub factor: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    source: (u32, u32, wl_shm::Format),
}

impl Shrunk {
    pub fn new(src: &[u8], width: u32, height: u32, stride: u32, format: wl_shm::Format, factor: u32) -> Self {
        let mut shrunk = Self {
            factor,
            width: width.div_ceil(factor),
            height: height.div_ceil(factor),
            pixels: Vec::new(),
            source: (width, height, format),
        };
        shrunk.pixels = vec![0; shrunk.width as usize * shrunk.height as usize * 4];
        shrunk.fill(src, stride, (0, 0, shrunk.width, shrunk.height));
        shrunk
    }

    /// Whether frames of this size and format at this factor can update it.
    pub fn fits(&self, width: u32, height: u32, format: wl_shm::Format, factor: u32) -> bool {
        self.source == (width, height, format) && self.factor == factor
    }

    pub fn format(&self) -> wl_shm::Format {
        if has_alpha(self.source.2) { wl_shm::Format::Argb8888 } else { wl_shm::Format::Xrgb8888 }
    }

    /// Redoes the part of the picture that `damage` (in source pixels)
    /// touches, and returns that part in shrunk pixels.
    pub fn update(&mut self, src: &[u8], stride: u32, damage: Damage) -> Damage {
        let f = self.factor as i64;
        let clamp = |v: i64, max: u32| v.clamp(0, max as i64) as u32;
        let x0 = clamp(damage.x as i64 / f, self.width);
        let y0 = clamp(damage.y as i64 / f, self.height);
        let x1 = clamp((damage.x as i64 + damage.width as i64 + f - 1) / f, self.width);
        let y1 = clamp((damage.y as i64 + damage.height as i64 + f - 1) / f, self.height);
        self.fill(src, stride, (x0, y0, x1, y1));
        Damage { x: x0 as i32, y: y0 as i32, width: (x1 - x0) as i32, height: (y1 - y0) as i32 }
    }

    /// Recomputes the shrunk pixels from x0..x1, y0..y1.
    ///
    /// This runs for every live frame, so the factor and the pixel layout
    /// are compile-time constants: the loops below unroll and vectorize,
    /// and averages over whole blocks become shifts.
    fn fill(&mut self, src: &[u8], stride: u32, region: (u32, u32, u32, u32)) {
        match self.factor {
            1 => self.fill_factor::<1>(src, stride, region),
            2 => self.fill_factor::<2>(src, stride, region),
            4 => self.fill_factor::<4>(src, stride, region),
            8 => self.fill_factor::<8>(src, stride, region),
            factor => unreachable!("Zoom never hands out factor {factor}"),
        }
    }

    fn fill_factor<const F: usize>(&mut self, src: &[u8], stride: u32, region: (u32, u32, u32, u32)) {
        use wl_shm::Format::*;
        // The byte offsets of R, G, B and A in memory. DRM formats name
        // channels from the most significant byte, and are little-endian,
        // so in memory they come the other way around.
        match self.source.2 {
            Argb8888 => self.fill_with::<F, 4, 2, 1, 0, 3>(src, stride, region),
            Abgr8888 => self.fill_with::<F, 4, 0, 1, 2, 3>(src, stride, region),
            Xbgr8888 => self.fill_with::<F, 4, 0, 1, 2, NO_ALPHA>(src, stride, region),
            Bgr888 => self.fill_with::<F, 3, 0, 1, 2, NO_ALPHA>(src, stride, region),
            Rgb888 => self.fill_with::<F, 3, 2, 1, 0, NO_ALPHA>(src, stride, region),
            _ => self.fill_with::<F, 4, 2, 1, 0, NO_ALPHA>(src, stride, region),
        }
    }

    /// Two straight passes per output row. First add up the `F` source rows
    /// behind it, byte by byte. Then add up each run of `F` pixels in that
    /// sum and put the channels in output order.
    fn fill_with<const F: usize, const BYTES: usize, const R: usize, const G: usize, const B: usize, const A: usize>(
        &mut self,
        src: &[u8],
        stride: u32,
        (x0, y0, x1, y1): (u32, u32, u32, u32),
    ) {
        let stride = stride as usize;
        let (src_width, src_height) = (self.source.0 as usize, self.source.1 as usize);
        let (x0, x1) = (x0 as usize, x1 as usize);
        let (left, right) = (x0 * F, (x1 * F).min(src_width));
        let row_bytes = self.width as usize * 4;
        let pixel = |sum: [u32; 4], count: u32| {
            // Whole blocks hold F * F pixels, a power of two.
            let average = |sum: u32| {
                let rounded = sum + count / 2;
                (if count == (F * F) as u32 { rounded >> (2 * F.trailing_zeros()) } else { rounded / count }) as u8
            };
            [average(sum[0]), average(sum[1]), average(sum[2]), if A < BYTES { average(sum[3]) } else { 255 }]
        };
        let add = |sum: &mut [u32; 4], pixel: &[u16; BYTES]| {
            sum[0] += pixel[B] as u32;
            sum[1] += pixel[G] as u32;
            sum[2] += pixel[R] as u32;
            if A < BYTES {
                sum[3] += pixel[A % BYTES] as u32;
            }
        };
        // At most 8 rows of 255 each, so u16 is plenty.
        let mut columns = vec![0u16; (right - left) * BYTES];

        for y in y0 as usize..y1 as usize {
            let (top, bottom) = (y * F, ((y + 1) * F).min(src_height));
            let out = &mut self.pixels[y * row_bytes + x0 * 4..y * row_bytes + x1 * 4];
            let (out, _) = out.as_chunks_mut::<4>();

            if F == 1 {
                let start = top * stride + left * BYTES;
                let (pixels, _) = src[start..start + (right - left) * BYTES].as_chunks::<BYTES>();
                for (out, pixel) in out.iter_mut().zip(pixels) {
                    let alpha = if A < BYTES { pixel[A % BYTES] } else { 255 };
                    *out = [pixel[B], pixel[G], pixel[R], alpha];
                }
                continue;
            }

            columns.fill(0);
            for line in top..bottom {
                let start = line * stride + left * BYTES;
                for (sum, &byte) in columns.iter_mut().zip(&src[start..start + (right - left) * BYTES]) {
                    *sum += byte as u16;
                }
            }
            let rows = (bottom - top) as u32;
            let (pixels, _) = columns.as_chunks::<BYTES>();
            // The right edge can end in a block narrower than F.
            let (blocks, tail) = pixels.as_chunks::<F>();
            for (out, block) in out.iter_mut().zip(blocks) {
                let mut sum = [0u32; 4];
                block.iter().for_each(|p| add(&mut sum, p));
                *out = pixel(sum, rows * F as u32);
            }
            if !tail.is_empty() {
                let mut sum = [0u32; 4];
                tail.iter().for_each(|p| add(&mut sum, p));
                out[blocks.len()] = pixel(sum, rows * tail.len() as u32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factor_keeps_a_pixel_per_screen_pixel() {
        let zoom = Zoom::default();
        assert_eq!(zoom.factor(), None, "no layout yet");
        for (scale, factor) in [(0.8, 1), (0.5, 2), (0.37, 2), (0.25, 4), (0.2, 4), (0.01, 8)] {
            zoom.set(scale);
            assert_eq!(zoom.factor(), Some(factor), "scale {scale}");
        }
    }

    #[test]
    fn averages_blocks_and_reorders_channels() {
        // 3x2 Bgr888 (bytes R, G, B), shrunk by 2: a 2x1 result whose last
        // column only covers one source column.
        let src = [
            10, 20, 30, 30, 40, 50, 100, 0, 0, //
            10, 20, 30, 30, 40, 50, 200, 0, 0,
        ];
        let shrunk = Shrunk::new(&src, 3, 2, 9, wl_shm::Format::Bgr888, 2);
        assert_eq!((shrunk.width, shrunk.height), (2, 1));
        assert_eq!(shrunk.format(), wl_shm::Format::Xrgb8888);
        // B, G, R, X
        assert_eq!(shrunk.pixels, [40, 30, 20, 255, 0, 0, 150, 255]);
    }

    #[test]
    fn odd_sizes_average_their_partial_edge_blocks() {
        // 5x3 Xbgr8888 (bytes R, G, B, X) shrunk by 4: the right block is
        // one column wide and both blocks only three rows tall.
        let mut src = vec![0u8; 5 * 3 * 4];
        for (i, pixel) in src.chunks_mut(4).enumerate() {
            pixel[0] = if i % 5 == 4 { 90 } else { 30 };
        }
        let shrunk = Shrunk::new(&src, 5, 3, 20, wl_shm::Format::Xbgr8888, 4);
        assert_eq!((shrunk.width, shrunk.height), (2, 1));
        assert_eq!(shrunk.pixels, [0, 0, 30, 255, 0, 0, 90, 255]);
    }

    #[test]
    fn updates_only_the_damaged_blocks() {
        let mut src = vec![0u8; 8 * 8 * 4];
        let mut shrunk = Shrunk::new(&src, 8, 8, 32, wl_shm::Format::Argb8888, 2);
        src.fill(255);
        let changed = shrunk.update(&src, 32, Damage { x: 3, y: 0, width: 2, height: 1 });
        // Source columns 3..5 straddle shrunk columns 1 and 2.
        assert_eq!((changed.x, changed.y, changed.width, changed.height), (1, 0, 2, 1));
        let lit: Vec<_> = shrunk.pixels.chunks(4).map(|p| p == [255; 4]).collect();
        assert_eq!(lit.iter().filter(|&&l| l).count(), 2);
        assert!(lit[1] && lit[2]);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release shrink_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn shrink_speed() {
        let (w, h) = (3836u32, 2156u32);
        let src: Vec<u8> = (0..w * h * 3).map(|i| i as u8).collect();
        for factor in [1, 2, 4] {
            let start = std::time::Instant::now();
            for _ in 0..20 {
                std::hint::black_box(Shrunk::new(&src, w, h, w * 3, wl_shm::Format::Bgr888, factor));
            }
            eprintln!("factor {factor}: {:?} per 4K frame", start.elapsed() / 20);
        }
        let start = std::time::Instant::now();
        for _ in 0..20 {
            std::hint::black_box(src.clone());
        }
        eprintln!("plain copy: {:?} per 4K frame", start.elapsed() / 20);
    }
}
