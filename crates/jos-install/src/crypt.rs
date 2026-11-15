// SPDX-License-Identifier: BSD-3-Clause
//! Disk encryption: the key of the pool's native ZFS encryption.
//!
//! The pool partition is a GELI volume (AES-256-XTS), which the boot loader
//! unlocks with the passphrase typed at power-on: FreeBSD's loader cannot
//! read a natively encrypted root dataset, so GELI is what protects the
//! kernel, `/etc` and `/boot`. Everything else (`/home`, `/var`, `/tmp` and
//! all the qubes) is encrypted again by ZFS (AES-256-GCM) under a key that
//! is *derived from the same passphrase*, so there is nothing else to
//! remember or type.
//!
//! The derivation is deterministic and meant to be audited; it is
//! PBKDF2 (RFC 8018) over HMAC-SHA-512 (RFC 2104), written out here on top
//! of the `sha2` crate's SHA-512 alone:
//!
//! ```text
//! zfs_key (32 bytes, 64 hex digits for `keyformat=hex`)
//!   = PBKDF2-HMAC-SHA512(
//!       password   = the passphrase, as typed (printable ASCII),
//!       salt       = "JuanitOS zfs key v1" || 0x00 || geli_salt,
//!       iterations = geli_iterations,
//!       dkLen      = 32)
//! ```
//!
//! `geli_salt` (64 bytes) and `geli_iterations` are the `Salt` and
//! `iterations` fields of `geli dump` for the pool partition: a per-volume
//! random salt, and the cost `geli init` calibrated to the machine. They
//! are public (they sit in the GELI header), and the label makes the key
//! independent of GELI's own user key, which is
//! `HMAC-SHA512("", PBKDF2-HMAC-SHA512(passphrase, geli_salt, iterations))`
//! for the same passphrase. `docs/security.md` has a test vector and the
//! same computation in a few lines of Python.

use juanitos_core::{Error, Result};
use sha2::{Digest, Sha512};

/// Domain separation of the ZFS key from every other use of the
/// passphrase and the GELI salt. Changing the derivation means a new
/// version number here.
pub const KEY_LABEL: &[u8] = b"JuanitOS zfs key v1";

/// The ZFS key's length: AES-256.
const KEY_LEN: usize = 32;

/// SHA-512's block and digest lengths.
const BLOCK: usize = 128;
const DIGEST: usize = 64;

/// The salt GELI keeps in its header (`G_ELI_SALTLEN`).
const GELI_SALT_LEN: usize = 64;

/// Shortest and longest passphrase. GELI reads a passphrase up to a
/// newline, in a 1024-byte buffer, and its kernel cache holds 255 bytes.
pub const MIN_PASSPHRASE: usize = 8;
pub const MAX_PASSPHRASE: usize = 128;

/// HMAC-SHA-512 with the pads absorbed once: PBKDF2 calls it for every
/// iteration of its loop.
struct HmacSha512 {
    inner: Sha512,
    outer: Sha512,
}

/// Copy as much of `src` as fits into `dst`.
fn copy_into(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s;
    }
}

impl HmacSha512 {
    fn new(key: &[u8]) -> Self {
        let mut block = [0u8; BLOCK];
        if key.len() > BLOCK {
            copy_into(&mut block, &Sha512::digest(key));
        } else {
            copy_into(&mut block, key);
        }
        HmacSha512 {
            inner: Sha512::new().chain_update(block.map(|b| b ^ 0x36)),
            outer: Sha512::new().chain_update(block.map(|b| b ^ 0x5c)),
        }
    }

    /// The MAC of `parts`, concatenated.
    fn mac(&self, parts: &[&[u8]]) -> [u8; DIGEST] {
        let mut inner = self.inner.clone();
        for part in parts {
            inner.update(part);
        }
        let mut mac = [0u8; DIGEST];
        copy_into(
            &mut mac,
            &self.outer.clone().chain_update(inner.finalize()).finalize(),
        );
        mac
    }
}

/// PBKDF2 with HMAC-SHA-512 as the pseudorandom function; fills `out`.
pub fn pbkdf2_hmac_sha512(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let prf = HmacSha512::new(password);
    for (i, chunk) in out.chunks_mut(DIGEST).enumerate() {
        let counter = (i as u32 + 1).to_be_bytes();
        let mut u = prf.mac(&[salt, &counter]);
        let mut t = u;
        for _ in 1..iterations {
            u = prf.mac(&[&u]);
            for (acc, byte) in t.iter_mut().zip(&u) {
                *acc ^= byte;
            }
        }
        copy_into(chunk, &t);
    }
}

