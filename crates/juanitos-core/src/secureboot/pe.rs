// SPDX-License-Identifier: BSD-3-Clause
//! PE/COFF images as UEFI firmware checks them: the Authenticode digest
//! (computed the way edk2's `DxeImageVerificationLib` does) and the
//! certificate table at the end of the file that carries the signature.

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// `WIN_CERTIFICATE.wRevision` for the current format.
const WIN_CERT_REVISION: u16 = 0x0200;
/// `WIN_CERT_TYPE_PKCS_SIGNED_DATA`: an Authenticode signature.
const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;
/// Index of the certificate table among the data directories.
const CERT_DIRECTORY: usize = 4;

/// Where the parts of an image that matter for signing are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Offset of `CheckSum` in the optional header.
    pub checksum: usize,
    /// Offset of the certificate table's data directory entry.
    pub cert_dir: usize,
    pub size_of_headers: usize,
    /// `(PointerToRawData, SizeOfRawData)` of the sections with data, in
    /// file order.
    pub sections: Vec<(usize, usize)>,
    /// `(offset, size)` of the certificate table; size 0 when unsigned.
    pub cert: (usize, usize),
}

fn not_pe(what: &str) -> Error {
    Error::Other(format!("not a usable PE image: {what}"))
}

fn u16_at(b: &[u8], off: usize) -> Result<usize> {
    b.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map(|c| usize::from(u16::from_le_bytes(*c)))
        .ok_or(not_pe("truncated"))
}

fn u32_at(b: &[u8], off: usize) -> Result<usize> {
    b.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map(|c| u32::from_le_bytes(*c) as usize)
        .ok_or(not_pe("truncated"))
}

/// `n` as a little-endian `u32`, the width of every PE size field; an
/// image this code handles is far below 4 GiB.
fn le32(n: usize) -> [u8; 4] {
    u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes()
}

/// Parse the headers of `image` and check that what they point at is
/// inside the file.
pub fn layout(image: &[u8]) -> Result<Layout> {
    let pe = u32_at(image, 0x3c)?;
    if image.get(..2) != Some(b"MZ") || image.get(pe..pe + 4) != Some(b"PE\0\0") {
        return Err(not_pe("no MZ/PE signature"));
    }
    let sections_count = u16_at(image, pe + 6)?;
    let optional_size = u16_at(image, pe + 20)?;
    let opt = pe + 24;
    let dirs = match u16_at(image, opt)? {
        0x10b => opt + 96,
        0x20b => opt + 112,
        magic => return Err(not_pe(&format!("optional header magic {magic:#x}"))),
    };
    // Read in file order, so that a truncated image fails at every read.
    let size_of_headers = u32_at(image, opt + 60)?;
    if u32_at(image, dirs - 4)? <= CERT_DIRECTORY {
        return Err(not_pe("no certificate table entry"));
    }
    let cert_dir = dirs + 8 * CERT_DIRECTORY;
    let cert = (u32_at(image, cert_dir)?, u32_at(image, cert_dir + 4)?);
    let table = opt + optional_size;
    let mut sections = (0..sections_count)
        .map(|i| {
            let header = table + 40 * i;
            let size = u32_at(image, header + 16)?;
            Ok((u32_at(image, header + 20)?, size))
        })
        .collect::<Result<Vec<_>>>()?;
    sections.retain(|&(_, size)| size > 0);
    sections.sort_unstable();
    let len = image.len();
    if cert_dir + 8 > size_of_headers
        || size_of_headers > len
        || sections.iter().any(|&(at, size)| at + size > len)
        || cert.0 + cert.1 > len
    {
        return Err(not_pe("headers point outside the file"));
    }
    Ok(Layout {
        checksum: opt + 64,
        cert_dir,
        size_of_headers,
        sections,
        cert,
    })
}

/// The Authenticode SHA-256 digest: the headers without the checksum and
/// the certificate table entry, the sections in file order, then any data
/// that follows them, up to the certificate table.
pub fn authenticode_digest(image: &[u8]) -> Result<[u8; 32]> {
    Ok(digest(image, &layout(image)?))
}

fn digest(image: &[u8], l: &Layout) -> [u8; 32] {
    let part = |from: usize, to: usize| image.get(from..to).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(part(0, l.checksum));
    hash.update(part(l.checksum + 4, l.cert_dir));
    hash.update(part(l.cert_dir + 8, l.size_of_headers));
    let mut hashed = l.size_of_headers;
    for &(at, size) in &l.sections {
        hash.update(part(at, at + size));
        hashed += size;
    }
    hash.update(part(hashed, image.len().saturating_sub(l.cert.1)));
    hash.finalize().into()
}

