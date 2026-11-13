// SPDX-License-Identifier: BSD-3-Clause
//! The PKCS#7 signatures UEFI firmware checks, made from what `openssl cms
//! -sign` writes: an Authenticode signature for an image (the signed
//! content is a `SpcIndirectDataContent` holding the image digest), and
//! the bare `SignedData` of an authenticated variable write.

use super::der::{
    self, Tlv, BIT_STRING, CONTEXT_0, INTEGER, NULL, OCTET_STRING, OID_SHA256, OID_SIGNED_DATA,
    OID_SPC_PE_IMAGE_DATA, SEQUENCE,
};
use crate::error::Result;

/// The OID `openssl cms -econtent_type` takes for Authenticode content.
pub const SPC_INDIRECT_DATA: &str = "1.3.6.1.4.1.311.2.1.4";

/// `SpcIndirectDataContent` for an image digest (the whole SEQUENCE).
/// Firmware takes the digest from its last 32 bytes.
pub fn indirect_data(digest: &[u8; 32]) -> Vec<u8> {
    der::encode(SEQUENCE, &indirect_data_content(digest))
}

/// The contents of [`indirect_data`], without the SEQUENCE header: what
/// the signature covers.
pub fn indirect_data_content(digest: &[u8; 32]) -> Vec<u8> {
    let obsolete: Vec<u8> = "<<<Obsolete>>>"
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    // SpcPeImageData: no flags, file = [0] { [2] { unicode [0] } }.
    let file = der::encode(CONTEXT_0, &der::encode(0xa2, &der::encode(0x80, &obsolete)));
    let image_data = der::sequence(&[&[BIT_STRING, 1, 0], &file]);
    let data = der::sequence(&[OID_SPC_PE_IMAGE_DATA, &image_data]);
    let algorithm = der::sequence(&[OID_SHA256, &der::encode(NULL, &[])]);
    let digest_info = der::sequence(&[&algorithm, &der::encode(OCTET_STRING, digest)]);
    [data, digest_info].concat()
}

fn not_cms() -> crate::Error {
    der::malformed("not an openssl cms SignedData")
}

/// The SignedData inside a ContentInfo.
fn signed_data<'a>(info: &Tlv<'a>) -> Result<Tlv<'a>> {
    match der::expect(info, SEQUENCE)?.as_slice() {
        [oid, wrapped] if oid.raw == OID_SIGNED_DATA => {
            match der::expect(wrapped, CONTEXT_0)?[..] {
                [signed] => Ok(signed),
                _ => Err(not_cms()),
            }
        }
        _ => Err(not_cms()),
    }
}

/// The bare SignedData of a detached `openssl cms -sign`, as
/// EFI_VARIABLE_AUTHENTICATION_2 carries it.
pub fn bare_signed_data(cms: &[u8]) -> Result<Vec<u8>> {
    Ok(signed_data(&der::parse(cms)?)?.raw.to_vec())
}

