// SPDX-License-Identifier: BSD-3-Clause
//! The signing pipeline end to end, with the real `openssl` and a fake
//! `efivar`. What the firmware does with the results is checked by
//! `jos-release secureboot-test`, with a real firmware in QEMU.

use std::fs;
use std::sync::Mutex;

use super::*;
use crate::sys::{shell_join, CmdOutput, DryRunner, RealRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// The command exits with status 1.
    Fail,
    /// The command "succeeds" and does nothing.
    Skip,
    /// The command runs, then leaves garbage in its `-out` file.
    Garbage,
}

/// Runs `openssl` for real and `efivar` through a script that keeps its
/// variables in `dir/efi`; any one command can be made to misbehave.
struct Host {
    dir: PathBuf,
    fault: Option<(usize, Fault)>,
    calls: Mutex<Vec<String>>,
    /// Whether `dir` goes when this host does.
    owner: bool,
}

impl Host {
    fn new(name: &str) -> Host {
        let dir = std::env::temp_dir().join(format!("jos-sb-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("efi")).unwrap();
        // `efivar -p -b -N -n GUID-NAME` prints the file NAME; `efivar -w
        // -t ATTRIBUTES -n GUID-NAME -f FILE` copies FILE to written-NAME.
        let script = format!(
            "#!/bin/sh\nd='{}/efi'\nv=${{5##*-}}\ncase \"$1\" in\n\
             -p) [ -f \"$d/$v\" ] && cat \"$d/$v\" ;;\n\
             -w) cp \"$7\" \"$d/written-$v\" ;;\nesac\n",
            dir.display()
        );
        FsOps::new(false)
            .write_executable(&dir.join("efivar"), &script)
            .unwrap();
        Host {
            dir,
            fault: None,
            calls: Mutex::new(Vec::new()),
            owner: true,
        }
    }

    /// Another host in the same place, with a command that misbehaves.
    fn faulty(&self, call: usize, fault: Fault) -> Host {
        Host {
            dir: self.dir.clone(),
            fault: Some((call, fault)),
            calls: Mutex::new(Vec::new()),
            owner: false,
        }
    }

    /// A host with the keys generated.
    fn keyed(name: &str) -> Host {
        let host = Host::new(name);
        host.sb().keygen(false, false).unwrap();
        host
    }

    fn sb(&self) -> SecureBoot<'_> {
        SecureBoot::new(self, false, self.keys())
    }

    fn keys(&self) -> PathBuf {
        self.dir.join("keys")
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Set what the fake firmware holds in `name`.
    fn set(&self, name: &str, value: &[u8]) {
        fs::write(self.dir.join("efi").join(name), value).unwrap();
    }

    fn read(&self, path: &str) -> Vec<u8> {
        fs::read(self.dir.join(path)).unwrap()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.owner {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

impl Runner for Host {
    fn run(&self, program: &str, args: &[&str]) -> Result<CmdOutput> {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(shell_join(program, args));
            calls.len() - 1
        };
        let fault = self.fault.filter(|&(at, _)| at == n).map(|(_, f)| f);
        match fault {
            Some(Fault::Fail) => return Ok(CmdOutput::failed(1, "mock failure")),
            Some(Fault::Skip) => return Ok(CmdOutput::ok("")),
            Some(Fault::Garbage) | None => (),
        }
        let script = arg(&self.dir.join("efivar"));
        let program = if program == "efivar" {
            &script
        } else {
            program
        };
        let args: Vec<String> = args
            .iter()
            .map(|a| a.replacen("efivar ", &format!("{script} "), 1))
            .collect();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = RealRunner.run(program, &args)?;
        if let (Some(Fault::Garbage), Some(at)) = (fault, args.iter().position(|a| *a == "-out")) {
            fs::write(args[at + 1], b"chuck")?;
        }
        Ok(out)
    }

    fn run_with_input(&self, program: &str, args: &[&str], _input: &str) -> Result<CmdOutput> {
        self.run(program, args)
    }
}

/// An image with another body than [`pe::tests::image`]'s.
fn other_image() -> Vec<u8> {
    let mut b = pe::tests::image(false);
    b[0x300] ^= 0xff;
    b
}

/// Does `signature` (a DER SignedData) over `content` hold, with `ca` as
/// the only trust anchor?
fn verifies(host: &Host, signature: &[u8], content: &[u8], ca: &str) -> bool {
    fs::write(host.dir.join("signature"), signature).unwrap();
    fs::write(host.dir.join("content"), content).unwrap();
    let dir = |name: &str| arg(&host.dir.join(name));
    let ca = arg(&host.keys().join(ca));
    RealRunner
        .run(
            "openssl",
            &[
                "smime",
                "-verify",
                "-inform",
                "DER",
                "-binary",
                "-partial_chain",
                "-purpose",
                "any",
                "-out",
                "/dev/null",
                "-in",
                &dir("signature"),
                "-content",
                &dir("content"),
                "-CAfile",
                &ca,
            ],
        )
        .unwrap()
        .success()
}

/// The Authenticode signature in `image`: the PKCS#7 in its certificate
/// table, and what firmware checks it against.
fn embedded(image: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let cert = pe::layout(image).unwrap().cert;
    let table = &image[cert.0 + 8..cert.0 + cert.1];
    let signature = der::read(table).unwrap().0.raw.to_vec();
    let digest = pe::prepare(image).unwrap().digest;
    (signature, pkcs7::indirect_data_content(&digest))
}

fn efi_time(bytes: &[u8]) -> EfiTime {
    EfiTime {
        year: u16::from_le_bytes([bytes[0], bytes[1]]),
        month: bytes[2],
        day: bytes[3],
        hour: bytes[4],
        minute: bytes[5],
        second: bytes[6],
    }
}

#[test]
fn keys_are_made_and_the_writes_are_signed() {
    let host = Host::keyed("keys");
    let keys = host.keys();
    for var in Var::ALL {
        for ext in KEY_FILES {
            assert!(
                keys.join(format!("{}.{ext}", var.name())).is_file(),
                "{} {ext}",
                var.name()
            );
        }
        let cer = fs::read(keys.join(format!("{}.cer", var.name()))).unwrap();
        let list = fs::read(keys.join(format!("{}.esl", var.name()))).unwrap();
        assert_eq!(list, efi::esl(efi::OWNER, &cer));
        assert!(cer.starts_with(&[0x30, 0x82]));

        // The signed write: header, bare SignedData, then the list.
        let auth = fs::read(keys.join(format!("{}.auth", var.name()))).unwrap();
        assert!(auth.ends_with(&list));
        let signed = &auth[40..auth.len() - list.len()];
        assert_eq!(&auth[16..20], &(24 + signed.len() as u32).to_le_bytes());
        // What edk2 checks before it looks at the signature.
        assert_eq!(signed[1], 0x82);
        assert_eq!(&signed[13..22], &der::OID_SHA256[2..]);
        let payload = efi::auth_payload(var, &efi_time(&auth), &list);
        let wrapped = der::sequence(&[der::OID_SIGNED_DATA, &der::encode(der::CONTEXT_0, signed)]);
        let signer = format!("{}.crt", var.signer().name());
        assert!(verifies(&host, &wrapped, &payload, &signer), "{signer}");
        let others = Var::ALL.map(|v| format!("{}.crt", v.name()));
        for other in others.iter().filter(|o| **o != signer) {
            assert!(!verifies(&host, &wrapped, &payload, other), "{other}");
        }
        let mut tampered = payload.clone();
        tampered[0] ^= 1;
        assert!(!verifies(&host, &wrapped, &tampered, &signer));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&keys).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    assert!(!fs::read_dir(&keys).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".sign")));
    assert_eq!(host.calls().len(), 9, "3 x (req, x509, cms)");
}

#[test]
fn existing_keys_are_kept_unless_forced() {
    let host = Host::keyed("force");
    let keys = host.keys();
    let before = fs::read(keys.join("PK.crt")).unwrap();
    let err = host.sb().keygen(false, false).unwrap_err().to_string();
    assert!(err.contains("already holds Secure Boot keys"), "{err}");
    assert_eq!(fs::read(keys.join("PK.crt")).unwrap(), before);
    host.sb().keygen(false, true).unwrap();
    assert_ne!(fs::read(keys.join("PK.crt")).unwrap(), before);
}

#[test]
fn the_firmwares_keys_stay_enrolled_next_to_ours() {
    let host = Host::new("vendor");
    let vendor = efi::Guid(1, 2, 3, [4; 8]);
    let kek = efi::esl(vendor, b"alice");
    let db = [
        efi::esl(efi::OWNER, b"an earlier ours"),
        efi::esl(vendor, b"bob"),
    ]
    .concat();
    host.set("KEK", &kek);
    host.set("db", &db);
    host.sb().keygen(true, false).unwrap();
    let keys = host.keys();
    let ours = |name: &str| {
        let cer = fs::read(keys.join(format!("{name}.cer"))).unwrap();
        efi::esl(efi::OWNER, &cer)
    };
    let list = |name: &str| fs::read(keys.join(format!("{name}.esl"))).unwrap();
    assert_eq!(list("PK"), ours("PK"));
    assert_eq!(list("KEK"), [ours("KEK"), kek.clone()].concat());
    assert_eq!(list("db"), [ours("db"), efi::esl(vendor, b"bob")].concat());
    // The firmware's lists are kept as they were.
    assert_eq!(fs::read(keys.join("vendor-db.esl")).unwrap(), db);
    assert_eq!(fs::read(keys.join("vendor-KEK.esl")).unwrap(), kek);
    let calls = host.calls();
    assert!(calls[0]
        .starts_with("sh -c 'efivar -p -b -N -n 8be4df61-93ca-11d2-aa0d-00e098032b8c-KEK > "));
    assert!(calls[1].contains("d719b2cb-3d3a-4596-a3bc-dad00e67656f-db"));
}

#[test]
fn a_firmware_that_cannot_be_read_is_replaced() {
    let host = Host::new("novendor");
    host.sb().keygen(true, false).unwrap();
    let keys = host.keys();
    let cer = fs::read(keys.join("db.cer")).unwrap();
    assert_eq!(
        fs::read(keys.join("db.esl")).unwrap(),
        efi::esl(efi::OWNER, &cer)
    );
    host.set("db", b"chuck");
    let err = host.sb().keygen(true, true).unwrap_err().to_string();
    assert_eq!(err, "not a well-formed EFI signature list");
}

#[test]
fn a_dry_run_makes_nothing() {
    let dir = std::env::temp_dir().join(format!("jos-sb-{}-dry", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let runner = DryRunner::new();
    SecureBoot::new(&runner, true, dir.clone())
        .keygen(true, false)
        .unwrap();
    assert!(!dir.exists());
    let commands = runner.commands();
    assert_eq!(commands.len(), 2 + 6);
    assert!(commands[0].starts_with("sh -c "));
    assert!(commands[2].starts_with("openssl req -new -x509 -newkey rsa:2048"));
    assert!(commands[2].contains("-subj '/CN=JuanitOS PK/'"));
    assert!(commands[3].starts_with("openssl x509 -in "));
}

#[test]
fn every_failing_step_of_keygen_is_reported() {
    let clean = Host::new("clean");
    clean.sb().keygen(true, false).unwrap();
    let total = clean.calls().len();
    assert_eq!(total, 2 + 6 + 3);
    for n in 0..total {
        let owner = Host::new(&format!("fail{n}"));
        let result = owner.faulty(n, Fault::Fail).sb().keygen(true, false);
        // Only the firmware's lists are optional.
        assert_eq!(result.is_ok(), n < 2, "call {n}: {result:?}");
    }
    // A step that does nothing, and one that writes rubbish.
    let skipped = |n: usize, fault: Fault| {
        let owner = Host::new(&format!("odd{n}-{fault:?}"));
        let err = owner.faulty(n, fault).sb().keygen(true, false);
        err.unwrap_err().to_string()
    };
    let missing = skipped(3, Fault::Skip);
    assert!(missing.starts_with("reading "), "{missing}");
    assert!(missing.contains("PK.cer"), "{missing}");
    assert!(skipped(8, Fault::Skip).contains("signature"));
    assert!(skipped(8, Fault::Garbage).starts_with("malformed DER"));
}

#[test]
fn files_that_cannot_be_written_are_reported() {
    let host = Host::new("blocked");
    let keys = host.keys();
    fs::create_dir_all(keys.join("PK.esl")).unwrap();
    assert!(host.sb().keygen(false, true).is_err());
    fs::remove_dir(keys.join("PK.esl")).unwrap();
    fs::create_dir_all(keys.join("PK.auth")).unwrap();
    assert!(host.sb().keygen(false, true).is_err());
    assert!(keys.join("PK.esl").is_file(), "written before the .auth");

    let file = host.dir.join("file");
    fs::write(&file, "x").unwrap();
    let under = SecureBoot::new(&host, false, file.join("keys"));
    assert!(under.keygen(false, false).is_err());
}

#[test]
fn images_are_signed_so_that_firmware_accepts_them() {
    let host = Host::keyed("images");
    let sb = host.sb();
    for pe32 in [false, true] {
        let image = pe::tests::image(pe32);
        let signed = sb.sign_image(&image).unwrap();
        // What is signed is the image padded to 8 bytes.
        let digest = pe::prepare(&image).unwrap().digest;
        assert_ne!(pe::authenticode_digest(&image).unwrap(), digest);
        assert_eq!(pe::authenticode_digest(&signed).unwrap(), digest);
        assert!(signed.len() > image.len());
        let (signature, content) = embedded(&signed);
        assert!(content.ends_with(&digest));
        assert!(verifies(&host, &signature, &content, "db.crt"));
        assert!(!verifies(&host, &signature, &content, "KEK.crt"));
        let mut tampered = content.clone();
        tampered[0] ^= 1;
        assert!(!verifies(&host, &signature, &tampered, "db.crt"));

        let cer = |name: &str| fs::read(host.keys().join(name)).unwrap();
        assert_eq!(pe::signature(&signed, &cer("db.cer")), Signature::Ours);
        assert_eq!(pe::signature(&signed, &cer("KEK.cer")), Signature::Other);
        // Signing again replaces the signature.
        let again = sb.sign_image(&signed).unwrap();
        assert_eq!(pe::authenticode_digest(&again).unwrap(), digest);
        assert!(verifies(&host, &embedded(&again).0, &content, "db.crt"));
    }
    assert!(sb.sign_image(b"chuck").is_err());
}

#[test]
fn signing_without_a_sound_scratch_directory_fails() {
    let host = Host::keyed("scratch");
    let scratch = host.keys().join(format!(".sign-{}", std::process::id()));
    let image = pe::tests::image(false);
    fs::write(&scratch, "x").unwrap();
    assert!(host.sb().sign_image(&image).is_err());
    fs::remove_file(&scratch).unwrap();
    fs::create_dir_all(scratch.join("content")).unwrap();
    assert!(host.sb().sign_image(&image).is_err());
    assert!(!scratch.exists(), "removed after the failure too");
    assert!(host.sb().sign_image(&image).is_ok());
}

/// A system root with the boot files of `JuanitOS` and an ESP that shares
/// them with a foreign one.
fn esp(host: &Host) -> (PathBuf, PathBuf) {
    let (root, esp) = (host.dir.join("root"), host.dir.join("esp"));
    let put = |path: PathBuf, content: &[u8]| {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    };
    put(root.join("boot/jos-boot.efi"), &pe::tests::image(false));
    put(root.join("boot/loader.efi"), &other_image());
    put(esp.join(dom0::ESP_SHARED), &pe::tests::image(false));
    put(esp.join(dom0::ESP_LOADER), &other_image());
    put(esp.join(dom0::ESP_DEFAULT), b"chuck's boot manager");
    (root, esp)
}

#[test]
fn only_the_juanitos_boot_files_are_signed() {
    let host = Host::keyed("esp");
    let (root, esp) = esp(&host);
    let sb = host.sb();
    let foreign = fs::read(esp.join(dom0::ESP_DEFAULT)).unwrap();
    let status = sb.status(&esp);
    assert!(status.keys);
    assert_eq!(
        status.images,
        [
            (dom0::ESP_DEFAULT.to_string(), Signature::None),
            (dom0::ESP_SHARED.to_string(), Signature::None),
            (dom0::ESP_LOADER.to_string(), Signature::None),
        ]
    );

    // Twice: a signed file is still the same boot file.
    for _ in 0..2 {
        assert_eq!(sb.sign_esp(&esp, &root).unwrap(), 2);
        let status = sb.status(&esp);
        assert_eq!(
            status.images.iter().map(|(_, s)| *s).collect::<Vec<_>>(),
            [Signature::None, Signature::Ours, Signature::Ours]
        );
        assert_eq!(fs::read(esp.join(dom0::ESP_DEFAULT)).unwrap(), foreign);
    }
    let signed = fs::read(esp.join(dom0::ESP_SHARED)).unwrap();
    let (signature, content) = embedded(&signed);
    assert!(verifies(&host, &signature, &content, "db.crt"));
    assert!(!esp.join("EFI/JUANITOS/BOOTX64.signing").exists());
    // The unsigned original is what the system keeps in /boot.
    let source = fs::read(root.join("boot/jos-boot.efi")).unwrap();
    assert_eq!(pe::signature(&source, b"x"), Signature::None);

    // The public halves are on the ESP; the private ones are not.
    for var in Var::ALL {
        for ext in ["auth", "cer", "esl"] {
            let name = format!("{}.{ext}", var.name());
            assert_eq!(
                fs::read(esp.join(ESP_KEYS).join(&name)).unwrap(),
                fs::read(host.keys().join(&name)).unwrap()
            );
        }
        assert!(!esp
            .join(ESP_KEYS)
            .join(format!("{}.key", var.name()))
            .exists());
    }
}

#[test]
fn signing_the_esp_needs_something_to_sign() {
    let host = Host::keyed("esperr");
    let (root, esp) = esp(&host);
    let sb = host.sb();
    let err = |sb: &SecureBoot<'_>, esp: &Path, root: &Path| {
        sb.sign_esp(esp, root).unwrap_err().to_string()
    };

    // Boot files of the system that are no images, or missing.
    fs::write(root.join("boot/jos-boot.efi"), b"alice").unwrap();
    fs::remove_file(root.join("boot/loader.efi")).unwrap();
    assert!(err(&sb, &esp, &root).contains("no boot files to compare with under"));
    fs::write(root.join("boot/loader.efi"), pe::tests::image(true)).unwrap();
    assert!(err(&sb, &esp, &root).contains("is the ESP mounted there?"));

    // Another ESP.
    fs::write(root.join("boot/loader.efi"), other_image()).unwrap();
    let elsewhere = host.dir.join("elsewhere");
    assert!(err(&sb, &elsewhere, &root).contains("is the ESP mounted there?"));

    // Keys that are not there.
    let nowhere = SecureBoot::new(&host, false, host.dir.join("nowhere"));
    assert!(err(&nowhere, &esp, &root).contains("run `jos secureboot keygen` first"));

    // A boot file that cannot be signed.
    let temp = esp.join("EFI/FREEBSD/LOADER.signing");
    fs::create_dir_all(&temp).unwrap();
    assert!(sb.sign_esp(&esp, &root).is_err());
    fs::remove_dir(&temp).unwrap();

    // A public key that cannot be put on the ESP.
    fs::write(esp.join("EFI/JUANITOS/keys"), "x").unwrap();
    assert!(sb.sign_esp(&esp, &root).is_err());
}

#[test]
fn one_file_is_signed_in_place_or_not_at_all() {
    let host = Host::keyed("file");
    let sb = host.sb();
    let path = host.dir.join("boot.efi");
    let image = pe::tests::image(false);
    fs::write(&path, &image).unwrap();

    let dry = SecureBoot::new(&host, true, host.keys());
    dry.sign_file(&path).unwrap();
    assert_eq!(fs::read(&path).unwrap(), image);
    sb.sign_file(&path).unwrap();
    assert!(embedded(&fs::read(&path).unwrap())
        .0
        .starts_with(&[0x30, 0x82]));

    // A full disk leaves the old file: here, the new one has no place.
    let signed = fs::read(&path).unwrap();
    fs::create_dir_all(host.dir.join("boot.signing")).unwrap();
    assert!(sb.sign_file(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), signed);

    let err = sb
        .sign_file(&host.dir.join("none"))
        .unwrap_err()
        .to_string();
    assert!(err.starts_with("reading "), "{err}");
    fs::write(&path, b"chuck").unwrap();
    assert!(sb.sign_file(&path).is_err());
    let nowhere = SecureBoot::new(&host, false, host.dir.join("nowhere"));
    assert!(nowhere.sign_file(&path).is_err());
}

#[test]
fn the_state_of_the_firmware_is_read_through_efivar() {
    let host = Host::keyed("status");
    let sb = host.sb();
    let esp = host.dir.join("esp");
    let unknown = sb.status(&esp);
    assert_eq!(
        (unknown.secure_boot, unknown.setup_mode, unknown.keys),
        (None, None, true)
    );
    assert!(unknown.images.is_empty());
    host.set("SecureBoot", &[1]);
    host.set("SetupMode", &[0]);
    let user = sb.status(&esp);
    assert_eq!(
        (user.secure_boot, user.setup_mode),
        (Some(true), Some(false))
    );
    host.set("SecureBoot", &[0]);
    host.set("SetupMode", &[1]);
    let setup = sb.status(&esp);
    assert_eq!(
        (setup.secure_boot, setup.setup_mode),
        (Some(false), Some(true))
    );
    let calls = host.calls();
    assert_eq!(
        calls[calls.len() - 1],
        "efivar -p -b -N -n 8be4df61-93ca-11d2-aa0d-00e098032b8c-SetupMode"
    );
    let bare = SecureBoot::new(&host, false, host.dir.join("nowhere")).status(&esp);
    assert!(!bare.keys);
}

#[test]
fn the_status_reads_well() {
    let status = Status {
        secure_boot: Some(true),
        setup_mode: Some(false),
        keys: true,
        images: vec![
            ("EFI/BOOT/BOOTX64.EFI".to_string(), Signature::Ours),
            ("EFI/FREEBSD/LOADER.EFI".to_string(), Signature::Other),
            ("EFI/JUANITOS/BOOTX64.EFI".to_string(), Signature::None),
        ],
    };
    assert_eq!(
        status.to_string(),
        "Secure Boot: enabled\nSetup mode:  no\nKeys:        generated\n\
         EFI/BOOT/BOOTX64.EFI: signed with the JuanitOS key\n\
         EFI/FREEBSD/LOADER.EFI: signed by someone else\n\
         EFI/JUANITOS/BOOTX64.EFI: not signed\n"
    );
    let status = Status {
        secure_boot: Some(false),
        setup_mode: Some(true),
        keys: false,
        images: Vec::new(),
    };
    assert_eq!(
        status.to_string(),
        "Secure Boot: disabled\nSetup mode:  yes\nKeys:        none (jos secureboot keygen)\n"
    );
    let status = Status {
        secure_boot: None,
        setup_mode: None,
        keys: false,
        images: Vec::new(),
    };
    assert!(status
        .to_string()
        .starts_with("Secure Boot: unknown (not booted through EFI?)\nSetup mode:  unknown"));
}

#[test]
fn enrolling_writes_the_signed_lists_in_order() {
    let host = Host::keyed("enroll");
    host.set("SetupMode", &[1]);
    host.sb().enroll().unwrap();
    let calls = host.calls();
    let writes: Vec<_> = calls.iter().filter(|c| c.contains("efivar -w")).collect();
    assert_eq!(writes.len(), 3);
    let guid = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
    assert!(writes[0].contains("-t 0x27 -n d719b2cb-3d3a-4596-a3bc-dad00e67656f-db -f "));
    assert!(writes[1].contains(&format!("-n {guid}-KEK -f ")));
    assert!(writes[2].contains(&format!("-n {guid}-PK -f ")));
    for var in Var::ALL {
        let name = var.name();
        assert_eq!(
            host.read(&format!("efi/written-{name}")),
            host.read(&format!("keys/{name}.auth"))
        );
    }
}

#[test]
fn enrolling_needs_keys_and_a_firmware_in_setup_mode() {
    let host = Host::keyed("noenroll");
    for state in [&[0][..], &[][..]] {
        host.set("SetupMode", state);
        let err = host.sb().enroll().unwrap_err().to_string();
        assert!(err.contains("not in setup mode"), "{err}");
        assert!(err.contains(ESP_KEYS), "{err}");
    }
    assert!(!host.dir.join("efi/written-PK").exists());
    let nowhere = SecureBoot::new(&host, false, host.dir.join("nowhere"));
    assert!(nowhere.enroll().is_err());

    // Each command that fails stops it: the lookup, then every write.
    host.set("SetupMode", &[1]);
    for n in 0..4 {
        let err = host
            .faulty(n, Fault::Fail)
            .sb()
            .enroll()
            .unwrap_err()
            .to_string();
        let expected = if n == 0 {
            "not in setup mode"
        } else {
            "mock failure"
        };
        assert!(err.contains(expected), "call {n}: {err}");
    }
}
