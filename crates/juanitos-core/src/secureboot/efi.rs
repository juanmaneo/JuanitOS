// SPDX-License-Identifier: BSD-3-Clause
//! The UEFI side of Secure Boot keys: GUIDs, signature lists (the
//! contents of PK, KEK and db) and the time-based authenticated writes
//! (`.auth` files) that put them into the firmware.

use std::fmt;

use crate::error::{Error, Result};

/// An EFI_GUID: its first three fields are stored little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

impl Guid {
    pub fn bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        let parts = [
            &self.0.to_le_bytes()[..],
            &self.1.to_le_bytes(),
            &self.2.to_le_bytes(),
            &self.3,
        ]
        .concat();
        out.copy_from_slice(&parts);
        out
    }
}

impl fmt::Display for Guid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g, h, i] = self.3;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{a:02x}{b:02x}-{c:02x}{d:02x}{e:02x}{g:02x}{h:02x}{i:02x}",
            self.0, self.1, self.2
        )
    }
}

/// EFI_GLOBAL_VARIABLE: PK, KEK, SecureBoot, SetupMode.
pub const GLOBAL_VARIABLE: Guid = Guid(
    0x8be4df61,
    0x93ca,
    0x11d2,
    [0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c],
);
/// EFI_IMAGE_SECURITY_DATABASE_GUID: db.
pub const IMAGE_SECURITY_DATABASE: Guid = Guid(
    0xd719b2cb,
    0x3d3a,
    0x4596,
    [0xa3, 0xbc, 0xda, 0xd0, 0x0e, 0x67, 0x65, 0x6f],
);
/// EFI_CERT_X509_GUID: a signature list of DER certificates.
pub const CERT_X509: Guid = Guid(
    0xa5c059a1,
    0x94e4,
    0x4aa7,
    [0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72],
);
/// EFI_CERT_TYPE_PKCS7_GUID: the signature of an authenticated write.
pub const CERT_TYPE_PKCS7: Guid = Guid(
    0x4aafd29d,
    0x68df,
    0x49ee,
    [0x8a, 0xa9, 0x34, 0x7d, 0x37, 0x56, 0x65, 0xa7],
);
/// The SignatureOwner of the certificates JuanitOS enrolls.
pub const OWNER: Guid = Guid(
    0x9d4c1a7e,
    0x5b2f,
    0x4e83,
    [0xa6, 0xd1, 0x3c, 0x7f, 0x0e, 0x92, 0xb8, 0x45],
);

/// NON_VOLATILE | BOOTSERVICE_ACCESS | RUNTIME_ACCESS |
/// TIME_BASED_AUTHENTICATED_WRITE_ACCESS: how PK, KEK and db are stored.
pub const AUTH_ATTRIBUTES: u32 = 0x27;
/// WIN_CERTIFICATE revision and WIN_CERT_TYPE_EFI_GUID.
const WIN_CERT_REVISION: u16 = 0x0200;
const WIN_CERT_TYPE_EFI_GUID: u16 = 0x0ef1;
/// EFI_SIGNATURE_LIST header: type GUID, list, header and entry sizes.
const LIST_HEADER: usize = 28;

/// The Secure Boot key variables, in the order of the hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Var {
    /// The platform key: whoever holds it controls KEK.
    Pk,
    /// The key exchange keys: they may change db.
    Kek,
    /// The signature database: images it accepts boot.
    Db,
}

impl Var {
    pub const ALL: [Var; 3] = [Var::Pk, Var::Kek, Var::Db];

    pub fn name(self) -> &'static str {
        match self {
            Var::Pk => "PK",
            Var::Kek => "KEK",
            Var::Db => "db",
        }
    }

    pub fn guid(self) -> Guid {
        match self {
            Var::Db => IMAGE_SECURITY_DATABASE,
            Var::Pk | Var::Kek => GLOBAL_VARIABLE,
        }
    }

    /// The key that signs writes of this variable.
    pub fn signer(self) -> Var {
        match self {
            Var::Db => Var::Kek,
            Var::Pk | Var::Kek => Var::Pk,
        }
    }
}