/// What the key derivation takes from `geli dump`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeliMeta {
    pub iterations: u32,
    /// The 64 bytes of the header's salt.
    pub salt: Vec<u8>,
}

impl GeliMeta {
    /// Stand-in for a dry run, where `geli dump` has nothing to print: a
    /// cheap key that nothing is encrypted with.
    pub fn dry_run() -> Self {
        GeliMeta {
            iterations: 1,
            salt: vec![0; GELI_SALT_LEN],
        }
    }
}

fn nibble(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let [high, low] = pair else { return None };
            Some((nibble(*high)? << 4) | nibble(*low)?)
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `iterations` and `Salt` lines of `geli dump`:
///
/// ```text
/// iterations: 1294201
///       Salt: 9f3a...(128 hex digits)
/// ```
pub fn parse_geli_dump(dump: &str) -> Result<GeliMeta> {
    let field = |name: &str| {
        dump.lines()
            .find_map(|line| line.trim_start().strip_prefix(name))
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
    };
    let iterations = field("iterations")
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n > 0);
    let salt = field("Salt")
        .and_then(decode_hex)
        .filter(|salt| salt.len() == GELI_SALT_LEN);
    match (iterations, salt) {
        (Some(iterations), Some(salt)) => Ok(GeliMeta { iterations, salt }),
        _ => Err(Error::Other(
            "cannot read the iterations and the salt of the GELI volume from `geli dump`".into(),
        )),
    }
}

/// The ZFS key for `passphrase` on the GELI volume `meta` describes, as
/// the 64 hex digits of `keyformat=hex`. See the module documentation.
pub fn derive_zfs_key(passphrase: &str, meta: &GeliMeta) -> String {
    let salt = [KEY_LABEL, &[0], &meta.salt].concat();
    let mut key = [0u8; KEY_LEN];
    pbkdf2_hmac_sha512(passphrase.as_bytes(), &salt, meta.iterations, &mut key);
    encode_hex(&key)
}