/// An Authenticode signature from `openssl cms -sign -nodetach
/// -econtent_type` [`SPC_INDIRECT_DATA`] over the *contents* of an
/// [`indirect_data`] (what PKCS#7 v1.5 and the firmware digest): the
/// content goes back into its SEQUENCE, and SignedData is version 1. The
/// signed attributes are unchanged, so the signature still holds.
pub fn authenticode(cms: &[u8]) -> Result<Vec<u8>> {
    let signed = signed_data(&der::parse(cms)?)?;
    let parts = der::expect(&signed, SEQUENCE)?;
    let [_version, algorithms, encapsulated, rest @ ..] = parts.as_slice() else {
        return Err(not_cms());
    };
    let [content_type, explicit] = der::expect(encapsulated, SEQUENCE)?[..] else {
        return Err(not_cms());
    };
    let [octets] = der::expect(&explicit, CONTEXT_0)?[..] else {
        return Err(not_cms());
    };
    if octets.tag != OCTET_STRING {
        return Err(not_cms());
    }
    let content = der::encode(CONTEXT_0, &der::encode(SEQUENCE, octets.value));
    let mut body = [
        der::encode(INTEGER, &[1]),
        algorithms.raw.to_vec(),
        der::sequence(&[content_type.raw, &content]),
    ]
    .concat();
    for part in rest {
        body.extend_from_slice(part.raw);
    }
    let signed = der::encode(SEQUENCE, &body);
    Ok(der::sequence(&[
        OID_SIGNED_DATA,
        &der::encode(CONTEXT_0, &signed),
    ]))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// What `openssl cms` makes, reduced to its shape.
    pub(crate) fn cms(content: &[u8], version: u8) -> Vec<u8> {
        let encapsulated = der::sequence(&[
            &der::encode(0x06, b"oid"),
            &der::encode(CONTEXT_0, &der::encode(OCTET_STRING, content)),
        ]);
        let signed = der::sequence(&[
            &der::encode(INTEGER, &[version]),
            &der::encode(0x31, b"algs"),
            &encapsulated,
            &der::encode(CONTEXT_0, b"certs"),
            &der::encode(0x31, b"signers"),
        ]);
        der::sequence(&[OID_SIGNED_DATA, &der::encode(CONTEXT_0, &signed)])
    }

    #[test]
    fn the_indirect_data_ends_with_the_digest() {
        let digest = [0xab; 32];
        let idc = indirect_data(&digest);
        assert!(idc.ends_with(&digest));
        let top = der::parse(&idc).unwrap();
        let parts = der::expect(&top, SEQUENCE).unwrap();
        let data = der::expect(&parts[0], SEQUENCE).unwrap();
        assert_eq!(data[0].raw, OID_SPC_PE_IMAGE_DATA);
        let image = der::expect(&data[1], SEQUENCE).unwrap();
        assert_eq!(image[0].raw, [3, 1, 0]);
        assert_eq!(image[1].raw.len(), 2 + 2 + 2 + 28);
        assert_eq!(&image[1].raw[..6], &[0xa0, 32, 0xa2, 30, 0x80, 28]);
        let info = der::expect(&parts[1], SEQUENCE).unwrap();
        assert_eq!(info[0].raw, der::sequence(&[OID_SHA256, &[5, 0]]));
    }

    #[test]
    fn authenticode_puts_the_content_back_into_its_sequence() {
        let out = authenticode(&cms(b"alice", 3)).unwrap();
        let signed = signed_data(&der::parse(&out).unwrap()).unwrap();
        let parts = der::expect(&signed, SEQUENCE).unwrap();
        assert_eq!(parts[0].raw, [2, 1, 1]);
        assert_eq!(parts[1].raw, der::encode(0x31, b"algs"));
        let encapsulated = der::expect(&parts[2], SEQUENCE).unwrap();
        assert_eq!(
            encapsulated[1].raw,
            der::encode(CONTEXT_0, &der::encode(SEQUENCE, b"alice"))
        );
        assert_eq!(parts[3].raw, der::encode(CONTEXT_0, b"certs"));
        assert_eq!(parts[4].raw, der::encode(0x31, b"signers"));
        assert_eq!(
            bare_signed_data(&cms(b"bob", 1)).unwrap(),
            signed_data(&der::parse(&cms(b"bob", 1)).unwrap())
                .unwrap()
                .raw
        );
    }

    #[test]
    fn other_structures_are_refused() {
        let refused = |input: &[u8]| {
            let a = authenticode(input).unwrap_err().to_string();
            assert_eq!(a.get(..13), Some("malformed DER"));
        };
        let wrap =
            |signed: &[u8]| der::sequence(&[OID_SIGNED_DATA, &der::encode(CONTEXT_0, signed)]);
        refused(&der::sequence(&[
            OID_SHA256,
            &der::encode(CONTEXT_0, &[5, 0]),
        ]));
        refused(&der::sequence(&[OID_SIGNED_DATA]));
        refused(&der::sequence(&[
            OID_SIGNED_DATA,
            &der::encode(CONTEXT_0, &[5, 0, 5, 0]),
        ]));
        refused(&der::sequence(&[OID_SIGNED_DATA, &[5, 0]]));
        refused(&wrap(&[5, 0]));
        refused(&wrap(&der::sequence(&[&[2, 1, 1]])));
        refused(&wrap(&der::sequence(&[&[2, 1, 1], &[0x31, 0], &[5, 0]])));
        let encapsulated = |inner: &[u8]| wrap(&der::sequence(&[&[2, 1, 1], &[0x31, 0], inner]));
        refused(&encapsulated(&der::sequence(&[&[6, 0]])));
        refused(&encapsulated(&der::sequence(&[&[6, 0], &[5, 0]])));
        refused(&encapsulated(&der::sequence(&[
            &[6, 0],
            &der::encode(CONTEXT_0, &[5, 0, 5, 0]),
        ])));
        refused(&encapsulated(&der::sequence(&[
            &[6, 0],
            &der::encode(CONTEXT_0, &[5, 0]),
        ])));
        refused(b"");
        assert!(bare_signed_data(&[5, 0]).is_err());
        assert!(bare_signed_data(b"").is_err());
    }
}