/// `n` as a little-endian `u32`; every size here is far below 4 GiB.
fn le32(n: usize) -> [u8; 4] {
    u32::try_from(n).unwrap_or(u32::MAX).to_le_bytes()
}

/// A signature list holding one X.509 certificate (DER).
pub fn esl(owner: Guid, cert: &[u8]) -> Vec<u8> {
    [
        &CERT_X509.bytes()[..],
        &le32(LIST_HEADER + 16 + cert.len()),
        &le32(0),
        &le32(16 + cert.len()),
        &owner.bytes(),
        cert,
    ]
    .concat()
}

fn word(b: &[u8], off: usize) -> usize {
    b.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map_or(0, |c| u32::from_le_bytes(*c) as usize)
}

/// The signature lists of `data` (what the firmware returns for KEK or db)
/// whose first entry is not owned by [`OWNER`], concatenated: the keys of
/// others, without the ones an earlier JuanitOS enrollment added. An error
/// unless `data` is a well-formed sequence of lists.
pub fn foreign_lists(data: &[u8]) -> Result<Vec<u8>> {
    let mut rest = data;
    let mut out = Vec::new();
    while !rest.is_empty() {
        let (size, header, entry) = (word(rest, 16), word(rest, 20), word(rest, 24));
        let entries = size
            .checked_sub(LIST_HEADER + header)
            .filter(|body| entry >= 16 && body % entry == 0);
        let (Some(_), Some(list), Some(next)) = (entries, rest.get(..size), rest.get(size..))
        else {
            return Err(Error::Other(
                "not a well-formed EFI signature list".to_string(),
            ));
        };
        let owner = LIST_HEADER + header;
        if list.get(owner..owner + 16) != Some(&OWNER.bytes()[..]) {
            out.extend_from_slice(list);
        }
        rest = next;
    }
    Ok(out)
}

/// EFI_TIME as authenticated writes carry it: UTC, to the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EfiTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl EfiTime {
    /// The date of `secs` seconds after 1970-01-01 (H. Hinnant's
    /// `civil_from_days`).
    pub fn from_unix(secs: u64) -> Self {
        let (days, rem) = (secs / 86_400, secs % 86_400);
        let z = days + 719_468;
        let era = z / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let (month, year) = if mp < 10 {
            (mp + 3, yoe + era * 400)
        } else {
            (mp - 9, yoe + era * 400 + 1)
        };
        EfiTime {
            year: year as u16,
            month: month as u8,
            day: day as u8,
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: (rem % 60) as u8,
        }
    }

    pub fn now() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        Self::from_unix(secs)
    }

    /// The 16-byte EFI_TIME; nanoseconds, time zone and daylight are 0, as
    /// authenticated writes require.
    pub fn bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        let [y0, y1] = self.year.to_le_bytes();
        let head = [
            y0,
            y1,
            self.month,
            self.day,
            self.hour,
            self.minute,
            self.second,
        ];
        for (slot, b) in out.iter_mut().zip(head) {
            *slot = b;
        }
        out
    }
}

