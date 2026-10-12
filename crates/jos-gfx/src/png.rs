//! PNG export (feature `png`, pulls in `std`): used by `jos-release` to
//! produce the kernel splash image and by `jos-splash render` for
//! previews. The kernel's `vt(4)` wants a 32-bit RGBA image, which is what
//! the loader's decoder produces from any 8-bit PNG.

use std::io::Write;
use std::vec::Vec;

use crate::image::Image;

#[derive(Debug)]
pub struct PngError(pub std::string::String);

impl core::fmt::Display for PngError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "png: {}", self.0)
    }
}

impl std::error::Error for PngError {}

/// Encode `img` as an 8-bit RGBA PNG.
pub fn encode(img: &Image) -> Result<Vec<u8>, PngError> {
    let mut data = Vec::with_capacity(img.pixels().len() * 4);
    for p in img.pixels() {
        data.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, img.width(), img.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc
            .write_header()
            .map_err(|e| PngError(std::format!("{e}")))?;
        w.write_image_data(&data)
            .map_err(|e| PngError(std::format!("{e}")))?;
    }
    Ok(out)
}

/// Encode and write to `path`.
pub fn write(img: &Image, path: &std::path::Path) -> Result<(), PngError> {
    let bytes = encode(img)?;
    let mut f = std::fs::File::create(path)
        .map_err(|e| PngError(std::format!("{}: {e}", path.display())))?;
    f.write_all(&bytes)
        .map_err(|e| PngError(std::format!("{}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::Rgba;

    #[test]
    fn encodes_a_valid_png() {
        let mut img = Image::new(8, 4, Rgba::rgb(1, 2, 3));
        img.blend(0, 0, Rgba::rgb(250, 251, 252));
        let bytes = encode(&img).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = std::vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (8, 4));
        assert_eq!(info.color_type, png::ColorType::Rgba);
        assert_eq!(&buf[..4], &[250, 251, 252, 255]);
        assert_eq!(&buf[4..8], &[1, 2, 3, 255]);
    }
}
