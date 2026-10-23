//! The few pixels woverview draws itself, in software: window titles, the
//! selection ring, and the masks that round the thumbnails' corners. The
//! compositor does everything else.

use std::f64::consts::{FRAC_PI_2, PI};

use cairo::{Context, Format, ImageSurface};

/// Titles without a size given come out this big, in logical pixels.
const DEFAULT_SIZE: f64 = 15.0;
/// Pango, like sway, puts 96 pixels in an inch, and 72 points.
const PIXELS_PER_POINT: f64 = 96.0 / 72.0;
/// How tall a line of titles is, next to the size of its text.
const LINE_SPACING: f64 = 1.6;
const TEXT: (f64, f64, f64, f64) = (1.0, 1.0, 1.0, 0.92);

/// The font window titles are set in.
#[derive(Clone, Debug)]
pub struct Font(String);

impl Font {
    /// Takes a Pango font description the way sway's `font` option does,
    /// `pango:` prefix and all: "JuliaMono 15" is 15 points, as in sway,
    /// and "JuliaMono 20px" is 20 pixels. Without a size, it's 15 pixels.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let text = text.strip_prefix("pango:").unwrap_or(text).trim();
        if text.is_empty() {
            return Err("--font needs a font, like \"Sans 11\"".to_owned());
        }
        Ok(Self(text.to_owned()))
    }

    fn description(&self) -> pango::FontDescription {
        let mut description = pango::FontDescription::from_string(&self.0);
        if description.size() <= 0 {
            description.set_absolute_size(DEFAULT_SIZE * pango::SCALE as f64);
        }
        description
    }

    /// The text's size in logical pixels.
    fn pixels(&self) -> f64 {
        let description = self.description();
        let size = description.size() as f64 / pango::SCALE as f64;
        if description.is_size_absolute() { size } else { size * PIXELS_PER_POINT }
    }

    /// How tall a line of titles is, in logical pixels. It comes from the
    /// size alone, so laying the overview out doesn't wait for fonts to load.
    pub fn line_height(&self) -> u32 {
        (self.pixels() * LINE_SPACING).ceil() as u32
    }
}

impl Default for Font {
    fn default() -> Self {
        Self("Sans".to_owned())
    }
}

/// Premultiplied pixels, bytes B, G, R, A: `wl_shm` `Argb8888`.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Image {
    pub fn stride(&self) -> u32 {
        self.width * 4
    }
}

/// A thumbnail's title line: its workspace, dimmed, then the window title,
/// centered and cut short to fit. Sizes are logical; `scale` is how many
/// pixels the output has per logical pixel.
pub fn label(workspace: &str, title: &str, font: &Font, width: u32, height: u32, scale: f64) -> Image {
    paint(width, height, scale, |cr| {
        let layout = pangocairo::functions::create_layout(cr);
        layout.set_font_description(Some(&font.description()));
        layout.set_markup(&format!(
            "<span alpha=\"60%\">{}</span>   {}",
            pango::glib::markup_escape_text(workspace),
            pango::glib::markup_escape_text(title),
        ));
        layout.set_single_paragraph_mode(true);
        layout.set_width(width as i32 * pango::SCALE);
        layout.set_ellipsize(pango::EllipsizeMode::End);
        layout.set_alignment(pango::Alignment::Center);
        let (_, text_height) = layout.pixel_size();
        cr.move_to(0.0, ((height as i32 - text_height) / 2) as f64);
        cr.set_source_rgba(TEXT.0, TEXT.1, TEXT.2, TEXT.3);
        pangocairo::functions::show_layout(cr, &layout);
    })
}

/// A rounded ring `thickness` wide just inside a `width` x `height` box.
pub fn ring(width: u32, height: u32, thickness: f64, radius: f64, color: (f64, f64, f64), scale: f64) -> Image {
    paint(width, height, scale, |cr| {
        let half = thickness / 2.0;
        rounded_rect(cr, half, half, width as f64 - thickness, height as f64 - thickness, radius - half);
        cr.set_source_rgb(color.0, color.1, color.2);
        cr.set_line_width(thickness);
        let _ = cr.stroke();
    })
}

/// What covers a thumbnail's corner to round it: `color` outside a quarter
/// circle of `radius`, clear inside. `corner` counts clockwise from the
/// top left.
pub fn corner(corner: usize, radius: u32, color: (f64, f64, f64), scale: f64) -> Image {
    paint(radius, radius, scale, |cr| {
        let r = radius as f64;
        // Where the circle's center sits, for each corner.
        let (cx, cy) = [(r, r), (0.0, r), (0.0, 0.0), (r, 0.0)][corner];
        cr.rectangle(0.0, 0.0, r, r);
        cr.arc_negative(cx, cy, r, 0.0, -2.0 * PI);
        cr.set_fill_rule(cairo::FillRule::EvenOdd);
        cr.set_source_rgb(color.0, color.1, color.2);
        let _ = cr.fill();
    })
}

