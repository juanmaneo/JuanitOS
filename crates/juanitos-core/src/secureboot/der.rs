// SPDX-License-Identifier: BSD-3-Clause
//! Just enough DER (ITU-T X.690) to build the Authenticode structures and
//! to rewrite what `openssl cms` makes: one-byte tags and definite lengths
//! of up to four bytes, which is all PKCS#7 signatures and certificates
//! use.

use crate::error::{Error, Result};

pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const NULL: u8 = 0x05;
pub const SEQUENCE: u8 = 0x30;
/// `[0]` constructed (EXPLICIT tagging or an IMPLICIT SET/SEQUENCE).
pub const CONTEXT_0: u8 = 0xa0;

/// sha256, 2.16.840.1.101.3.4.2.1, as a complete OBJECT IDENTIFIER.
pub const OID_SHA256: &[u8] = &[
    0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01,
];
/// pkcs7-signedData, 1.2.840.113549.1.7.2.
pub const OID_SIGNED_DATA: &[u8] = &[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x07, 0x02,
];
/// SPC_INDIRECT_DATA_OBJID, 1.3.6.1.4.1.311.2.1.4.
pub const OID_SPC_INDIRECT_DATA: &[u8] = &[
    0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x01, 0x04,
];
/// SPC_PE_IMAGE_DATAOBJ, 1.3.6.1.4.1.311.2.1.15.
pub const OID_SPC_PE_IMAGE_DATA: &[u8] = &[
    0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x01, 0x0f,
];

/// One element: its tag, its contents and its whole encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

pub(crate) fn malformed(what: &str) -> Error {
    Error::Other(format!("malformed DER: {what}"))
}

/// The first element of `input`, and what follows it.
pub fn read(input: &[u8]) -> Result<(Tlv<'_>, &[u8])> {
    let &[tag, first] = input.first_chunk::<2>().ok_or(malformed("truncated"))?;
    if tag & 0x1f == 0x1f {
        return Err(malformed("multi-byte tag"));
    }
    let (header, len) = match first {
        0..=0x7f => (2, usize::from(first)),
        0x81..=0x84 => {
            let n = usize::from(first & 0x7f);
            let bytes = input.get(2..2 + n).ok_or(malformed("truncated"))?;
            let len = bytes
                .iter()
                .fold(0usize, |acc, &b| acc << 8 | usize::from(b));
            (2 + n, len)
        }
        _ => return Err(malformed("unsupported length")),
    };
    let (raw, rest) = input
        .split_at_checked(header.saturating_add(len))
        .ok_or(malformed("truncated"))?;
    let value = raw.get(header..).unwrap_or_default();
    Ok((Tlv { tag, value, raw }, rest))
}

/// `input` as exactly one element.
pub fn parse(input: &[u8]) -> Result<Tlv<'_>> {
    match read(input)? {
        (tlv, []) => Ok(tlv),
        _ => Err(malformed("trailing data")),
    }
}

/// The elements of a constructed value.
pub fn children(value: &[u8]) -> Result<Vec<Tlv<'_>>> {
    let mut out = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let (tlv, next) = read(rest)?;
        out.push(tlv);
        rest = next;
    }
    Ok(out)
}

/// The elements of `tlv`, which must have tag `tag`.
pub fn expect<'a>(tlv: &Tlv<'a>, tag: u8) -> Result<Vec<Tlv<'a>>> {
    if tlv.tag != tag {
        return Err(malformed(&format!(
            "tag {:#04x} where {tag:#04x} was expected",
            tlv.tag
        )));
    }
    children(tlv.value)
}

/// Encode one element.
pub fn encode(tag: u8, value: &[u8]) -> Vec<u8> {
    let len = value.len();
    let mut out = vec![tag];
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let zeros = bytes.iter().take_while(|&&b| b == 0).count();
        let significant = bytes.get(zeros..).unwrap_or_default();
        out.push(0x80 | significant.len() as u8);
        out.extend_from_slice(significant);
    }
    out.extend_from_slice(value);
    out
}

/// A SEQUENCE of already encoded elements.
pub fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    encode(SEQUENCE, &parts.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_and_long_lengths_round_trip() {
        let short = encode(OCTET_STRING, b"alice");
        assert_eq!(short, b"\x04\x05alice");
        let value = vec![7u8; 300];
        let long = encode(SEQUENCE, &value);
        assert_eq!(long.get(..4), Some(&[0x30, 0x82, 0x01, 0x2c][..]));
        let both = [long.as_slice(), &short].concat();
        let (tlv, rest) = read(&both).unwrap();
        assert_eq!((tlv.tag, tlv.value, tlv.raw), (0x30, &value[..], &long[..]));
        assert_eq!(rest, short);
        assert_eq!(encode(NULL, &[]), [0x05, 0x00]);
        assert_eq!(
            encode(OCTET_STRING, &[0; 0x80]).get(..3),
            Some(&[4, 0x81, 0x80][..])
        );
    }

    #[test]
    fn sequences_have_children() {
        let seq = sequence(&[&encode(INTEGER, &[1]), OID_SHA256]);
        let tlv = parse(&seq).unwrap();
        let kids = expect(&tlv, SEQUENCE).unwrap();
        assert_eq!(kids.len(), 2);
        assert_eq!(kids[0].value, [1]);
        assert_eq!(kids[1].raw, OID_SHA256);
        assert!(expect(&tlv, CONTEXT_0)
            .unwrap_err()
            .to_string()
            .contains("tag 0x30 where 0xa0 was expected"));
        assert!(children(&[0x02]).is_err());
    }

    #[test]
    fn malformed_input_is_refused() {
        let error = |input: &[u8]| read(input).unwrap_err().to_string();
        assert_eq!(error(&[]), "malformed DER: truncated");
        assert_eq!(error(&[0x1f, 0x01]), "malformed DER: multi-byte tag");
        assert_eq!(error(&[0x30, 0x80]), "malformed DER: unsupported length");
        assert_eq!(error(&[0x30, 0x85]), "malformed DER: unsupported length");
        assert_eq!(error(&[0x30, 0x82, 0x01]), "malformed DER: truncated");
        assert_eq!(error(&[0x30, 0x05, 0x00]), "malformed DER: truncated");
        assert_eq!(
            parse(&[0x05, 0x00, 0x05]).unwrap_err().to_string(),
            "malformed DER: trailing data"
        );
    }
}
