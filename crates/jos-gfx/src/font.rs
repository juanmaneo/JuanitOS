//! Anti-aliased text in Open Sans, the interface font of COSMIC.
//!
//! The face is embedded (`fonts/OpenSans-Regular.ttf`, SIL OFL 1.1, see
//! `fonts/OFL.txt`), parsed by `skrifa` and rasterised with coverage by
//! `ab_glyph_rasterizer`; both are `no_std` and run in the UEFI pre-loader
//! as well as on the host.
//!
//! Sizes are an integer `scale` and `y` is the top of the capitals: text is
//! `7 * scale` pixels from cap top to baseline. Lowercase descenders hang
//! below that box. Open Sans covers Latin, Greek and Cyrillic; any other
//! character keeps the advance of the face's (empty) `.notdef` glyph, so a
//! caret still moves past it.

use ab_glyph_rasterizer::{point, Point, Rasterizer};
use alloc::string::String;
use alloc::vec::Vec;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::{FontRef, GlyphId, MetadataProvider};

use crate::color::Rgba;
use crate::image::Image;

/// Cap height in pixels at scale 1.
pub const CAP_HEIGHT: i32 = 7;

const OPEN_SANS: &[u8] = include_bytes!("../fonts/OpenSans-Regular.ttf");

struct Face {
    font: FontRef<'static>,
    /// Em size over cap height, both in font units.
    em_per_cap: f32,
}

/// A parsed font face. A face that fails to parse draws nothing and
/// measures zero, so text can never bring a frame down.
pub struct Font {
    face: Option<Face>,
}

impl Font {
    /// The embedded Open Sans Regular.
    pub fn builtin() -> Self {
        Self::from_bytes(OPEN_SANS)
    }

    pub fn from_bytes(data: &'static [u8]) -> Self {
        let face = FontRef::new(data).ok().map(|font| {
            let m = font.metrics(Size::unscaled(), LocationRef::default());
            let em = f32::from(m.units_per_em);
            // A face without an OS/2 cap height: the usual 70 % of the em.
            let cap = m.cap_height.unwrap_or(em * 0.7);
            Face {
                font,
                em_per_cap: em / cap,
            }
        });
        Font { face }
    }

    /// The face and its pixels-per-em for capitals `7 * scale` high.
    fn sized(&self, scale: i32) -> Option<(&FontRef<'static>, Size)> {
        let face = self.face.as_ref()?;
        let ppem = text_height(scale) as f32 * face.em_per_cap;
        Some((&face.font, Size::new(ppem)))
    }

    /// Exact (fractional) advance of `text`.
    fn advance(&self, text: &str, scale: i32) -> f32 {
        let Some((font, size)) = self.sized(scale) else {
            return 0.0;
        };
        let charmap = font.charmap();
        let metrics = font.glyph_metrics(size, LocationRef::default());
        text.chars()
            .map(|c| glyph_advance(&metrics, charmap.map(c).unwrap_or_default()))
            .sum()
    }

    /// Width in pixels of `text`, rounded to the nearest pixel.
    pub fn text_width(&self, text: &str, scale: i32) -> i32 {
        (self.advance(text, scale) + 0.5) as i32
    }

    /// `text` cut to at most `max_width` pixels, with a trailing `...` when
    /// something was cut.
    pub fn ellipsize(&self, text: &str, max_width: i32, scale: i32) -> String {
        if self.text_width(text, scale) <= max_width {
            return String::from(text);
        }
        let room = max_width as f32 - self.advance("...", scale);
        let mut out = String::new();
        for ch in text.chars() {
            out.push(ch);
            if self.advance(&out, scale) > room {
                out.pop();
                break;
            }
        }
        out.push_str("...");
        out
    }

