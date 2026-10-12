//! The logo and the boot-stage icons, drawn from primitives.
//!
//! Every icon is designed in a unit square and scaled to `size` pixels, so
//! the same code produces the 48 px row on the splash and a 24 px variant
//! in a menu. Icons are drawn in a single colour: lit icons use the accent
//! colour, dormant ones the dim grey.

use crate::color::Rgba;
use crate::image::{Gap, Image, Rect};

/// Boot-stage icons, in the order they light up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// Firmware / pre-loader: a power symbol.
    Power,
    /// Kernel: an atom.
    Kernel,
    /// ZFS pools and datasets: a stack of platters.
    Storage,
    /// Networking (sys-net / sys-firewall): three linked nodes.
    Network,
    /// Qubes autostart: three isolated cubes.
    Qubes,
    /// Desktop (COSMIC greeter): a window with a dock.
    Desktop,
}

/// Draw `icon` with its top-left corner at (x, y) in a `size`×`size` box.
/// `bg` is the colour the icon sits on (used to cut holes).
pub fn draw_icon(img: &mut Image, icon: Icon, x: i32, y: i32, size: i32, c: Rgba, bg: Rgba) {
    let s = size as f32;
    let ox = x as f32;
    let oy = y as f32;
    // Unit-square helpers.
    let px = |u: f32| ox + u * s;
    let py = |v: f32| oy + v * s;
    let r = |u: f32| u * s;
    let rect = |u: f32, v: f32, w: f32, h: f32| {
        Rect::new(
            (ox + u * s) as i32,
            (oy + v * s) as i32,
            (w * s) as i32,
            (h * s) as i32,
        )
    };

    match icon {
        Icon::Power => {
            img.arc_ring(
                px(0.5),
                py(0.54),
                r(0.30),
                r(0.40),
                Gap {
                    angle: -core::f32::consts::FRAC_PI_2,
                    half: 0.55,
                },
                c,
            );
            img.line(px(0.5), py(0.10), px(0.5), py(0.50), r(0.10), c);
        }
        Icon::Kernel => {
            img.fill_circle(px(0.5), py(0.5), r(0.11), c);
            img.ring(px(0.5), py(0.5), r(0.36), r(0.42), c);
            for k in 0..3 {
                let a = -core::f32::consts::FRAC_PI_2 + k as f32 * core::f32::consts::TAU / 3.0;
                let (sin, cos) = sin_cos(a);
                img.fill_circle(px(0.5) + r(0.39) * cos, py(0.5) + r(0.39) * sin, r(0.08), c);
            }
        }
        Icon::Storage => {
            for k in 0..3 {
                let v = 0.10 + k as f32 * 0.28;
                img.rounded_rect(rect(0.12, v, 0.76, 0.22), r(0.08), c);
            }
        }
        Icon::Network => {
            let top = (px(0.5), py(0.24));
            let left = (px(0.22), py(0.78));
            let right = (px(0.78), py(0.78));
            img.line(top.0, top.1, left.0, left.1, r(0.07), c);
            img.line(top.0, top.1, right.0, right.1, r(0.07), c);
            img.line(left.0, left.1, right.0, right.1, r(0.07), c);
            for (cx, cy) in [top, left, right] {
                img.fill_circle(cx, cy, r(0.13), c);
                img.fill_circle(cx, cy, r(0.06), bg);
            }
        }
        Icon::Qubes => {
            for (u, v) in [(0.08, 0.08), (0.58, 0.08), (0.33, 0.58)] {
                img.rounded_rect(rect(u, v, 0.34, 0.34), r(0.07), c);
            }
        }
        Icon::Desktop => {
            img.rounded_rect(rect(0.06, 0.12, 0.88, 0.66), r(0.08), c);
            img.fill_rect(rect(0.14, 0.30, 0.72, 0.40), bg);
            img.rounded_rect(rect(0.22, 0.84, 0.56, 0.10), r(0.05), c);
        }
    }
}