/// An image ready to be signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsigned {
    /// The image without its signature, zero-padded to 8 bytes (where the
    /// certificate table must start).
    pub image: Vec<u8>,
    /// Its Authenticode digest: what the signature covers.
    pub digest: [u8; 32],
    cert_dir: usize,
}

/// Strip the signature of `image`, if any, and digest what is left.
pub fn prepare(image: &[u8]) -> Result<Unsigned> {
    let mut l = layout(image)?;
    let mut out = image.to_vec();
    if l.cert.1 > 0 {
        if l.cert.0 + l.cert.1 != image.len() || l.cert.0 < l.size_of_headers {
            return Err(not_pe("the certificate table is not at the end"));
        }
        out.truncate(l.cert.0);
        out.get_mut(l.cert_dir..l.cert_dir + 8)
            .into_iter()
            .for_each(|entry| entry.fill(0));
    }
    out.resize(out.len().next_multiple_of(8), 0);
    l.cert = (0, 0);
    Ok(Unsigned {
        digest: digest(&out, &l),
        image: out,
        cert_dir: l.cert_dir,
    })
}

impl Unsigned {
    /// The image with `pkcs7` (an Authenticode `SignedData`) as its
    /// certificate table.
    pub fn embed(self, pkcs7: &[u8]) -> Vec<u8> {
        let mut out = self.image;
        let offset = out.len();
        let length = 8 + pkcs7.len();
        let padded = length.next_multiple_of(8);
        out.extend_from_slice(&le32(length));
        out.extend_from_slice(&WIN_CERT_REVISION.to_le_bytes());
        out.extend_from_slice(&WIN_CERT_TYPE_PKCS_SIGNED_DATA.to_le_bytes());
        out.extend_from_slice(pkcs7);
        out.resize(offset + padded, 0);
        let entry = [le32(offset), le32(padded)].concat();
        out.get_mut(self.cert_dir..self.cert_dir + 8)
            .into_iter()
            .for_each(|slot| slot.copy_from_slice(&entry));
        out
    }
}

/// Who signed an image, as far as its certificate table tells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signature {
    /// Not a PE image, or no certificate table.
    None,
    /// The table holds the given certificate (`openssl cms` embeds the
    /// signer's).
    Ours,
    /// Signed by someone else.
    Other,
}