    /// Draw `text` with the top of its capitals at `y`, starting at `x`.
    pub fn draw_text(&self, img: &mut Image, x: i32, y: i32, text: &str, scale: i32, c: Rgba) {
        let Some((font, size)) = self.sized(scale) else {
            return;
        };
        let charmap = font.charmap();
        let metrics = font.glyph_metrics(size, LocationRef::default());
        let outlines = font.outline_glyphs();
        let baseline = (y + text_height(scale)) as f32;
        let mut pen = x as f32;
        for ch in text.chars() {
            let id = charmap.map(ch).unwrap_or_default();
            let mut path = Path::new(pen, baseline);
            // A missing or broken glyph leaves the path empty or partial:
            // drawn as far as it goes, never an error.
            let settings = DrawSettings::unhinted(size, LocationRef::default());
            let _ = outlines
                .get(id)
                .map(|glyph| glyph.draw(settings, &mut path));
            path.fill(img, c);
            pen += glyph_advance(&metrics, id);
        }
    }

    /// Draw `text` horizontally centred on `cx`.
    pub fn draw_text_centered(
        &self,
        img: &mut Image,
        cx: i32,
        y: i32,
        text: &str,
        scale: i32,
        c: Rgba,
    ) {
        let x = cx - self.text_width(text, scale) / 2;
        self.draw_text(img, x, y, text, scale, c);
    }
}

fn glyph_advance(metrics: &skrifa::metrics::GlyphMetrics, id: GlyphId) -> f32 {
    metrics.advance_width(id).unwrap_or_default()
}

enum Segment {
    Line(Point, Point),
    Quad(Point, Point, Point),
    Cubic(Point, Point, Point, Point),
}

/// A glyph outline in screen pixels (y down), with the bounding box of all
/// its points.
struct Path {
    origin: Point,
    start: Point,
    last: Point,
    min: Point,
    max: Point,
    segments: Vec<Segment>,
}

impl Path {
    fn new(x: f32, baseline: f32) -> Self {
        let origin = point(x, baseline);
        Path {
            origin,
            start: origin,
            last: origin,
            min: point(f32::MAX, f32::MAX),
            max: point(f32::MIN, f32::MIN),
            segments: Vec::new(),
        }
    }

    /// Font units (y up, already scaled to pixels) to screen pixels.
    fn at(&mut self, x: f32, y: f32) -> Point {
        let p = point(self.origin.x + x, self.origin.y - y);
        self.min = point(self.min.x.min(p.x), self.min.y.min(p.y));
        self.max = point(self.max.x.max(p.x), self.max.y.max(p.y));
        p
    }

    fn push(&mut self, s: Segment, to: Point) {
        self.segments.push(s);
        self.last = to;
    }

    /// Composite the outline's coverage onto `img`.
    fn fill(&self, img: &mut Image, c: Rgba) {
        if self.segments.is_empty() {
            return;
        }
        // One spare pixel on each side: `as` truncates towards zero, which
        // is not `floor` left of or above the image.
        let (x0, y0) = (self.min.x as i32 - 1, self.min.y as i32 - 1);
        let w = (self.max.x as i32 + 2 - x0) as usize;
        let h = (self.max.y as i32 + 2 - y0) as usize;
        let o = |p: Point| point(p.x - x0 as f32, p.y - y0 as f32);
        let mut r = Rasterizer::new(w, h);
        for s in &self.segments {
            match *s {
                Segment::Line(a, b) => r.draw_line(o(a), o(b)),
                Segment::Quad(a, b, d) => r.draw_quad(o(a), o(b), o(d)),
                Segment::Cubic(a, b, d, e) => r.draw_cubic(o(a), o(b), o(d), o(e)),
            }
        }
        r.for_each_pixel_2d(|x, y, coverage| {
            let alpha = (coverage.min(1.0) * 255.0 + 0.5) as u8;
            img.blend(x0 + x as i32, y0 + y as i32, c.with_alpha_scaled(alpha));
        });
    }
}

