//! Raw GPT disk images for tests: used by the `disk` unit tests and by the
//! CLI tests, where a fake `qemu-img convert` copies one into place as the
//! builder VM's disk.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::Path;
use std::process::Command;

/// The EFI system partition type GUID as stored on disk, written out
/// independently of `disk::ESP_TYPE_GUID` so each checks the other.
pub const ESP: [u8; 16] = [
    0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e, 0xc9, 0x3b,
];

/// LBA 0 to 33: a GPT header at LBA 1 and 128-byte entries from LBA 2,
/// one per `(type, first LBA, last LBA)`.
pub fn head(entries: &[([u8; 16], u64, u64)]) -> Vec<u8> {
    let mut d = vec![0u8; 34 * 512];
    let hdr = 512;
    d[hdr..hdr + 8].copy_from_slice(b"EFI PART");
    d[hdr + 72..hdr + 80].copy_from_slice(&2u64.to_le_bytes());
    d[hdr + 80..hdr + 84].copy_from_slice(&128u32.to_le_bytes());
    d[hdr + 84..hdr + 88].copy_from_slice(&128u32.to_le_bytes());
    for (i, (ty, first, last)) in entries.iter().enumerate() {
        let e = 2 * 512 + i * 128;
        d[e..e + 16].copy_from_slice(ty);
        d[e + 32..e + 40].copy_from_slice(&first.to_le_bytes());
        d[e + 40..e + 48].copy_from_slice(&last.to_le_bytes());
    }
    d
}

/// Where the ESP written by [`esp_disk`] starts.
pub const ESP_FIRST_LBA: u64 = 2048;

/// Write a disk image to `path` whose only partition is a 4 MB FAT ESP at
/// [`ESP_FIRST_LBA`], formatted by `mformat` (needs mtools).
pub fn esp_disk(path: &Path) {
    let fat = path.with_extension("fat");
    let _ = std::fs::remove_file(&fat);
    let st = Command::new("mformat")
        .args(["-C", "-i", &fat.to_string_lossy(), "-T", "8192", "::"])
        .status()
        .expect("this test needs mtools (mformat) on PATH");
    assert!(st.success());
    let fat_bytes = std::fs::read(&fat).unwrap();
    std::fs::remove_file(&fat).unwrap();
    let last = ESP_FIRST_LBA + fat_bytes.len() as u64 / 512 - 1;
    let mut disk = head(&[(ESP, ESP_FIRST_LBA, last)]);
    disk.resize(ESP_FIRST_LBA as usize * 512, 0);
    disk.extend_from_slice(&fat_bytes);
    std::fs::write(path, disk).unwrap();
}