pub fn signature(image: &[u8], cert: &[u8]) -> Signature {
    let table = layout(image)
        .ok()
        .and_then(|l| image.get(l.cert.0..l.cert.0 + l.cert.1))
        .unwrap_or_default();
    if table.is_empty() {
        Signature::None
    } else if !cert.is_empty() && table.windows(cert.len()).any(|w| w == cert) {
        Signature::Ours
    } else {
        Signature::Other
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const OPT: usize = 0x58;

    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A PE32+ image (PE32 when `pe32`): 0x200 bytes of headers, two
    /// sections stored in reverse order, an empty one, then "chuck".
    pub(crate) fn image(pe32: bool) -> Vec<u8> {
        let mut b: Vec<u8> = (0..0x605u32).map(|i| (i * 7 % 251) as u8).collect();
        b[..2].copy_from_slice(b"MZ");
        put32(&mut b, 0x3c, 0x40);
        b[0x40..0x44].copy_from_slice(b"PE\0\0");
        b[0x46..0x48].copy_from_slice(&3u16.to_le_bytes());
        b[0x54..0x56].copy_from_slice(&240u16.to_le_bytes());
        let (magic, dirs) = if pe32 { (0x10bu16, 96) } else { (0x20b, 112) };
        b[OPT..OPT + 2].copy_from_slice(&magic.to_le_bytes());
        put32(&mut b, OPT + 60, 0x200);
        put32(&mut b, OPT + dirs - 4, 16);
        put32(&mut b, OPT + dirs + 32, 0);
        put32(&mut b, OPT + dirs + 36, 0);
        let table = OPT + 240;
        for (i, (at, size)) in [(0x400, 0x200), (0x200, 0x200), (0x600, 0)]
            .into_iter()
            .enumerate()
        {
            put32(&mut b, table + 40 * i + 16, size);
            put32(&mut b, table + 40 * i + 20, at);
        }
        b[0x600..].copy_from_slice(b"chuck");
        b
    }

    fn by_hand(b: &[u8], dirs: usize, tail_end: usize) -> [u8; 32] {
        let cert_dir = OPT + dirs + 32;
        let mut h = Sha256::new();
        h.update(&b[..OPT + 64]);
        h.update(&b[OPT + 68..cert_dir]);
        h.update(&b[cert_dir + 8..0x200]);
        h.update(&b[0x200..0x600]);
        h.update(&b[0x600..tail_end]);
        h.finalize().into()
    }

    #[test]
    fn the_digest_skips_the_checksum_and_the_certificate_entry() {
        for (pe32, dirs) in [(false, 112), (true, 96)] {
            let b = image(pe32);
            assert_eq!(authenticode_digest(&b).unwrap(), by_hand(&b, dirs, b.len()));
            let mut c = b.clone();
            c[OPT + 64] ^= 0xff;
            assert_eq!(authenticode_digest(&c).unwrap(), by_hand(&b, dirs, b.len()));
            c[0x300] ^= 0xff;
            assert_ne!(authenticode_digest(&c).unwrap(), by_hand(&b, dirs, b.len()));
        }
        let l = layout(&image(false)).unwrap();
        assert_eq!(l.sections, [(0x200, 0x200), (0x400, 0x200)]);
        assert_eq!(
            (l.checksum, l.cert_dir, l.cert),
            (OPT + 64, OPT + 144, (0, 0))
        );
    }

    #[test]
    fn signing_appends_a_table_the_digest_ignores() {
        let b = image(false);
        let prepared = prepare(&b).unwrap();
        let u = prepared.image.clone();
        assert_eq!(u.len(), 0x608);
        assert_eq!(&u[..b.len()], &b[..]);
        let digest = authenticode_digest(&u).unwrap();
        assert_eq!(digest, by_hand(&u, 112, u.len()));
        assert_eq!(prepared.digest, digest);

        let signed = prepared.embed(b"alice's signature");
        assert_eq!(signed.len(), 0x608 + 32);
        let l = layout(&signed).unwrap();
        assert_eq!(l.cert, (0x608, 32));
        assert_eq!(&signed[0x608..0x60c], &25u32.to_le_bytes());
        assert_eq!(&signed[0x60c..0x610], &[0x00, 0x02, 0x02, 0x00]);
        assert_eq!(&signed[0x610..0x621], b"alice's signature");
        assert_eq!(authenticode_digest(&signed).unwrap(), digest);
        let again = prepare(&signed).unwrap();
        assert_eq!((again.image, again.digest), (u.clone(), digest));

        assert_eq!(signature(&signed, b"alice"), Signature::Ours);
        assert_eq!(signature(&signed, b"bob"), Signature::Other);
        assert_eq!(signature(&signed, b""), Signature::Other);
        assert_eq!(signature(&u, b"alice"), Signature::None);
        assert_eq!(signature(b"chuck", b"alice"), Signature::None);
    }

    #[test]
    fn broken_images_are_refused() {
        let error = |edit: &dyn Fn(&mut Vec<u8>)| {
            let mut b = image(false);
            edit(&mut b);
            layout(&b).unwrap_err().to_string()
        };
        let msg = |what: &str| format!("not a usable PE image: {what}");
        assert_eq!(error(&|b| b.truncate(0x3c)), msg("truncated"));
        for len in 0x44..0x1b0 {
            assert_eq!((len, error(&|b| b.truncate(len))), (len, msg("truncated")));
        }
        assert_eq!(error(&|b| b[0] = b'X'), msg("no MZ/PE signature"));
        assert_eq!(error(&|b| b[0x41] = b'X'), msg("no MZ/PE signature"));
        assert_eq!(
            error(&|b| b[OPT] = 0x0c),
            msg("optional header magic 0x20c")
        );
        assert_eq!(
            error(&|b| put32(b, OPT + 108, 4)),
            msg("no certificate table entry")
        );
        assert_eq!(error(&|b| b[0x46] = 0xff), msg("truncated"));
        let outside = msg("headers point outside the file");
        assert_eq!(error(&|b| put32(b, OPT + 60, 0xe0)), outside);
        assert_eq!(error(&|b| put32(b, OPT + 60, 0x700)), outside);
        assert_eq!(error(&|b| put32(b, OPT + 256, 0x500)), outside);
        assert_eq!(error(&|b| put32(b, OPT + 148, 0x700)), outside);

        let mut b = image(false);
        put32(&mut b, OPT + 144, 0x600);
        put32(&mut b, OPT + 148, 4);
        assert_eq!(
            prepare(&b).unwrap_err().to_string(),
            msg("the certificate table is not at the end")
        );
        // A table that starts inside the headers would be cut through them.
        let mut b = image(false);
        put32(&mut b, OPT + 144, 0x100);
        put32(&mut b, OPT + 148, 0x505);
        assert_eq!(
            prepare(&b).unwrap_err().to_string(),
            msg("the certificate table is not at the end")
        );
        assert!(authenticode_digest(b"MZ").is_err());
        assert!(prepare(b"MZ").is_err());
        assert_eq!(le32(usize::MAX), [0xff; 4]);
    }
}
