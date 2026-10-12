//! Just enough GPT to find a disk image's EFI system partition, and a
//! helper that drops a file into it with `mtools` (no root, no loop
//! devices). Used to give the stock FreeBSD VM image a serial console
//! through `/efi/freebsd/loader.env`, which FreeBSD 15's `loader.efi`
//! reads from the ESP before anything else.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

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
    /// Size in bytes.
    pub fn byte_len(&self) -> u64 {
        (self.last_lba + 1).saturating_sub(self.first_lba) * SECTOR
    }
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4)?.try_into().ok().map(u32::from_le_bytes)
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    b.get(off..off + 8)?.try_into().ok().map(u64::from_le_bytes)
}

/// Find the first partition of type `type_guid` in a raw GPT disk image
/// prefix (`head` must cover the header at LBA 1 and the entry array).
pub fn find_partition(head: &[u8], type_guid: &[u8; 16]) -> anyhow::Result<Option<Partition>> {
    let hdr = head
        .get(SECTOR as usize..2 * SECTOR as usize)
        .context("image too short for a GPT header")?;
    if hdr.get(..8) != Some(b"EFI PART") {
        bail!("no GPT header at LBA 1");
    }
    let entries_lba = u64_at(hdr, 72).context("gpt")?;
    let count = u32_at(hdr, 80).context("gpt")? as usize;
    let size = u32_at(hdr, 84).context("gpt")? as usize;
    if size < 128 {
        bail!("unexpected GPT entry size {size}");
    }
    let start = usize::try_from(entries_lba * SECTOR).context("gpt")?;
    for i in 0..count {
        let Some(e) = head.get(start + i * size..start + (i + 1) * size) else {
            break;
        };
        if e.get(..16) != Some(&type_guid[..]) {
            continue;
        }
        let (Some(first), Some(last)) = (u64_at(e, 32), u64_at(e, 40)) else {
            continue;
        };
        return Ok(Some(Partition {
            index: i + 1,
            first_lba: first,
            last_lba: last,
        }));
    }
    Ok(None)
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
/// the ESP of a raw disk image, creating directories. Needs `mmd`/`mcopy`
/// from mtools; the partition is copied out, edited and copied back so no
/// privileged block-device access is required.
pub fn write_into_esp(raw_image: &Path, path_in_esp: &str, content: &[u8]) -> anyhow::Result<()> {
    let esp = find_esp(raw_image)?;
    let len = usize::try_from(esp.byte_len()).context("ESP size")?;
    let mut disk = OpenOptions::new().read(true).write(true).open(raw_image)?;
    let mut part = vec![0u8; len];
    disk.seek(SeekFrom::Start(esp.offset()))?;
    disk.read_exact(&mut part).context("reading the ESP")?;

    let tmp = std::env::temp_dir().join(format!("jos-esp-{}.img", std::process::id()));
    std::fs::write(&tmp, &part)?;
    let src = std::env::temp_dir().join(format!("jos-esp-{}.file", std::process::id()));
    std::fs::write(&src, content)?;
    let result = (|| {
        let tmp_s = tmp.to_string_lossy().into_owned();
        // Create each missing directory level. mtools prompts on stdin
        // when a name exists with different case, so check with mdir
        // first and keep stdin closed.
        let mut dir = String::new();
        let file_name = path_in_esp.rsplit('/').next().unwrap_or("");
        for comp in path_in_esp.trim_start_matches('/').split('/') {
            if comp.is_empty() || comp == file_name {
                break;
            }
            dir = if dir.is_empty() {
                comp.to_string()
            } else {
                format!("{dir}/{comp}")
            };
            let exists = Command::new("mdir")
                .args(["-i", &tmp_s, &format!("::/{dir}")])
                .stdin(std::process::Stdio::null())
                .output()
                .context("running mdir (is mtools installed?)")?
                .status
                .success();
            if !exists {
                let out = Command::new("mmd")
                    .args(["-i", &tmp_s, &format!("::/{dir}")])
                    .stdin(std::process::Stdio::null())
                    .output()?;
                if !out.status.success() {
                    bail!("mmd {dir}: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            }
        }
        let out = Command::new("mcopy")
            .args([
                "-i",
                &tmp_s,
                "-o",
                &src.to_string_lossy(),
                &format!("::/{}", path_in_esp.trim_start_matches('/')),
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .context("running mcopy (is mtools installed?)")?;
        if !out.status.success() {
            bail!("mcopy: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let edited = std::fs::read(&tmp)?;
        if edited.len() != len {
            bail!("mtools changed the partition size");
        }
        disk.seek(SeekFrom::Start(esp.offset()))?;
        disk.write_all(&edited)?;
        disk.flush()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&src);
    result
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

    fn synthetic_gpt() -> Vec<u8> {
        let mut d = vec![0u8; GPT_HEAD_LEN];
        let hdr = 512;
        d[hdr..hdr + 8].copy_from_slice(b"EFI PART");
        d[hdr + 72..hdr + 80].copy_from_slice(&2u64.to_le_bytes()); // entries at LBA 2
        d[hdr + 80..hdr + 84].copy_from_slice(&128u32.to_le_bytes());
        d[hdr + 84..hdr + 88].copy_from_slice(&128u32.to_le_bytes());
        let e0 = 2 * 512;
        d[e0..e0 + 16].copy_from_slice(&[1u8; 16]); // some other type
        d[e0 + 32..e0 + 40].copy_from_slice(&34u64.to_le_bytes());
        d[e0 + 40..e0 + 48].copy_from_slice(&380u64.to_le_bytes());
        let e1 = e0 + 128;
        d[e1..e1 + 16].copy_from_slice(&ESP_TYPE_GUID);
        d[e1 + 32..e1 + 40].copy_from_slice(&381u64.to_le_bytes());
        d[e1 + 40..e1 + 48].copy_from_slice(&66964u64.to_le_bytes());
        d
    }

    #[test]
    fn finds_the_esp() {
        let p = find_partition(&synthetic_gpt(), &ESP_TYPE_GUID)
            .unwrap()
            .unwrap();
        assert_eq!(p.index, 2);
        assert_eq!(p.first_lba, 381);
        assert_eq!(p.offset(), 381 * 512);
        assert_eq!(p.byte_len(), (66964 - 381 + 1) * 512);
        assert!(find_partition(&synthetic_gpt(), &[9u8; 16])
            .unwrap()
            .is_none());
        assert!(find_partition(&vec![0u8; GPT_HEAD_LEN], &ESP_TYPE_GUID).is_err());
    }

    /// A GPT disk with one 4 MB FAT12 ESP, built with mformat.
    #[test]
    fn writes_a_file_into_the_esp() {
        if Command::new("mformat").arg("--help").output().is_err() {
            eprintln!("skipped: mtools not installed");
            return;
        }
        let dir = std::env::temp_dir().join(format!("jos-disk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let esp = dir.join("esp.img");
        let st = Command::new("mformat")
            .args(["-C", "-i", &esp.to_string_lossy(), "-T", "8192", "::"])
            .status()
            .unwrap();
        assert!(st.success());
        let esp_bytes = std::fs::read(&esp).unwrap();
        let first = 2048u64;
        let last = first + esp_bytes.len() as u64 / SECTOR - 1;
        let mut head = synthetic_gpt();
        let e1 = 2 * 512 + 128;
        head[e1 + 32..e1 + 40].copy_from_slice(&first.to_le_bytes());
        head[e1 + 40..e1 + 48].copy_from_slice(&last.to_le_bytes());
        let mut disk = head;
        disk.resize(first as usize * 512, 0);
        disk.extend_from_slice(&esp_bytes);
        let img = dir.join("disk.raw");
        std::fs::write(&img, &disk).unwrap();

        write_into_esp(&img, "efi/freebsd/loader.env", b"boot_serial=YES\n").unwrap();
        // Idempotent: directories exist now.
        write_into_esp(&img, "efi/freebsd/loader.env", b"boot_serial=YES\nx=1\n").unwrap();

        let edited = std::fs::read(&img).unwrap();
        let part = &edited[first as usize * 512..];
        let out = dir.join("part.img");
        std::fs::write(&out, part).unwrap();
        let got = Command::new("mtype")
            .args(["-i", &out.to_string_lossy(), "::/efi/freebsd/loader.env"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&got.stdout),
            "boot_serial=YES\nx=1\n"
        );
        assert_eq!(edited.len(), disk.len(), "the image size must not change");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn serial_env_sets_both_consoles() {
        assert!(SERIAL_LOADER_ENV.contains("boot_serial=YES"));
        assert!(SERIAL_LOADER_ENV.contains("boot_multicons=YES"));
        assert!(SERIAL_LOADER_ENV.contains("hw.uart.console=io:0x3f8"));
    }
}
