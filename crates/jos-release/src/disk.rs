//! Just enough GPT to find a disk image's EFI system partition, and a
//! helper that drops a file into it with `mtools` (no root, no loop
//! devices). Used to give the stock FreeBSD VM image a serial console
//! through `/efi/freebsd/loader.env`, which FreeBSD 15's `loader.efi`
//! reads from the ESP before anything else.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{bail, Context};
use juanitos_core::sys::Runner;

pub const SECTOR: u64 = 512;

/// EFI System Partition type GUID, as the 16 mixed-endian bytes found on
/// disk (C12A7328-F81F-11D2-BA4B-00A0C93EC93B).
pub const ESP_TYPE_GUID: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    pub index: usize,
    pub first_lba: u64,
    pub last_lba: u64,
}

impl Partition {
    pub fn offset(&self) -> u64 {
        self.first_lba * SECTOR
    }
}

/// The little-endian `u64` at `off`; 0 past the end of `b`.
fn u64_at(b: &[u8], off: usize) -> u64 {
    b.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map_or(0, |c| u64::from_le_bytes(*c))
}

/// The little-endian `u32` at `off`; 0 past the end of `b`.
fn u32_at(b: &[u8], off: usize) -> u32 {
    b.get(off..)
        .and_then(<[u8]>::first_chunk)
        .map_or(0, |c| u32::from_le_bytes(*c))
}

/// Find the first partition of type `type_guid` in a raw GPT disk image
/// prefix (`head` must cover the header at LBA 1 and the entry array;
/// entries past its end are not seen).
pub fn find_partition(head: &[u8], type_guid: &[u8; 16]) -> anyhow::Result<Option<Partition>> {
    let hdr = head
        .get(SECTOR as usize..2 * SECTOR as usize)
        .context("image too short for a GPT header")?;
    if hdr.get(..8) != Some(b"EFI PART") {
        bail!("no GPT header at LBA 1");
    }
    let start = usize::try_from(u64_at(hdr, 72).saturating_mul(SECTOR)).unwrap_or(usize::MAX);
    let count = u32_at(hdr, 80) as usize;
    let size = u32_at(hdr, 84) as usize;
    if size < 128 {
        bail!("unexpected GPT entry size {size}");
    }
    Ok(head
        .get(start..)
        .unwrap_or_default()
        .chunks_exact(size)
        .take(count)
        .enumerate()
        .find(|(_, e)| e.get(..16) == Some(&type_guid[..]))
        .map(|(i, e)| Partition {
            index: i + 1,
            first_lba: u64_at(e, 32),
            last_lba: u64_at(e, 40),
        }))
}

/// Bytes needed from the start of a disk to parse its GPT (header plus a
/// standard 128-entry table).
pub const GPT_HEAD_LEN: usize = 34 * SECTOR as usize;

pub fn find_esp(raw_image: &Path) -> anyhow::Result<Partition> {
    let mut f = File::open(raw_image)?;
    let mut head = vec![0u8; GPT_HEAD_LEN];
    f.read_exact(&mut head)
        .with_context(|| format!("reading the GPT of {}", raw_image.display()))?;
    find_partition(&head, &ESP_TYPE_GUID)?
        .with_context(|| format!("{} has no EFI system partition", raw_image.display()))
}

/// Write `content` as `path_in_esp` (e.g. `efi/freebsd/loader.env`) into
/// the ESP of a raw disk image, creating directories. Needs `mdir`, `mmd`
/// and `mcopy` from mtools, which edit the partition in place
/// (`-i image@@offset`): no privileged block-device access is required.
pub fn write_into_esp(
    runner: &dyn Runner,
    raw_image: &Path,
    path_in_esp: &str,
    content: &str,
) -> anyhow::Result<()> {
    let image = format!("{}@@{}", raw_image.display(), find_esp(raw_image)?.offset());
    let path = path_in_esp.trim_start_matches('/');
    // Create each missing directory level. mtools prompts on stdin when a
    // name exists with different case, so check with mdir first (the
    // runner keeps stdin closed).
    for (end, _) in path.match_indices('/') {
        let dir = format!("::/{}", path.get(..end).unwrap_or_default());
        if !runner.run("mdir", &["-i", &image, &dir])?.success() {
            runner.run_checked("mmd", &["-i", &image, &dir])?;
        }
    }
    let dest = format!("::/{path}");
    runner.run_with_input_checked("mcopy", &["-i", &image, "-o", "-", &dest], content)?;
    Ok(())
}

