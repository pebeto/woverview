//! Decides where each thumbnail goes.
//!
//! Every window shrinks by the same factor, so a big window stays bigger than
//! a small one. Windows fill rows in the order given. We try every row count
//! and keep the one that lets the thumbnails be largest.

use std::ops::Range;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }
}

pub struct Params {
    /// Gap between thumbnails, in both directions.
    pub spacing: f64,
    /// Room kept free under each thumbnail for its title.
    pub label_height: f64,
    /// Thumbnails never get larger than this fraction of the real window.
    pub max_scale: f64,
}

/// Returns one rectangle per entry of `sizes` (natural width and height),
/// all inside `area`.
pub fn arrange(sizes: &[(f64, f64)], area: Rect, params: &Params) -> Vec<Rect> {
    if sizes.is_empty() {
        return Vec::new();
    }
    let (scale, rows) = (1..=sizes.len())
        .map(|count| {
            let rows = split(sizes, count);
            (scale_for(sizes, &rows, area, params), rows)
        })
        // On a tie, keep the fewer rows.
        .reduce(|best, next| if next.0 > best.0 { next } else { best })
        .unwrap();
    place(sizes, &rows, scale, area, params)
}

/// Splits the windows into at most `count` rows of roughly equal width.
fn split(sizes: &[(f64, f64)], count: usize) -> Vec<Range<usize>> {
    let ideal = sizes.iter().map(|s| s.0).sum::<f64>() / count as f64;
    let mut rows = Vec::with_capacity(count);
    let (mut start, mut width) = (0, 0.0);
    for (i, &(w, _)) in sizes.iter().enumerate() {
        // Break before this window if keeping it would overshoot the ideal
        // width by more than leaving it out would fall short.
        if i > start && rows.len() + 1 < count && width + w - ideal > ideal - width {
            rows.push(start..i);
            start = i;
            width = 0.0;
        }
        width += w;
    }
    rows.push(start..sizes.len());
    rows
}

fn row_height(sizes: &[(f64, f64)], row: &Range<usize>) -> f64 {
    sizes[row.clone()].iter().map(|s| s.1).fold(0.0, f64::max)
}

fn row_width(sizes: &[(f64, f64)], row: &Range<usize>) -> f64 {
    sizes[row.clone()].iter().map(|s| s.0).sum()
}

fn scale_for(sizes: &[(f64, f64)], rows: &[Range<usize>], area: Rect, params: &Params) -> f64 {
    let gaps = params.spacing * (rows.len() - 1) as f64 + params.label_height * rows.len() as f64;
    let height: f64 = rows.iter().map(|row| row_height(sizes, row)).sum();
    let mut scale = ((area.height - gaps) / height).min(params.max_scale);
    for row in rows {
        let room = area.width - params.spacing * (row.len() - 1) as f64;
        scale = scale.min(room / row_width(sizes, row));
    }
    scale.max(0.0)
}

fn place(sizes: &[(f64, f64)], rows: &[Range<usize>], scale: f64, area: Rect, params: &Params) -> Vec<Rect> {
    let total: f64 = rows.iter().map(|row| row_height(sizes, row) * scale + params.label_height).sum::<f64>()
        + params.spacing * (rows.len() - 1) as f64;
    let mut y = area.y + (area.height - total) / 2.0;
    let mut rects = Vec::with_capacity(sizes.len());
    for row in rows {
        let height = row_height(sizes, row) * scale;
        let width = row_width(sizes, row) * scale + params.spacing * (row.len() - 1) as f64;
        let mut x = area.x + (area.width - width) / 2.0;
        for &(w, h) in &sizes[row.clone()] {
            let (w, h) = (w * scale, h * scale);
            rects.push(Rect { x, y: y + (height - h) / 2.0, width: w, height: h });
            x += w + params.spacing;
        }
        y += height + params.label_height + params.spacing;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 100.0, y: 100.0, width: 3640.0, height: 1960.0 };
    const PARAMS: Params = Params { spacing: 60.0, label_height: 40.0, max_scale: 0.8 };

    fn overlaps(a: &Rect, b: &Rect) -> bool {
        a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
    }

    fn inside(r: &Rect, area: &Rect) -> bool {
        const EPS: f64 = 1e-6;
        r.x >= area.x - EPS
            && r.y >= area.y - EPS
            && r.x + r.width <= area.x + area.width + EPS
            && r.y + r.height + PARAMS.label_height <= area.y + area.height + EPS
    }

    #[test]
    fn nothing_to_arrange() {
        assert!(arrange(&[], SCREEN, &PARAMS).is_empty());
    }

    #[test]
    fn a_lone_window_is_centered_and_capped() {
        let rects = arrange(&[(1000.0, 500.0)], SCREEN, &PARAMS);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        assert_eq!((r.width, r.height), (800.0, 400.0));
        assert_eq!(r.center().0, SCREEN.center().0);
    }

    #[test]
    fn mixed_sizes_fit_without_overlap() {
        // What a day on a 4K screen looks like: full-screen windows on some
        // workspaces, a 2x2 grid on another, one floating dialog.
        let sizes = [
            (3840.0, 2130.0),
            (3840.0, 2130.0),
            (1920.0, 1065.0),
            (1920.0, 1065.0),
            (1920.0, 1065.0),
            (1920.0, 1065.0),
            (800.0, 600.0),
            (3840.0, 2130.0),
        ];
        let rects = arrange(&sizes, SCREEN, &PARAMS);
        assert_eq!(rects.len(), sizes.len());
        for (i, (r, s)) in rects.iter().zip(&sizes).enumerate() {
            assert!(inside(r, &SCREEN), "{i} sticks out: {r:?}");
            assert!((r.width / r.height - s.0 / s.1).abs() < 1e-9, "{i} lost its shape");
            for other in &rects[i + 1..] {
                assert!(!overlaps(r, other), "{r:?} overlaps {other:?}");
            }
        }
        // One shared scale: the full-screen windows stay twice as wide as the tiles.
        assert!((rects[0].width / rects[2].width - 2.0).abs() < 1e-9);
    }

    #[test]
    fn many_windows_use_several_rows() {
        let sizes = vec![(1600.0, 900.0); 12];
        let rects = arrange(&sizes, SCREEN, &PARAMS);
        let mut rows: Vec<i64> = rects.iter().map(|r| r.y.round() as i64).collect();
        rows.dedup();
        assert!(rows.len() > 1);
        assert!(rects.iter().all(|r| inside(r, &SCREEN)));
    }
}
