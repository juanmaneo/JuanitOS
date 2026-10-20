//! An RGBA raster and the handful of primitives the artwork needs.
//!
//! Shapes are anti-aliased by 4×4 supersampling of a signed-distance test,
//! which is plenty for icons under a hundred pixels and keeps the code
//! free of any clever (and bug-prone) scanline logic.

use alloc::vec;
use alloc::vec::Vec;

use crate::color::Rgba;

/// Sub-samples per axis for anti-aliasing.
const SS: i32 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    width: u32,
    height: u32,
    pixels: Vec<Rgba>,
}

/// The opening of an [`Image::arc_ring`]: centre angle and half-width,
/// both in radians.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gap {
    pub angle: f32,
    pub half: f32,
}

/// An axis-aligned rectangle in pixel coordinates (`x..x+w`, `y..y+h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub const fn right(&self) -> i32 {
        self.x + self.w
    }

    pub const fn bottom(&self) -> i32 {
        self.y + self.h
    }
}

impl Image {
    pub fn new(width: u32, height: u32, fill: Rgba) -> Self {
        let n = width as usize * height as usize;
        Image {
            width,
            height,
            pixels: vec![fill; n],
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn pixels(&self) -> &[Rgba] {
        &self.pixels
    }

    /// One row of pixels, or `None` when `y` is outside the image.
    pub fn row(&self, y: u32) -> Option<&[Rgba]> {
        if y >= self.height {
            return None;
        }
        let w = self.width as usize;
        let start = y as usize * w;
        self.pixels.get(start..start + w)
    }

    /// Index of (x, y) in `pixels`, `None` outside the image.
    fn index(&self, x: i32, y: i32) -> Option<usize> {
        let inside = (0..self.width as i32).contains(&x) && (0..self.height as i32).contains(&y);
        inside.then(|| y as usize * self.width as usize + x as usize)
    }

    pub fn get(&self, x: i32, y: i32) -> Option<Rgba> {
        self.index(x, y).and_then(|i| self.pixels.get(i)).copied()
    }

    /// Composite `c` over the pixel at (x, y); out-of-range is ignored.
    pub fn blend(&mut self, x: i32, y: i32, c: Rgba) {
        if let Some(p) = self.index(x, y).and_then(|i| self.pixels.get_mut(i)) {
            *p = c.over(*p);
        }
    }

    /// Composite `src` onto `self` with its top-left corner at (x, y).
    pub fn draw_image(&mut self, src: &Image, x: i32, y: i32) {
        let rows = src.pixels.chunks(src.width.max(1) as usize);
        for (sy, row) in rows.enumerate() {
            for (sx, &c) in row.iter().enumerate() {
                self.blend(x + sx as i32, y + sy as i32, c);
            }
        }
    }

    /// Axis-aligned filled rectangle (no anti-aliasing needed).
    pub fn fill_rect(&mut self, r: Rect, c: Rgba) {
        for y in r.y..r.bottom() {
            for x in r.x..r.right() {
                self.blend(x, y, c);
            }
        }
    }

    /// Fill every pixel whose centre satisfies `inside`, anti-aliased by
    /// supersampling within the bounding box `bounds`.
    fn fill_shape<F: Fn(f32, f32) -> bool>(&mut self, bounds: Rect, c: Rgba, inside: F) {
        let total = (SS * SS) as u32;
        for y in bounds.y..bounds.bottom() {
            for x in bounds.x..bounds.right() {
                let mut hits = 0u32;
                for j in 0..SS {
                    for i in 0..SS {
                        let px = x as f32 + (i as f32 + 0.5) / SS as f32;
                        let py = y as f32 + (j as f32 + 0.5) / SS as f32;
                        if inside(px, py) {
                            hits += 1;
                        }
                    }
                }
                if hits > 0 {
                    let a = ((hits * 255 + total / 2) / total) as u8;
                    self.blend(x, y, c.with_alpha_scaled(a));
                }
            }
        }
    }

    /// Anti-aliased filled circle.
    pub fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, c: Rgba) {
        let r = radius.max(0.0);
        let bounds = circle_bounds(cx, cy, r);
        let r2 = r * r;
        self.fill_shape(bounds, c, |x, y| {
            let dx = x - cx;
            let dy = y - cy;
            dx * dx + dy * dy <= r2
        });
    }

    /// Anti-aliased ring between `inner` and `outer` radius.
    pub fn ring(&mut self, cx: f32, cy: f32, inner: f32, outer: f32, c: Rgba) {
        let r = outer.max(0.0);
        let bounds = circle_bounds(cx, cy, r);
        let o2 = r * r;
        let i2 = inner.max(0.0) * inner.max(0.0);
        self.fill_shape(bounds, c, |x, y| {
            let dx = x - cx;
            let dy = y - cy;
            let d2 = dx * dx + dy * dy;
            d2 <= o2 && d2 >= i2
        });
    }

    /// Ring with a gap: the arc is skipped for angles within `gap.half`
    /// radians of `gap.angle` (angles measured from +x, y pointing down).
    pub fn arc_ring(&mut self, cx: f32, cy: f32, inner: f32, outer: f32, gap: Gap, c: Rgba) {
        let Gap {
            angle: gap_angle,
            half: gap_half,
        } = gap;
        let r = outer.max(0.0);
        let bounds = circle_bounds(cx, cy, r);
        let o2 = r * r;
        let i2 = inner.max(0.0) * inner.max(0.0);
        self.fill_shape(bounds, c, |x, y| {
            let dx = x - cx;
            let dy = y - cy;
            let d2 = dx * dx + dy * dy;
            d2 <= o2 && d2 >= i2 && angle_distance(atan2(dy, dx), gap_angle) > gap_half
        });
    }

    /// Anti-aliased rounded rectangle.
    pub fn rounded_rect(&mut self, r: Rect, radius: f32, c: Rgba) {
        let rad = radius.max(0.0).min(r.w as f32 / 2.0).min(r.h as f32 / 2.0);
        let x0 = r.x as f32;
        let y0 = r.y as f32;
        let x1 = r.right() as f32;
        let y1 = r.bottom() as f32;
        // Every sample `fill_shape` takes lies inside `r`, its bounds.
        self.fill_shape(r, c, |x, y| {
            // Distance to the inner (corner-centre) box.
            let ix = (x0 + rad).max(x.min(x1 - rad));
            let iy = (y0 + rad).max(y.min(y1 - rad));
            let dx = x - ix;
            let dy = y - iy;
            dx * dx + dy * dy <= rad * rad
        });
    }

    /// Anti-aliased filled polygon (even-odd rule); fewer than three points
    /// draw nothing.
    pub fn fill_polygon(&mut self, points: &[(f32, f32)], c: Rgba) {
        if points.len() < 3 {
            return;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in points {
            (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
        }
        let bounds = Rect::new(
            floor_i(x0),
            floor_i(y0),
            ceil_i(x1) - floor_i(x0) + 1,
            ceil_i(y1) - floor_i(y0) + 1,
        );
        let edges = || points.iter().zip(points.iter().cycle().skip(1));
        self.fill_shape(bounds, c, |x, y| {
            edges()
                .filter(|(&(xa, ya), &(xb, yb))| {
                    (ya > y) != (yb > y) && x < xa + (y - ya) * (xb - xa) / (yb - ya)
                })
                .count()
                % 2
                == 1
        });
    }

    /// Anti-aliased thick line segment with round caps.
    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, thickness: f32, c: Rgba) {
        let half = thickness / 2.0;
        let minx = x0.min(x1) - half;
        let miny = y0.min(y1) - half;
        let maxx = x0.max(x1) + half;
        let maxy = y0.max(y1) + half;
        let bounds = Rect::new(
            floor_i(minx) - 1,
            floor_i(miny) - 1,
            ceil_i(maxx - minx) + 3,
            ceil_i(maxy - miny) + 3,
        );
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len2 = dx * dx + dy * dy;
        let h2 = half * half;
        self.fill_shape(bounds, c, |x, y| {
            let t = if len2 <= 0.0 {
                0.0
            } else {
                (((x - x0) * dx + (y - y0) * dy) / len2).clamp(0.0, 1.0)
            };
            let px = x0 + t * dx;
            let py = y0 + t * dy;
            let ex = x - px;
            let ey = y - py;
            ex * ex + ey * ey <= h2
        });
    }
}

/// `floor` as an integer; `core` has no float rounding without libm.
fn floor_i(x: f32) -> i32 {
    let t = x as i32;
    if (t as f32) > x {
        t - 1
    } else {
        t
    }
}

/// `ceil` as an integer.
fn ceil_i(x: f32) -> i32 {
    let t = x as i32;
    if (t as f32) < x {
        t + 1
    } else {
        t
    }
}

fn fabs(x: f32) -> f32 {
    if x < 0.0 {
        -x
    } else {
        x
    }
}

/// Bounding box of a circle of radius `r` around (cx, cy), one pixel of
/// slack on each side.
fn circle_bounds(cx: f32, cy: f32, r: f32) -> Rect {
    Rect::new(
        floor_i(cx - r) - 1,
        floor_i(cy - r) - 1,
        ceil_i(2.0 * r) + 3,
        ceil_i(2.0 * r) + 3,
    )
}

/// Smallest absolute difference between two angles, in radians.
fn angle_distance(a: f32, b: f32) -> f32 {
    let tau = core::f32::consts::TAU;
    let mut d = (a - b) % tau;
    if d < 0.0 {
        d += tau;
    }
    if d > core::f32::consts::PI {
        tau - d
    } else {
        d
    }
}

/// `atan2` without libm: good to ~0.005 rad, which is far below the
/// anti-aliasing resolution of this crate.
fn atan2(y: f32, x: f32) -> f32 {
    use core::f32::consts::{FRAC_PI_2, PI};
    if x == 0.0 && y == 0.0 {
        return 0.0;
    }
    let ax = fabs(x);
    let ay = fabs(y);
    let (num, den, swap) = if ay > ax {
        (ax, ay, true)
    } else {
        (ay, ax, false)
    };
    let t = num / den; // 0..=1
                       // Polynomial approximation of atan on [0, 1].
    let t2 = t * t;
    let mut a =
        t * (0.999_866 + t2 * (-0.330_299 + t2 * (0.180_141 + t2 * (-0.085_133 + t2 * 0.020_835))));
    if swap {
        a = FRAC_PI_2 - a;
    }
    if x < 0.0 {
        a = PI - a;
    }
    if y < 0.0 {
        -a
    } else {
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    const BLACK: Rgba = Rgba::rgb(0, 0, 0);

    #[test]
    fn rect_fill_and_bounds() {
        let mut img = Image::new(8, 8, BLACK);
        img.fill_rect(Rect::new(2, 2, 3, 2), WHITE);
        assert_eq!(img.get(2, 2), Some(WHITE));
        assert_eq!(img.get(4, 3), Some(WHITE));
        assert_eq!(img.get(5, 3), Some(BLACK));
        assert_eq!(img.get(2, 4), Some(BLACK));
        assert_eq!(img.get(8, 0), None);
        assert_eq!(img.get(-1, 0), None);
        // Drawing outside is silently clipped.
        img.fill_rect(Rect::new(-5, -5, 100, 100), WHITE);
        assert_eq!(img.get(7, 7), Some(WHITE));
    }

    #[test]
    fn circle_is_solid_inside_and_soft_on_edge() {
        let mut img = Image::new(32, 32, BLACK);
        // Centre on a pixel corner so the rim straddles pixels 6 and 26.
        img.fill_circle(16.5, 16.5, 10.0, WHITE);
        assert_eq!(img.get(16, 16), Some(WHITE));
        assert_eq!(img.get(0, 0), Some(BLACK));
        // The edge pixel is partially covered.
        let edge = img.get(26, 16).unwrap();
        assert!(edge.r > 0 && edge.r < 255, "edge pixel {edge:?}");
        // Symmetry.
        assert_eq!(img.get(6, 16), img.get(26, 16));
    }

    #[test]
    fn ring_has_a_hole() {
        let mut img = Image::new(40, 40, BLACK);
        img.ring(20.0, 20.0, 8.0, 12.0, WHITE);
        assert_eq!(img.get(20, 20), Some(BLACK));
        assert_eq!(img.get(30, 20), Some(WHITE));
        assert_eq!(img.get(35, 20), Some(BLACK));
    }

    #[test]
    fn arc_ring_gap_is_at_the_top() {
        let mut img = Image::new(40, 40, BLACK);
        // Gap centred at -90° (up).
        img.arc_ring(
            20.0,
            20.0,
            8.0,
            12.0,
            Gap {
                angle: -core::f32::consts::FRAC_PI_2,
                half: 0.5,
            },
            WHITE,
        );
        assert_eq!(img.get(20, 10), Some(BLACK));
        assert_eq!(img.get(20, 30), Some(WHITE));
    }

    #[test]
    fn polygons_are_filled_inside_their_edges() {
        let mut img = Image::new(20, 20, BLACK);
        // A right triangle with its right angle at the top left.
        img.fill_polygon(&[(2.0, 2.0), (18.0, 2.0), (2.0, 18.0)], WHITE);
        assert_eq!(img.get(4, 4), Some(WHITE));
        assert_eq!(img.get(16, 16), Some(BLACK));
        assert_eq!(img.get(1, 1), Some(BLACK));
        let edge = img.get(9, 10).unwrap();
        assert!(edge != WHITE && edge != BLACK, "{edge:?}");
        // Degenerate input draws nothing.
        let mut img = Image::new(8, 8, BLACK);
        img.fill_polygon(&[(0.0, 0.0), (7.0, 7.0)], WHITE);
        assert!(img.pixels().iter().all(|&p| p == BLACK));
    }

    #[test]
    fn rounded_rect_cuts_corners() {
        let mut img = Image::new(40, 40, BLACK);
        img.rounded_rect(Rect::new(5, 5, 30, 30), 8.0, WHITE);
        assert_eq!(img.get(5, 5), Some(BLACK));
        assert_eq!(img.get(20, 20), Some(WHITE));
        assert_eq!(img.get(20, 5), Some(WHITE));
    }

    #[test]
    fn line_covers_its_path() {
        let mut img = Image::new(40, 40, BLACK);
        img.line(5.0, 20.0, 35.0, 20.0, 4.0, WHITE);
        assert_eq!(img.get(20, 20), Some(WHITE));
        assert_eq!(img.get(20, 10), Some(BLACK));
        assert_eq!(img.get(20, 19), Some(WHITE));
        // A zero-length line is a dot.
        let mut img = Image::new(8, 8, BLACK);
        img.line(4.0, 4.0, 4.0, 4.0, 2.0, WHITE);
        for (x, y) in [(3, 3), (4, 3), (3, 4), (4, 4)] {
            assert!(img.get(x, y).unwrap().r > 0, "({x}, {y})");
        }
        assert_eq!(img.get(0, 0), Some(BLACK));
    }

    #[test]
    fn atan2_approximation() {
        use core::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        let close = |a: f32, b: f32| fabs(a - b) < 0.01;
        assert!(close(atan2(0.0, 1.0), 0.0));
        assert!(close(atan2(1.0, 1.0), FRAC_PI_4));
        assert!(close(atan2(1.0, 0.0), FRAC_PI_2));
        assert!(close(atan2(0.0, -1.0), PI));
        assert!(close(atan2(-1.0, 0.0), -FRAC_PI_2));
        assert_eq!(atan2(0.0, 0.0), 0.0);
        assert!(close(angle_distance(-FRAC_PI_2, 3.0 * FRAC_PI_2), 0.0));
    }

    #[test]
    fn integer_rounding_helpers() {
        assert_eq!(floor_i(2.3), 2);
        assert_eq!(floor_i(-2.3), -3);
        assert_eq!(floor_i(4.0), 4);
        assert_eq!(ceil_i(2.3), 3);
        assert_eq!(ceil_i(-2.3), -2);
        assert_eq!(ceil_i(4.0), 4);
    }

    #[test]
    fn draw_image_composites() {
        let mut dst = Image::new(4, 4, BLACK);
        let src = Image::new(2, 2, WHITE);
        dst.draw_image(&src, 1, 1);
        assert_eq!(dst.get(1, 1), Some(WHITE));
        assert_eq!(dst.get(2, 2), Some(WHITE));
        assert_eq!(dst.get(3, 3), Some(BLACK));
        assert_eq!(dst.row(1).map(|r| r.len()), Some(4));
        assert!(dst.row(4).is_none());
        // An empty image draws nothing.
        let before = dst.clone();
        dst.draw_image(&Image::new(0, 3, WHITE), 0, 0);
        assert_eq!(dst, before);
    }
}