impl OutlinePen for Path {
    fn move_to(&mut self, x: f32, y: f32) {
        self.start = self.at(x, y);
        self.last = self.start;
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let b = self.at(x, y);
        self.push(Segment::Line(self.last, b), b);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        let c0 = self.at(cx0, cy0);
        let b = self.at(x, y);
        self.push(Segment::Quad(self.last, c0, b), b);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let c0 = self.at(cx0, cy0);
        let c1 = self.at(cx1, cy1);
        let b = self.at(x, y);
        self.push(Segment::Cubic(self.last, c0, c1, b), b);
    }

    fn close(&mut self) {
        let (a, b) = (self.last, self.start);
        self.push(Segment::Line(a, b), b);
    }
}

/// What a password of `len` characters is shown as.
pub fn mask(len: usize) -> String {
    "\u{2022}".repeat(len)
}

/// Distance in pixels from the top of the capitals to the baseline.
pub fn text_height(scale: i32) -> i32 {
    CAP_HEIGHT * scale.max(1)
}

/// [`Font::text_width`] with the embedded face.
pub fn text_width(text: &str, scale: i32) -> i32 {
    Font::builtin().text_width(text, scale)
}

/// [`Font::ellipsize`] with the embedded face.
pub fn ellipsize(text: &str, max_width: i32, scale: i32) -> String {
    Font::builtin().ellipsize(text, max_width, scale)
}

/// [`Font::draw_text`] with the embedded face.
pub fn draw_text(img: &mut Image, x: i32, y: i32, text: &str, scale: i32, c: Rgba) {
    Font::builtin().draw_text(img, x, y, text, scale, c);
}