/// `loader.env` that puts FreeBSD's loader and kernel on the serial port
/// as well as the screen (what `-serial stdio` needs for an unattended
/// guest). `/etc/ttys` runs a getty on `ttyu0` whenever it is a console.
pub const SERIAL_LOADER_ENV: &str = "boot_multicons=YES\n\
boot_serial=YES\n\
hw.uart.console=io:0x3f8,br:115200\n\
autoboot_delay=2\n";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_gpt as gpt;
    use std::process::Command;

    fn synthetic_gpt() -> Vec<u8> {
        gpt::head(&[([1u8; 16], 34, 380), (gpt::ESP, 381, 66964)])
    }

    #[test]
    fn finds_the_esp() {
        assert_eq!(gpt::ESP, ESP_TYPE_GUID);
        let p = find_partition(&synthetic_gpt(), &ESP_TYPE_GUID)
            .unwrap()
            .unwrap();
        assert_eq!(p.index, 2);
        assert_eq!(p.first_lba, 381);
        assert_eq!(p.offset(), 381 * 512);
        assert_eq!(p.last_lba, 66964);
        assert_eq!(find_partition(&synthetic_gpt(), &[9u8; 16]).unwrap(), None);
        // Entries past the end of what was read are not seen.
        let short = synthetic_gpt();
        assert_eq!(
            find_partition(&short[..2 * 512 + 200], &ESP_TYPE_GUID).unwrap(),
            None
        );
    }

    #[test]
    fn rejects_what_is_not_a_gpt() {
        let err = |head: &[u8]| {
            find_partition(head, &ESP_TYPE_GUID)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(err(&[0u8; 600]), "image too short for a GPT header");
        assert_eq!(err(&[0u8; GPT_HEAD_LEN]), "no GPT header at LBA 1");
        let mut small = synthetic_gpt();
        small[512 + 84..512 + 88].copy_from_slice(&64u32.to_le_bytes());
        assert_eq!(err(&small), "unexpected GPT entry size 64");
        // An entry table beyond any address: nothing found.
        let mut far = synthetic_gpt();
        far[512 + 72..512 + 80].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(find_partition(&far, &ESP_TYPE_GUID).unwrap(), None);
    }

    #[test]
    fn find_esp_reads_the_image() {
        let dir = std::env::temp_dir().join(format!("jos-find-esp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("disk.raw");
        assert!(find_esp(&img).is_err(), "missing");
        std::fs::write(&img, [0u8; 512]).unwrap();
        assert_eq!(
            find_esp(&img).unwrap_err().to_string(),
            format!("reading the GPT of {}", img.display())
        );
        std::fs::write(&img, [0u8; GPT_HEAD_LEN]).unwrap();
        assert_eq!(
            find_esp(&img).unwrap_err().to_string(),
            "no GPT header at LBA 1"
        );
        std::fs::write(&img, gpt::head(&[([1u8; 16], 34, 380)])).unwrap();
        assert_eq!(
            find_esp(&img).unwrap_err().to_string(),
            format!("{} has no EFI system partition", img.display())
        );
        std::fs::write(&img, synthetic_gpt()).unwrap();
        assert_eq!(find_esp(&img).unwrap().first_lba, 381);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A GPT disk with one 4 MB FAT12 ESP, built with mformat. Needs mtools.
    #[test]
    fn writes_a_file_into_the_esp() {
        let dir = std::env::temp_dir().join(format!("jos-disk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("disk.raw");
        gpt::esp_disk(&img);
        let len = std::fs::metadata(&img).unwrap().len();
        let real = juanitos_core::sys::RealRunner;

        write_into_esp(&real, &img, "efi/freebsd/loader.env", "boot_serial=YES\n").unwrap();
        // Idempotent: directories exist now.
        write_into_esp(
            &real,
            &img,
            "/efi/freebsd/loader.env",
            "boot_serial=YES\nx=1\n",
        )
        .unwrap();

        let part = format!("{}@@{}", img.display(), gpt::ESP_FIRST_LBA * SECTOR);
        let got = Command::new("mtype")
            .args(["-i", &part, "::/efi/freebsd/loader.env"])
            .output()
            .expect("this test needs mtools (mtype) on PATH");
        assert_eq!(
            String::from_utf8_lossy(&got.stdout),
            "boot_serial=YES\nx=1\n"
        );
        assert_eq!(std::fs::metadata(&img).unwrap().len(), len);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Where each mtools failure stops the edit.
    #[test]
    fn mtools_failures_stop_the_edit() {
        use juanitos_core::sys::{CmdOutput, MockRunner};
        let dir = std::env::temp_dir().join(format!("jos-esp-mock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("disk.raw");
        std::fs::write(&img, synthetic_gpt()).unwrap();
        let i = format!("{}@@{}", img.display(), 381 * SECTOR);
        let mdir = |d: &str| format!("mdir -i {i} ::/{d}");
        let mmd = |d: &str| format!("mmd -i {i} ::/{d}");
        let mcopy = format!("mcopy -i {i} -o - ::/efi/freebsd/loader.env");
        let write = |r: &MockRunner| write_into_esp(r, &img, "efi/freebsd/loader.env", "x=1\n");

        let present = MockRunner::new();
        write(&present).unwrap();
        assert_eq!(
            present.calls(),
            [mdir("efi"), mdir("efi/freebsd"), mcopy.clone()]
        );

        let absent = || MockRunner::new().respond("mdir", &[], CmdOutput::failed(1, ""));
        let all = [
            mdir("efi"),
            mmd("efi"),
            mdir("efi/freebsd"),
            mmd("efi/freebsd"),
            mcopy,
        ];
        let r = absent();
        write(&r).unwrap();
        assert_eq!(r.calls(), all);
        // The mmd, then the mcopy, fails.
        for n in [1, 4] {
            let r = absent().fail_call(n);
            assert!(write(&r).is_err());
            assert_eq!(r.calls(), all[..=n]);
        }
        let r = MockRunner::new().missing("mdir");
        assert!(write(&r).is_err());
        assert_eq!(r.calls(), all[..1]);

        std::fs::write(&img, [0u8; GPT_HEAD_LEN]).unwrap();
        let r = MockRunner::new();
        assert!(write(&r).is_err());
        assert_eq!(r.calls(), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serial_env_sets_both_consoles() {
        assert!(SERIAL_LOADER_ENV.contains("boot_serial=YES"));
        assert!(SERIAL_LOADER_ENV.contains("boot_multicons=YES"));
        assert!(SERIAL_LOADER_ENV.contains("hw.uart.console=io:0x3f8"));
    }
}