/// The close button a hovered thumbnail shows, as macOS does: a light disc
/// with a dark cross, outlined so it reads on light windows too. It's a
/// little darker while the pointer is on it.
pub fn close_button(size: u32, hot: bool, scale: f64) -> Image {
    paint(size, size, scale, |cr| {
        let r = size as f64 / 2.0;
        cr.arc(r, r, r - 1.0, 0.0, 2.0 * PI);
        let fill = if hot { 0.76 } else { 0.92 };
        cr.set_source_rgb(fill, fill, fill);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
        cr.set_line_width(1.0);
        let _ = cr.stroke();

        let arm = r * 0.36;
        cr.move_to(r - arm, r - arm);
        cr.line_to(r + arm, r + arm);
        cr.move_to(r + arm, r - arm);
        cr.line_to(r - arm, r + arm);
        cr.set_source_rgb(0.15, 0.15, 0.15);
        cr.set_line_width((r * 0.16).max(1.5));
        cr.set_line_cap(cairo::LineCap::Round);
        let _ = cr.stroke();
    })
}

fn rounded_rect(cr: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.max(0.0).min(width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + width - r, y + height - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + height - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Runs `draw` on a clear surface of `width` x `height` logical pixels,
/// at `scale`, and returns what it drew.
fn paint(width: u32, height: u32, scale: f64, draw: impl FnOnce(&Context)) -> Image {
    let (pw, ph) = ((width as f64 * scale).ceil().max(1.0) as i32, (height as f64 * scale).ceil().max(1.0) as i32);
    let mut surface = ImageSurface::create(Format::ARgb32, pw, ph).expect("cairo can't make an image surface");
    {
        let cr = Context::new(&surface).expect("cairo can't draw on its own surface");
        cr.scale(scale, scale);
        draw(&cr);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().expect("the cairo context is gone");
    let row = pw as usize * 4;
    let mut pixels = Vec::with_capacity(row * ph as usize);
    for line in data.chunks(stride).take(ph as usize) {
        pixels.extend_from_slice(&line[..row]);
    }
    Image { width: pw as u32, height: ph as u32, pixels }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(image: &Image, x: u32, y: u32) -> u8 {
        image.pixels[(y * image.stride() + x * 4 + 3) as usize]
    }

    #[test]
    fn close_button_is_a_disc_with_a_cross() {
        let image = close_button(28, false, 1.0);
        let pixel = |x: u32, y: u32| &image.pixels[(y * image.stride() + x * 4) as usize..][..4];
        assert_eq!(pixel(0, 0)[3], 0, "outside the disc stays clear");
        assert!(pixel(14, 14)[0] < 80, "the cross runs through the middle");
        assert!(pixel(14, 4)[0] > 200, "the disc around it is light");
        let hot = close_button(28, true, 1.0);
        assert!(hot.pixels[(4 * hot.stride() + 14 * 4) as usize] < pixel(14, 4)[0], "darker under the pointer");
    }

    #[test]
    fn corners_cover_only_outside_the_arc() {
        for (i, (outside, inside)) in [((0, 0), (11, 11)), ((11, 0), (0, 11)), ((11, 11), (0, 0)), ((0, 11), (11, 0))].into_iter().enumerate() {
            let image = corner(i, 12, (0.0, 0.0, 0.0), 1.0);
            assert_eq!(alpha(&image, outside.0, outside.1), 255, "corner {i} should cover {outside:?}");
            assert_eq!(alpha(&image, inside.0, inside.1), 0, "corner {i} should leave {inside:?} clear");
        }
    }

    #[test]
    fn ring_is_hollow() {
        let image = ring(100, 60, 4.0, 16.0, (0.3, 0.6, 1.0), 1.0);
        assert_eq!(alpha(&image, 50, 1), 255);
        assert_eq!(alpha(&image, 50, 30), 0);
    }

    #[test]
    fn font_sizes_read_like_sway_reads_them() {
        // 15 points at 96 per inch is 20 pixels; lines are 1.6 times that.
        assert_eq!(Font::parse("JuliaMono 15").unwrap().line_height(), 32);
        assert_eq!(Font::parse("pango:JuliaMono Regular 10").unwrap().line_height(), 22);
        assert_eq!(Font::parse("Sans 20px").unwrap().line_height(), 32);
        assert_eq!(Font::parse("Sans").unwrap().line_height(), Font::default().line_height());
        assert_eq!(Font::default().line_height(), 24);
        assert!(Font::parse("  pango:  ").is_err());
    }

    #[test]
    fn labels_come_out_at_the_output_scale() {
        let image = label("3", "A window", &Font::default(), 200, 30, 1.5);
        assert_eq!((image.width, image.height), (300, 45));
        assert!(image.pixels.chunks(4).any(|p| p[3] > 0), "no text drawn");
    }
}
