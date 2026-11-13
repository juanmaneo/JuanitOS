// SPDX-License-Identifier: BSD-3-Clause
//! Secure Boot with the owner's own keys.
//!
//! [`SecureBoot::keygen`] makes a Platform Key, a Key Exchange Key and a
//! signature database key (`openssl`), and the signed `.auth` files that
//! put them into the firmware. [`SecureBoot::sign_esp`] signs the boot
//! files JuanitOS keeps on the EFI system partition with the database
//! key, and [`SecureBoot::enroll`] writes the `.auth` files to the
//! firmware while it is in setup mode. Only what the firmware needs is
//! built here (DER, PE/COFF digests, EFI signature lists): the signing
//! itself is `openssl cms`.

pub mod der;
pub mod efi;
pub mod pe;
pub mod pkcs7;

use std::fmt;
use std::path::{Path, PathBuf};

use crate::dom0;
use crate::error::{Error, Result};
use crate::fsops::FsOps;
use crate::sys::{shell_quote, Runner};
use efi::{EfiTime, Var};
use pe::Signature;

/// Where the keys live, below the configuration directory.
pub const KEYS_DIR: &str = "secureboot";
/// Where the installed system mounts its EFI system partition.
pub const ESP_MOUNT: &str = "/boot/efi";
/// Where the public halves go on the ESP: the firmware's own setup menu
/// can enroll them from there.
pub const ESP_KEYS: &str = "EFI/JUANITOS/keys";
/// The boot files JuanitOS puts on an ESP (its own, or one shared with
/// other systems), under the ESP's root.
pub const ESP_IMAGES: [&str; 3] = [dom0::ESP_DEFAULT, dom0::ESP_SHARED, dom0::ESP_LOADER];
/// Where those files come from: what an ESP file must match to be signed.
const SOURCES: [&str; 2] = [dom0::JOS_BOOT_EFI, dom0::LOADER_EFI];
/// What each key has: its private key, certificate (PEM and DER), the
/// signature list and the signed write.
const KEY_FILES: [&str; 5] = ["key", "crt", "cer", "esl", "auth"];

fn arg(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The contents of `path`; an error names it.
fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| Error::Other(format!("reading {}: {e}", path.display())))
}

#[cfg(test)]
mod tests;

/// Where Secure Boot stands on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// `SecureBoot`: firmware checks what it loads. `None`: unreadable.
    pub secure_boot: Option<bool>,
    /// `SetupMode`: no Platform Key, keys can be enrolled.
    pub setup_mode: Option<bool>,
    /// Whether the keys were generated.
    pub keys: bool,
    /// The boot files found on the ESP and who signed them.
    pub images: Vec<(String, Signature)>,
}

fn state(flag: Option<bool>, yes: &'static str, no: &'static str) -> &'static str {
    match flag {
        Some(true) => yes,
        Some(false) => no,
        None => "unknown (not booted through EFI?)",
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = format!(
            "Secure Boot: {}\nSetup mode:  {}\nKeys:        {}\n",
            state(self.secure_boot, "enabled", "disabled"),
            state(self.setup_mode, "yes", "no"),
            if self.keys {
                "generated"
            } else {
                "none (jos secureboot keygen)"
            },
        );
        for (name, signature) in &self.images {
            let who = match signature {
                Signature::Ours => "signed with the JuanitOS key",
                Signature::Other => "signed by someone else",
                Signature::None => "not signed",
            };
            out.push_str(&format!("{name}: {who}\n"));
        }
        f.write_str(&out)
    }
}

/// The Secure Boot keys of one machine and the tools that use them.
pub struct SecureBoot<'a> {
    pub runner: &'a dyn Runner,
    pub fs: FsOps,
    /// Where the keys are (or go).
    pub keys: PathBuf,
}

impl<'a> SecureBoot<'a> {
    pub fn new(runner: &'a dyn Runner, dry_run: bool, keys: PathBuf) -> Self {
        SecureBoot {
            runner,
            fs: FsOps::new(dry_run),
            keys,
        }
    }

    fn file(&self, name: &str, ext: &str) -> PathBuf {
        self.keys.join(format!("{name}.{ext}"))
    }

    fn key(&self, var: Var, ext: &str) -> PathBuf {
        self.file(var.name(), ext)
    }

    fn keys_present(&self) -> bool {
        Var::ALL
            .iter()
            .flat_map(|&var| KEY_FILES.map(|ext| self.key(var, ext)))
            .all(|path| path.is_file())
    }