/// [`Font::draw_text_centered`] with the embedded face.
pub fn draw_text_centered(img: &mut Image, cx: i32, y: i32, text: &str, scale: i32, c: Rgba) {
    Font::builtin().draw_text_centered(img, cx, y, text, scale, c);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BG: Rgba = Rgba::rgb(0, 0, 0);
    const FG: Rgba = Rgba::rgb(255, 255, 255);

    /// Bounding box (x0, y0, x1, y1), inclusive, of the non-background
    /// pixels, and whether some of them are partial (anti-aliased).
    fn ink(img: &Image) -> Option<(i32, i32, i32, i32, bool)> {
        let mut found: Option<(i32, i32, i32, i32, bool)> = None;
        for y in 0..img.height() as i32 {
            for x in 0..img.width() as i32 {
                let p = img.get(x, y).unwrap();
                if p != BG {
                    let partial = p != FG;
                    found = Some(match found {
                        None => (x, y, x, y, partial),
                        Some((x0, y0, x1, y1, aa)) => {
                            (x0.min(x), y0.min(y), x1.max(x), y1.max(y), aa | partial)
                        }
                    });
                }
            }
        }
        found
    }

    #[test]
    fn the_cap_height_is_read_from_the_face() {
        let font = Font::builtin();
        let face = font.face.as_ref().unwrap();
        // Open Sans: 2048 units per em, capitals 1462 units high, which is
        // exactly the height of the 'H' outline.
        assert_eq!(face.em_per_cap, 2048.0 / 1462.0);
        let metrics = face
            .font
            .glyph_metrics(Size::unscaled(), LocationRef::default());
        let h = metrics
            .bounds(face.font.charmap().map('H').unwrap())
            .unwrap();
        assert_eq!(h.y_max - h.y_min, 1462.0);
    }

    #[test]
    fn cubic_outlines_are_filled_too() {
        // TrueType glyphs are quadratic; CFF faces draw cubics.
        let mut path = Path::new(10.0, 30.0);
        path.move_to(0.0, 0.0);
        path.curve_to(0.0, 20.0, 20.0, 20.0, 20.0, 0.0);
        path.close();
        let mut img = Image::new(40, 40, BG);
        path.fill(&mut img, FG);
        let (x0, y0, x1, y1, aa) = ink(&img).unwrap();
        assert!(
            x0 >= 9 && x1 <= 31 && y0 >= 14 && y1 <= 30,
            "{x0},{y0} {x1},{y1}"
        );
        assert!(aa);
    }

    #[test]
    fn capitals_fill_the_cap_box_with_soft_edges() {
        for scale in 1..=4 {
            let mut img = Image::new(120, 60, BG);
            draw_text(&mut img, 10, 20, "HI", scale, FG);
            let (x0, y0, _, y1, aa) = ink(&img).unwrap();
            assert_eq!(y0, 20, "cap top at y, scale {scale}");
            assert_eq!(y1, 20 + text_height(scale) - 1, "baseline, scale {scale}");
            assert!(
                (10..=10 + scale).contains(&x0),
                "left bearing, scale {scale}"
            );
            assert!(aa, "anti-aliased, scale {scale}");
        }
    }

    #[test]
    fn descenders_hang_below_the_baseline() {
        let mut img = Image::new(60, 60, BG);
        draw_text(&mut img, 4, 10, "gy", 3, FG);
        let (_, _, _, y1, _) = ink(&img).unwrap();
        assert!(y1 >= 10 + text_height(3) + 3, "{y1}");
    }

    #[test]
    fn widths_follow_the_advances() {
        assert_eq!(text_width("", 2), 0);
        assert!(text_width("  ", 2) > text_width(" ", 2));
        let one = text_width("JUANITOS", 1);
        assert!(one > 0);
        assert!((text_width("JUANITOS", 3) - 3 * one).abs() <= 3);
        assert!(text_width("W", 2) > text_width("i", 2));
        assert_eq!(text_height(3), 21);
        assert_eq!(text_height(0), 7);
    }

    #[test]
    fn centred_text_is_centred() {
        let mut img = Image::new(200, 40, BG);
        draw_text_centered(&mut img, 100, 10, "alice", 2, FG);
        let (x0, _, x1, _, _) = ink(&img).unwrap();
        assert!(((x0 + x1) / 2 - 100).abs() <= 2, "{x0}..{x1}");
    }

    #[test]
    fn spaces_and_unknown_characters() {
        let mut img = Image::new(40, 40, BG);
        draw_text(&mut img, 2, 2, "   ", 2, FG);
        assert!(ink(&img).is_none());
        draw_text(&mut img, 2, 2, "\u{2603}", 2, FG);
        assert!(ink(&img).is_none());
        assert!(text_width("\u{2603}", 2) > 0);
        draw_text(&mut img, 2, 2, "\u{e9}\u{3b1}\u{436}", 2, FG);
        assert!(ink(&img).is_some());
    }

    #[test]
    fn masks_are_visible_bullets() {
        assert_eq!(mask(0), "");
        assert_eq!(mask(3), "\u{2022}\u{2022}\u{2022}");
        let mut img = Image::new(60, 40, BG);
        draw_text(&mut img, 2, 10, &mask(3), 2, FG);
        assert!(ink(&img).is_some());
    }

    #[test]
    fn ellipsize_cuts_long_text() {
        assert_eq!(ellipsize("short", 1000, 1), "short");
        let cut = ellipsize("a fairly long line of text", 60, 1);
        assert!(cut.ends_with("..."), "{cut}");
        assert!(cut.len() > 3, "{cut}");
        assert!(text_width(&cut, 1) <= 60, "{cut}");
        assert_eq!(ellipsize("abc", 0, 1), "...");
    }

    #[test]
    fn an_invalid_face_draws_and_measures_nothing() {
        let font = Font::from_bytes(b"chuck's font");
        let mut img = Image::new(40, 40, BG);
        font.draw_text(&mut img, 2, 2, "bob", 2, FG);
        font.draw_text_centered(&mut img, 20, 2, "bob", 2, FG);
        assert!(ink(&img).is_none());
        assert_eq!(font.text_width("bob", 2), 0);
        assert_eq!(font.ellipsize("bob", 10, 2), "bob");
    }
}