/// A passphrase the boot loader can take: the loader reads the firmware's
/// keyboard, which types with a US layout whatever the installer's keymap
/// is, so only printable ASCII is allowed.
pub fn check_passphrase(passphrase: &str) -> Result<()> {
    let len = passphrase.len();
    if len < MIN_PASSPHRASE {
        return Err(Error::Other(format!(
            "the encryption passphrase needs {MIN_PASSPHRASE} characters at least"
        )));
    }
    if len > MAX_PASSPHRASE {
        return Err(Error::Other(format!(
            "the encryption passphrase takes {MAX_PASSPHRASE} characters at most"
        )));
    }
    if !passphrase.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return Err(Error::Other(
            "the encryption passphrase takes printable ASCII only: the boot loader types \
             with a US keyboard layout"
                .into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn hex_of(bytes: &[u8]) -> String {
        encode_hex(bytes)
    }

    #[test]
    fn hmac_follows_rfc_4231() {
        // Test case 2, and 6: a key longer than the block is hashed first.
        let mac = HmacSha512::new(b"Jefe").mac(&[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex_of(&mac),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
             9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
        let key = [0xaa; 131];
        let msg = b"Test Using Larger Than Block-Size Key - Hash Key First";
        assert_eq!(
            hex_of(&HmacSha512::new(&key).mac(&[msg])),
            "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f352\
             6b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598"
        );
    }

    #[test]
    fn pbkdf2_matches_the_reference() {
        // Python's hashlib.pbkdf2_hmac('sha512', b'password', b'salt', c, n)
        let mut out = [0u8; 64];
        pbkdf2_hmac_sha512(b"password", b"salt", 1, &mut out);
        assert_eq!(
            hex_of(&out),
            "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252\
             c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce"
        );
        pbkdf2_hmac_sha512(b"password", b"salt", 2, &mut out);
        assert_eq!(
            hex_of(&out),
            "e1d9c16aa681708a45f5c7c4e215ceb66e011a2e9f0040713f18aefdb866d53c\
             f76cab2868a39b9f7840edce4fef5a82be67335c77a6068e04112754f27ccf4e"
        );
        // More than one block of output: the second block has counter 2.
        let mut long = [0u8; 80];
        pbkdf2_hmac_sha512(b"password", b"salt", 1, &mut long);
        assert_eq!(
            hex_of(&long),
            "867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252\
             c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce\
             7b532e206c2967d4c7d2ffa460539fc4"
        );
    }

    pub(crate) const DUMP: &str = "     magic: GEOM::ELI\n   version: 7\n     flags: 0x00\n     \
                        ealgo: AES-XTS\n    keylen: 256\n  provsize: 107374182400\n\
                        sectorsize: 4096\n      keys: 0x01\niterations: 1000\n      \
                        Salt: 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\
                        202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f\n\
                        Master Key: 00\n";

    pub(crate) fn meta() -> GeliMeta {
        GeliMeta {
            iterations: 1000,
            salt: (0..64).collect(),
        }
    }

    #[test]
    fn the_geli_dump_is_read() {
        assert_eq!(parse_geli_dump(DUMP).unwrap(), meta());
        // What a dry run prints, or a damaged header, is refused.
        let bad = [
            "",
            "iterations: 1000\n",
            "iterations: -1\nSalt: 00\n",
            "iterations: 0\n      Salt: 00\n",
            "iterations: many\n",
            "iterations 1000\n",
            "iterations: 5\nSalt: 0g\n",
        ];
        for dump in bad {
            let err = parse_geli_dump(dump).unwrap_err().to_string();
            assert!(err.contains("geli dump"), "{dump:?}: {err}");
        }
        // A salt of the wrong length, or of half a byte.
        let short = DUMP.replace("3f\n", "\n");
        assert!(parse_geli_dump(&short).is_err());
        let odd = DUMP.replace("3f\n", "3\n");
        assert!(parse_geli_dump(&odd).is_err());
    }

    #[test]
    fn hex_is_decoded_strictly() {
        assert_eq!(decode_hex("00ff"), Some(vec![0x00, 0xff]));
        assert_eq!(decode_hex("0"), None, "odd tail");
        assert_eq!(decode_hex("g0"), None, "bad high nibble");
        assert_eq!(decode_hex("0g"), None, "bad low nibble");
        assert_eq!(decode_hex(""), Some(Vec::new()));
    }

    #[test]
    fn the_zfs_key_derivation_is_fixed() {
        // Python: hashlib.pbkdf2_hmac('sha512', b'correct horse',
        //   b'JuanitOS zfs key v1\0' + bytes(range(64)), 1000, 32).hex()
        let key = derive_zfs_key("correct horse", &meta());
        assert_eq!(
            key,
            "5405e2e0ecfc0a2a9c1552d69e5352fb3ca9ddbe113d1664832255286b0e8200"
        );
        // Python: the same with 128 'x' and 3 iterations (a password as long
        // as the block).
        let other = GeliMeta {
            iterations: 3,
            ..meta()
        };
        assert_eq!(
            derive_zfs_key(&"x".repeat(128), &other),
            "9311a97e036bb47dfa6666f7c5bff1d76e1ef402b0b222aee8aa6e3b50517b5f"
        );
        // Every input counts: the passphrase, the salt, the cost.
        let salt = GeliMeta {
            salt: vec![1; 64],
            ..meta()
        };
        for changed in [
            derive_zfs_key("correct horse!", &meta()),
            derive_zfs_key("correct horse", &salt),
            derive_zfs_key("correct horse", &other),
        ] {
            assert_ne!(changed, key);
            assert_eq!(changed.len(), 64);
        }
    }

    #[test]
    fn the_dry_run_stand_in_is_cheap_and_well_formed() {
        let meta = GeliMeta::dry_run();
        assert_eq!(meta.iterations, 1);
        assert_eq!(meta.salt.len(), 64);
        assert_eq!(derive_zfs_key("correct horse", &meta).len(), 64);
    }

    #[test]
    fn passphrases_are_checked() {
        assert!(check_passphrase("correct horse battery staple + 1!").is_ok());
        assert!(check_passphrase("12345678").is_ok());
        let err = |pw: &str| check_passphrase(pw).unwrap_err().to_string();
        assert!(err("short").contains("8 characters at least"));
        assert!(err(&"x".repeat(129)).contains("128 characters at most"));
        assert!(check_passphrase(&"x".repeat(128)).is_ok());
        // Accents, a tab: nothing the loader's US layout can type.
        assert!(err("caf\u{e9} au lait").contains("printable ASCII"));
        assert!(err("tab\tseparated").contains("printable ASCII"));
    }
}