    fn require_keys(&self) -> Result<()> {
        if self.keys_present() {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "no complete set of Secure Boot keys in {}: run `jos secureboot keygen` first",
                self.keys.display()
            )))
        }
    }

    /// Make the three keys, their signature lists and the signed writes.
    /// With `keep_vendor` the firmware's own KEK and db entries stay
    /// enrolled next to ours: the option ROMs of some graphics and network
    /// cards are signed by them, and refuse to run without. The firmware's
    /// lists are saved as `vendor-NAME.esl`, whatever happens next.
    pub fn keygen(&self, keep_vendor: bool, force: bool) -> Result<()> {
        if !force && self.key(Var::Pk, "key").exists() {
            return Err(Error::Other(format!(
                "{} already holds Secure Boot keys: --force replaces them, and what they \
                 enrolled in the firmware stops working",
                self.keys.display()
            )));
        }
        self.fs
            .mkdir_p(&self.keys)
            .and_then(|()| self.fs.chmod(&self.keys, 0o700))?;
        let foreign = Var::ALL
            .iter()
            .map(|&var| {
                if keep_vendor && var != Var::Pk {
                    self.vendor_list(var)
                } else {
                    Ok(Vec::new())
                }
            })
            .collect::<Result<Vec<_>>>()?;
        for var in Var::ALL {
            self.generate(var)?;
        }
        if self.fs.dry_run {
            return Ok(());
        }
        let time = EfiTime::now();
        for (var, foreign) in Var::ALL.into_iter().zip(&foreign) {
            self.assemble(var, foreign, &time)?;
        }
        Ok(())
    }

    /// The firmware's lists of `var` that are not ours, saved next to the
    /// keys. Nothing when it cannot be read: the keys then replace it.
    fn vendor_list(&self, var: Var) -> Result<Vec<u8>> {
        let saved = self.file(&format!("vendor-{}", var.name()), "esl");
        let command = format!(
            "efivar -p -b -N -n {}-{} > {}",
            var.guid(),
            var.name(),
            shell_quote(&arg(&saved))
        );
        let read = self
            .runner
            .run("sh", &["-c", &command])
            .is_ok_and(|out| out.success());
        if !read {
            println!(
                "note: the firmware's {} cannot be read (not booted through EFI?): \
                 the new one replaces it",
                var.name()
            );
        }
        if !read || self.fs.dry_run {
            return Ok(Vec::new());
        }
        efi::foreign_lists(&std::fs::read(&saved).unwrap_or_default())
    }

    /// A self-signed certificate and its private key, then the DER form
    /// the firmware takes.
    fn generate(&self, var: Var) -> Result<()> {
        let (key, crt, cer) = (
            arg(&self.key(var, "key")),
            arg(&self.key(var, "crt")),
            arg(&self.key(var, "cer")),
        );
        let subject = format!("/CN=JuanitOS {}/", var.name());
        let request = [
            "req", "-new", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-days", "3650",
            "-subj", &subject, "-keyout", &key, "-out", &crt,
        ];
        self.runner
            .run_checked("openssl", &request)
            .and_then(|_| {
                let der = ["x509", "-in", &crt, "-outform", "DER", "-out", &cer];
                self.runner.run_checked("openssl", &der)
            })
            .map(drop)
    }

    /// `var`'s signature list (our certificate, then `foreign`) and the
    /// signed write of it.
    fn assemble(&self, var: Var, foreign: &[u8], time: &EfiTime) -> Result<()> {
        let list = read(&self.key(var, "cer"))
            .map(|cer| [efi::esl(efi::OWNER, &cer), foreign.to_vec()].concat())?;
        let payload = efi::auth_payload(var, time, &list);
        let auth = self
            .cms_sign(var.signer(), &payload, &["-noattr", "-nosmimecap"])
            .and_then(|cms| pkcs7::bare_signed_data(&cms))
            .map(|signed| efi::auth_write(time, &signed, &list))?;
        self.fs
            .write_bytes(&self.key(var, "esl"), &list)
            .and_then(|()| self.fs.write_bytes(&self.key(var, "auth"), &auth))
    }

    /// `openssl cms -sign` of `content` with the key of `signer` (DER).
    fn cms_sign(&self, signer: Var, content: &[u8], extra: &[&str]) -> Result<Vec<u8>> {
        let scratch = self.keys.join(format!(".sign-{}", std::process::id()));
        let signed = self.sign_in(&scratch, signer, content, extra);
        let _ = std::fs::remove_dir_all(&scratch);
        signed
    }

    fn sign_in(
        &self,
        scratch: &Path,
        signer: Var,
        content: &[u8],
        extra: &[&str],
    ) -> Result<Vec<u8>> {
        let (input, output) = (scratch.join("content"), scratch.join("signature"));
        std::fs::create_dir_all(scratch).and_then(|()| std::fs::write(&input, content))?;
        let (crt, key) = (arg(&self.key(signer, "crt")), arg(&self.key(signer, "key")));
        let (input, signature) = (arg(&input), arg(&output));
        let mut args = vec![
            "cms", "-sign", "-binary", "-md", "sha256", "-outform", "DER", "-signer", &crt,
            "-inkey", &key, "-in", &input, "-out", &signature,
        ];
        args.extend_from_slice(extra);
        self.runner.run_checked("openssl", &args)?;
        read(&output)
    }

    /// `image` with an Authenticode signature by the db key (replacing
    /// any other).
    pub fn sign_image(&self, image: &[u8]) -> Result<Vec<u8>> {
        let prepared = pe::prepare(image)?;
        let content = pkcs7::indirect_data_content(&prepared.digest);
        let extra = [
            "-nodetach",
            "-econtent_type",
            pkcs7::SPC_INDIRECT_DATA,
            "-nosmimecap",
        ];
        self.cms_sign(Var::Db, &content, &extra)
            .and_then(|cms| pkcs7::authenticode(&cms))
            .map(|signature| prepared.embed(&signature))
    }

    /// Sign the file at `path` in place: written beside it first, so that
    /// a full disk leaves the old file.
    pub fn sign_file(&self, path: &Path) -> Result<()> {
        self.require_keys()?;
        if self.fs.dry_run {
            println!("+ sign {}", path.display());
            return Ok(());
        }
        let signed = read(path).and_then(|image| self.sign_image(&image))?;
        let temp = path.with_extension("signing");
        let written = std::fs::write(&temp, signed).and_then(|()| std::fs::rename(&temp, path));
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        written
            .map(|()| println!("signed {}", path.display()))
            .map_err(Into::into)
    }

    /// Sign the JuanitOS boot files on the ESP mounted at `esp`: the ones
    /// that are what `root` (the installed system) has in `/boot`, signed
    /// or not. Files of other systems on a shared ESP are left alone. The
    /// public keys go to the ESP too. Returns how many files were signed.
    pub fn sign_esp(&self, esp: &Path, root: &Path) -> Result<usize> {
        self.require_keys()?;
        let known: Vec<[u8; 32]> = SOURCES
            .iter()
            .filter_map(|src| std::fs::read(root.join(src.trim_start_matches('/'))).ok())
            .filter_map(|image| pe::prepare(&image).ok())
            .map(|prepared| prepared.digest)
            .collect();
        if known.is_empty() {
            return Err(Error::Other(format!(
                "no boot files to compare with under {}/boot",
                root.display()
            )));
        }
        let mut signed = 0;
        for rel in ESP_IMAGES {
            let path = esp.join(rel);
            let Ok(image) = std::fs::read(&path) else {
                continue;
            };
            if pe::prepare(&image).is_ok_and(|prepared| known.contains(&prepared.digest)) {
                self.sign_file(&path)?;
                signed += 1;
            } else {
                println!("{rel}: not a JuanitOS boot file, left alone");
            }
        }
        if signed == 0 {
            return Err(Error::Other(format!(
                "no JuanitOS boot file found under {}: is the ESP mounted there?",
                esp.display()
            )));
        }
        self.publish(esp)?;
        Ok(signed)
    }

    /// The public halves of the keys, where the firmware can enroll them.
    fn publish(&self, esp: &Path) -> Result<()> {
        let target = esp.join(ESP_KEYS);
        Var::ALL
            .iter()
            .flat_map(|&var| ["auth", "cer", "esl"].map(|ext| format!("{}.{ext}", var.name())))
            .try_for_each(|name| self.fs.copy(&self.keys.join(&name), &target.join(&name)))
    }

    /// A one-byte EFI variable of the global namespace as a flag.
    fn flag(&self, name: &str) -> Option<bool> {
        let id = format!("{}-{name}", efi::GLOBAL_VARIABLE);
        self.runner
            .run("efivar", &["-p", "-b", "-N", "-n", &id])
            .ok()
            .filter(|out| out.success())
            .map(|out| out.stdout.starts_with('\u{1}'))
    }

    /// What the firmware, the keys and the boot files on `esp` say.
    pub fn status(&self, esp: &Path) -> Status {
        let cert = std::fs::read(self.key(Var::Db, "cer")).unwrap_or_default();
        let images = ESP_IMAGES
            .iter()
            .filter_map(|rel| {
                let image = std::fs::read(esp.join(rel)).ok()?;
                Some((rel.to_string(), pe::signature(&image, &cert)))
            })
            .collect();
        Status {
            secure_boot: self.flag("SecureBoot"),
            setup_mode: self.flag("SetupMode"),
            keys: self.keys_present(),
            images,
        }
    }

    /// Write the signed lists to the firmware: db and KEK first, the
    /// Platform Key last, which leaves setup mode. Only a firmware in
    /// setup mode takes them this way.
    pub fn enroll(&self) -> Result<()> {
        self.require_keys()?;
        if self.flag("SetupMode") != Some(true) {
            return Err(Error::Other(format!(
                "the firmware is not in setup mode: clear its Platform Key in the firmware \
                 setup first, or enroll the .auth files from {ESP_KEYS} on the ESP with the \
                 firmware's own menu"
            )));
        }
        let attributes = format!("{:#x}", efi::AUTH_ATTRIBUTES);
        for var in [Var::Db, Var::Kek, Var::Pk] {
            let id = format!("{}-{}", var.guid(), var.name());
            let auth = arg(&self.key(var, "auth"));
            let write = ["-w", "-t", &attributes, "-n", &id, "-f", &auth];
            self.runner.run_checked("efivar", &write)?;
        }
        Ok(())
    }
}
