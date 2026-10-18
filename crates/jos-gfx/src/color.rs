//! Colours and the JuanitOS palette.

/// A straight-alpha RGBA colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Rgba { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Rgba { r, g, b, a: 255 }
    }

    pub const TRANSPARENT: Rgba = Rgba::new(0, 0, 0, 0);

    /// Same colour with its alpha multiplied by `k` (0..=255).
    pub fn with_alpha_scaled(self, k: u8) -> Self {
        Rgba {
            a: ((self.a as u32 * k as u32 + 127) / 255) as u8,
            ..self
        }
    }

    /// Source-over compositing of `self` onto `dst`. The result is opaque
    /// whenever `dst` is: backgrounds are always painted first.
    pub fn over(self, dst: Rgba) -> Rgba {
        match self.a {
            0 => dst,
            255 => self,
            a => {
                let sa = a as u32;
                let da = dst.a as u32;
                // At least `sa`, which is 1..=254 here: never zero.
                let out_a = sa + da * (255 - sa) / 255;
                let ch = |s: u8, d: u8| -> u8 {
                    let s = s as u32;
                    let d = d as u32;
                    ((s * sa + d * da * (255 - sa) / 255) / out_a) as u8
                };
                Rgba {
                    r: ch(self.r, dst.r),
                    g: ch(self.g, dst.g),
                    b: ch(self.b, dst.b),
                    a: out_a as u8,
                }
            }
        }
    }

    /// Linear interpolation towards `other` by `t` (0..=255).
    pub fn mix(self, other: Rgba, t: u8) -> Rgba {
        let t = t as u32;
        let ch = |a: u8, b: u8| ((a as u32 * (255 - t) + b as u32 * t) / 255) as u8;
        Rgba {
            r: ch(self.r, other.r),
            g: ch(self.g, other.g),
            b: ch(self.b, other.b),
            a: ch(self.a, other.a),
        }
    }
}

/// The palette. Pure black background (what `vt(4)` blanks the screen to,
/// so the kernel splash and our frames are indistinguishable), a single
/// warm accent for "lit" elements, a cool grey for everything dormant.
pub struct Theme {
    pub background: Rgba,
    pub accent: Rgba,
    pub accent_soft: Rgba,
    pub dim: Rgba,
    pub text: Rgba,
    pub text_muted: Rgba,
    pub highlight: Rgba,
}

pub const THEME: Theme = Theme {
    background: Rgba::rgb(0x00, 0x00, 0x00),
    accent: Rgba::rgb(0xf5, 0xa6, 0x23),
    accent_soft: Rgba::new(0xf5, 0xa6, 0x23, 96),
    dim: Rgba::rgb(0x2c, 0x31, 0x39),
    text: Rgba::rgb(0xd8, 0xdc, 0xe2),
    text_muted: Rgba::rgb(0x6c, 0x74, 0x80),
    highlight: Rgba::rgb(0x1c, 0x22, 0x2b),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_opaque_and_transparent_are_identities() {
        let bg = Rgba::rgb(10, 20, 30);
        let fg = Rgba::rgb(200, 100, 50);
        assert_eq!(fg.over(bg), fg);
        assert_eq!(Rgba::TRANSPARENT.over(bg), bg);
    }

    #[test]
    fn over_half_alpha_blends() {
        let bg = Rgba::rgb(0, 0, 0);
        let fg = Rgba::new(255, 255, 255, 128);
        let out = fg.over(bg);
        assert_eq!(out.a, 255);
        assert!((126..=129).contains(&out.r));
    }

    #[test]
    fn alpha_scaling_and_mixing() {
        let c = Rgba::rgb(255, 0, 0);
        assert_eq!(c.with_alpha_scaled(0).a, 0);
        assert_eq!(c.with_alpha_scaled(255).a, 255);
        let m = c.mix(Rgba::rgb(0, 0, 255), 255);
        assert_eq!((m.r, m.b), (0, 255));
    }
}