/// UCS-2 little-endian, without a terminator.
pub fn ucs2(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// What the signature of an authenticated write of `var` covers: its name,
/// GUID, attributes, the time stamp and the new contents.
pub fn auth_payload(var: Var, time: &EfiTime, data: &[u8]) -> Vec<u8> {
    [
        &ucs2(var.name())[..],
        &var.guid().bytes(),
        &AUTH_ATTRIBUTES.to_le_bytes(),
        &time.bytes(),
        data,
    ]
    .concat()
}

/// An authenticated write (EFI_VARIABLE_AUTHENTICATION_2, then the data):
/// `signed_data` is a bare PKCS#7 SignedData over [`auth_payload`].
pub fn auth_write(time: &EfiTime, signed_data: &[u8], data: &[u8]) -> Vec<u8> {
    [
        &time.bytes()[..],
        &le32(24 + signed_data.len()),
        &WIN_CERT_REVISION.to_le_bytes(),
        &WIN_CERT_TYPE_EFI_GUID.to_le_bytes(),
        &CERT_TYPE_PKCS7.bytes(),
        signed_data,
        data,
    ]
    .concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guids_print_and_encode_mixed_endian() {
        assert_eq!(
            GLOBAL_VARIABLE.to_string(),
            "8be4df61-93ca-11d2-aa0d-00e098032b8c"
        );
        assert_eq!(
            GLOBAL_VARIABLE.bytes(),
            [
                0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03,
                0x2b, 0x8c
            ]
        );
        assert_eq!(
            IMAGE_SECURITY_DATABASE.to_string(),
            "d719b2cb-3d3a-4596-a3bc-dad00e67656f"
        );
        assert_eq!(OWNER.to_string(), "9d4c1a7e-5b2f-4e83-a6d1-3c7f0e92b845");
    }

    #[test]
    fn the_key_hierarchy() {
        let names: Vec<_> = Var::ALL.iter().map(|v| v.name()).collect();
        assert_eq!(names, ["PK", "KEK", "db"]);
        let signers: Vec<_> = Var::ALL.iter().map(|v| v.signer()).collect();
        assert_eq!(signers, [Var::Pk, Var::Pk, Var::Kek]);
        assert_eq!(Var::Kek.guid(), GLOBAL_VARIABLE);
        assert_eq!(Var::Db.guid(), IMAGE_SECURITY_DATABASE);
    }

    #[test]
    fn signature_lists() {
        let list = esl(OWNER, b"alice");
        assert_eq!(list.len(), 28 + 16 + 5);
        assert_eq!(&list[..16], &CERT_X509.bytes());
        assert_eq!(&list[16..28], &[49, 0, 0, 0, 0, 0, 0, 0, 21, 0, 0, 0]);
        assert_eq!(&list[28..44], &OWNER.bytes());
        assert_eq!(&list[44..], b"alice");

        let vendor = esl(GLOBAL_VARIABLE, b"bob");
        let three = [vendor.clone(), list.clone(), vendor.clone()].concat();
        assert_eq!(
            foreign_lists(&three).unwrap(),
            [&vendor[..], &vendor].concat()
        );
        assert_eq!(foreign_lists(&[]).unwrap(), b"");
        let bad = |data: &[u8]| foreign_lists(data).unwrap_err().to_string();
        assert_eq!(bad(b"chuck"), "not a well-formed EFI signature list");
        assert!(bad(&list[..40]).contains("well-formed"));
        let mut odd = list.clone();
        odd[24] = 20;
        assert!(bad(&odd).contains("well-formed"));
        odd[24] = 0;
        assert!(bad(&odd).contains("well-formed"));
        odd[16] = 27;
        assert!(bad(&odd).contains("well-formed"));
    }

    #[test]
    fn times_are_utc_calendar_dates() {
        let at = |secs| {
            let t = EfiTime::from_unix(secs);
            (t.year, t.month, t.day, t.hour, t.minute, t.second)
        };
        assert_eq!(at(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(at(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(at(1_794_573_296), (2026, 11, 13, 12, 34, 56));
        assert_eq!(at(1_798_761_599), (2026, 12, 31, 23, 59, 59));
        assert!(EfiTime::now().year >= 2026);
        let t = EfiTime::from_unix(1_794_573_296);
        assert_eq!(
            t.bytes(),
            [0xea, 0x07, 11, 13, 12, 34, 56, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn authenticated_writes() {
        let t = EfiTime::from_unix(0);
        let payload = auth_payload(Var::Kek, &t, b"bob");
        assert_eq!(
            payload,
            [
                &[b'K', 0, b'E', 0, b'K', 0][..],
                &GLOBAL_VARIABLE.bytes(),
                &[0x27, 0, 0, 0],
                &t.bytes(),
                b"bob"
            ]
            .concat()
        );
        let write = auth_write(&t, b"alice", b"bob");
        assert_eq!(&write[..16], &t.bytes());
        assert_eq!(&write[16..24], &[29, 0, 0, 0, 0x00, 0x02, 0xf1, 0x0e]);
        assert_eq!(&write[24..40], &CERT_TYPE_PKCS7.bytes());
        assert_eq!(&write[40..], b"alicebob");
    }
}