/// The JuanitOS mark: a ring (the host) holding three isolated cubes
/// (the qubes). `radius` is the outer radius of the ring.
pub fn draw_logo(img: &mut Image, cx: i32, cy: i32, radius: i32, ring: Rgba, cubes: Rgba) {
    let rr = radius as f32;
    let fx = cx as f32;
    let fy = cy as f32;
    img.ring(fx, fy, rr * 0.86, rr, ring);
    let cube = (rr * 0.42) as i32;
    let half = cube / 2;
    let dist = rr * 0.40;
    for k in 0..3 {
        let a = -core::f32::consts::FRAC_PI_2 + k as f32 * core::f32::consts::TAU / 3.0;
        let (sin, cos) = sin_cos(a);
        let x = (fx + dist * cos) as i32 - half;
        let y = (fy + dist * sin) as i32 - half;
        img.rounded_rect(Rect::new(x, y, cube, cube), rr * 0.08, cubes);
    }
}

/// `(sin, cos)` without libm, via a short Taylor series after range
/// reduction. Accurate to ~1e-4 on the reduced range.
pub fn sin_cos(a: f32) -> (f32, f32) {
    use core::f32::consts::{PI, TAU};
    let mut x = a % TAU;
    if x > PI {
        x -= TAU;
    } else if x < -PI {
        x += TAU;
    }
    // Reduce to [-pi/2, pi/2] for the series.
    let (x, sign) = if x > PI / 2.0 {
        (PI - x, 1.0)
    } else if x < -PI / 2.0 {
        (-PI - x, 1.0)
    } else {
        (x, 0.0)
    };
    let x2 = x * x;
    let sin = x * (1.0 - x2 / 6.0 * (1.0 - x2 / 20.0 * (1.0 - x2 / 42.0 * (1.0 - x2 / 72.0))));
    let cos = 1.0 - x2 / 2.0 * (1.0 - x2 / 12.0 * (1.0 - x2 / 30.0 * (1.0 - x2 / 56.0)));
    if sign > 0.0 {
        (sin, -cos)
    } else {
        (sin, cos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: Rgba = Rgba::rgb(0, 0, 0);
    const FG: Rgba = Rgba::rgb(255, 255, 255);

    fn lit_pixels(img: &Image) -> usize {
        img.pixels().iter().filter(|p| p.r > 0).count()
    }

    #[test]
    fn sin_cos_matches_reference_points() {
        use core::f32::consts::{FRAC_PI_2, PI};
        let close = |a: f32, b: f32| {
            let d = a - b;
            (if d < 0.0 { -d } else { d }) < 1e-3
        };
        let (s, c) = sin_cos(0.0);
        assert!(close(s, 0.0) && close(c, 1.0));
        let (s, c) = sin_cos(FRAC_PI_2);
        assert!(close(s, 1.0) && close(c, 0.0));
        let (s, c) = sin_cos(PI);
        assert!(close(s, 0.0) && close(c, -1.0), "{s} {c}");
        let (s, c) = sin_cos(-FRAC_PI_2);
        assert!(close(s, -1.0) && close(c, 0.0));
        let (s, c) = sin_cos(2.0 * PI / 3.0);
        assert!(close(s, 0.866) && close(c, -0.5), "{s} {c}");
    }

    #[test]
    fn every_icon_draws_something_within_its_box() {
        for icon in [
            Icon::Power,
            Icon::Kernel,
            Icon::Storage,
            Icon::Network,
            Icon::Qubes,
            Icon::Desktop,
        ] {
            let mut img = Image::new(64, 64, BG);
            draw_icon(&mut img, icon, 8, 8, 48, FG, BG);
            let lit = lit_pixels(&img);
            assert!(lit > 100, "{icon:?} drew only {lit} pixels");
            // Nothing leaks outside the 48px box (plus 1px of AA slack).
            for y in 0..64 {
                for x in 0..64 {
                    if !(7..=56).contains(&x) || !(7..=56).contains(&y) {
                        assert_eq!(img.get(x, y), Some(BG), "{icon:?} leaked at {x},{y}");
                    }
                }
            }
        }
    }

    #[test]
    fn logo_has_a_hollow_ring() {
        let mut img = Image::new(200, 200, BG);
        draw_logo(&mut img, 100, 100, 80, FG, FG);
        assert_eq!(img.get(100, 23), Some(FG)); // on the ring, top
        assert_eq!(img.get(100, 40), Some(BG)); // just inside the ring
        assert!(lit_pixels(&img) > 2000);
    }
}
