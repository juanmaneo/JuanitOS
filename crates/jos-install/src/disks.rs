// SPDX-License-Identifier: BSD-3-Clause
//! Disk discovery on the live medium (`kern.disks` + `diskinfo`), and what
//! is already on each disk (`kern.geom.conftxt`): its partitions, their
//! GPT labels and the free space between them, for installing next to
//! the systems already there.

use juanitos_core::sys::Runner;
use juanitos_core::Result;

const MIB: u64 = 1 << 20;

/// GPT labels the install gives its partitions (`/dev/gpt/<label>`).
pub const ESP_LABEL: &str = "efiboot0";
pub const SWAP_LABEL: &str = "swap0";
pub const ZFS_LABEL: &str = "zfs0";

/// The smallest ZFS pool an install next to other systems accepts: the
/// dom0 itself takes a few GiB, templates and qubes the rest.
pub const MIN_POOL_GIB: u64 = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Disk {
    /// Device name without `/dev/`, e.g. `ada0`, `nda0`, `vtbd0`.
    pub name: String,
    pub size_bytes: u64,
    pub description: String,
    /// Partition table (`GPT`, `MBR`), when the disk has partitions.
    pub scheme: Option<String>,
    /// The disk's partitions, by offset.
    pub partitions: Vec<Partition>,
    /// GPT labels on the disk (`zfs0` for `/dev/gpt/zfs0`).
    pub labels: Vec<String>,
}

/// A partition, as GEOM describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Partition {
    /// Provider name, e.g. `nda0p1`.
    pub name: String,
    /// `gpart` type, e.g. `efi`, `linux-data`, `apple-apfs`.
    pub kind: String,
    /// Offset and size in bytes.
    pub offset: u64,
    pub size: u64,
}

/// Where an install next to other systems goes: the largest free area of
/// a GPT disk, in MiB, and the EFI system partition it shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreeSpace {
    /// The ESP's provider name, e.g. `nda0p1`.
    pub esp: String,
    pub start_mib: u64,
    pub size_mib: u64,
}

impl FreeSpace {
    pub fn size_gib(&self) -> f64 {
        self.size_mib as f64 / 1024.0
    }
}

impl Disk {
    pub fn size_gib(&self) -> f64 {
        self.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    /// The free areas between the partitions, `(start, size)` in bytes,
    /// aligned to 1 MiB. The first and the last MiB are left to the GPT
    /// headers.
    pub fn free_areas(&self) -> Vec<(u64, u64)> {
        let mut areas = Vec::new();
        let mut add = |from: u64, to: u64| {
            let start = from.div_ceil(MIB) * MIB;
            let end = to / MIB * MIB;
            if end > start {
                areas.push((start, end - start));
            }
        };
        let mut cursor = MIB;
        for p in &self.partitions {
            add(cursor, p.offset);
            cursor = cursor.max(p.offset + p.size);
        }
        add(cursor, self.size_bytes.saturating_sub(MIB));
        areas
    }

    /// Where JuanitOS would go next to the systems on this disk, or why it
    /// cannot. `all` are the machine's disks: the install's GPT labels must
    /// name its own partitions only.
    pub fn free_space(&self, all: &[Disk]) -> std::result::Result<FreeSpace, String> {
        if self.scheme.as_deref() != Some("GPT") {
            return Err("the disk has no GPT partition table".into());
        }
        let Some(esp) = self.partitions.iter().find(|p| p.kind == "efi") else {
            return Err("the disk has no EFI system partition".into());
        };
        if let Some((label, disk)) = all.iter().find_map(|d| {
            d.labels
                .iter()
                .find(|l| [SWAP_LABEL, ZFS_LABEL].contains(&l.as_str()))
                .map(|l| (l, &d.name))
        }) {
            return Err(format!(
                "{disk} already has a partition labelled {label} (an earlier install?)"
            ));
        }
        let (start, size) = self
            .free_areas()
            .into_iter()
            .max_by_key(|a| a.1)
            .unwrap_or_default();
        if size < MIN_POOL_GIB << 30 {
            return Err(format!(
                "{:.1} GiB of free space, JuanitOS needs {MIN_POOL_GIB} GiB",
                size as f64 / (1u64 << 30) as f64
            ));
        }
        Ok(FreeSpace {
            esp: esp.name.clone(),
            start_mib: start / MIB,
            size_mib: size / MIB,
        })
    }
}

/// Disk names from `sysctl -n kern.disks`, excluding optical drives
/// (`cd*`) and memory disks (`md*`), which are never install targets.
/// The kernel lists the most recently attached disk first; sorted by name
/// so the first disk of the machine comes first.
pub fn parse_kern_disks(text: &str) -> Vec<String> {
    let mut disks: Vec<String> = text
        .split_whitespace()
        .filter(|d| !d.starts_with("cd") && !d.starts_with("md"))
        .map(String::from)
        .collect();
    disks.sort();
    disks
}

/// `diskinfo -v` output: tab-separated `value\t# comment` lines.
pub fn parse_diskinfo(text: &str) -> (u64, String) {
    let mut size = 0;
    let mut descr = String::new();
    for line in text.lines() {
        let Some((value, comment)) = line.split_once('#') else {
            continue;
        };
        let value = value.trim();
        let comment = comment.trim();
        if comment.starts_with("mediasize in bytes") {
            size = value.parse().unwrap_or(0);
        } else if comment.starts_with("Disk descr") {
            descr = value.to_string();
        }
    }
    (size, descr)
}

/// Device mounted on `/`, from `mount -p` (fstab format), without `/dev/`.
pub fn parse_root_device(mount_p: &str) -> Option<String> {
    mount_p.lines().find_map(|line| {
        let mut f = line.split_whitespace();
        let dev = f.next()?;
        (f.next()? == "/").then(|| dev.trim_start_matches("/dev/").to_string())
    })
}

/// The disk under GEOM provider `name`, from `sysctl -n kern.geom.conftxt`:
/// `<depth> <class> <name> <mediasize> <sectorsize> ...` lines, depth
/// first, each hanging off the nearest shallower line above it, with the
/// disks at depth 0. This resolves whatever is stacked on the disk
/// (partitions, labels, Ventoy's `ventoy/IMAGE` whose name says nothing
/// about the stick it lives on), unlike guessing from the provider name.
pub fn parse_conftxt_disk(conftxt: &str, name: &str) -> Option<String> {
    let mut disk = None;
    for line in conftxt.lines() {
        let mut f = line.split_whitespace();
        let (Some(depth), Some(_class), Some(provider)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if depth == "0" {
            disk = Some(provider);
        }
        if provider == name {
            return disk.map(String::from);
        }
    }
    None
}

/// The lines of the `kern.geom.conftxt` subtree of `disk` (the disk's own
/// line excluded), split in fields.
fn conftxt_subtree<'a>(conftxt: &'a str, disk: &'a str) -> impl Iterator<Item = Vec<&'a str>> {
    let mut inside = false;
    conftxt.lines().filter_map(move |line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.first() == Some(&"0") {
            inside = f.get(2) == Some(&disk);
            return None;
        }
        (inside && f.len() >= 3).then_some(f)
    })
}

/// The value after `key` in a conftxt line's `key value` pairs.
fn conftxt_value<'a>(fields: &[&'a str], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .skip(5)
        .step_by(2)
        .position(|k| *k == key)
        .and_then(|i| fields.get(6 + 2 * i).copied())
}

/// The partition table of `disk` and its partitions, sorted by offset:
/// the `PART` lines right under the disk (depth 1).
pub fn parse_partitions(conftxt: &str, disk: &str) -> (Option<String>, Vec<Partition>) {
    let mut scheme = None;
    let mut parts: Vec<Partition> = conftxt_subtree(conftxt, disk)
        .filter(|f| f.first() == Some(&"1") && f.get(1) == Some(&"PART"))
        .map(|f| {
            scheme = conftxt_value(&f, "xs").map(String::from);
            let number = |key| {
                conftxt_value(&f, key)
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0)
            };
            Partition {
                name: f.get(2).copied().unwrap_or_default().to_string(),
                kind: conftxt_value(&f, "ty").unwrap_or_default().to_string(),
                offset: number("o"),
                size: f.get(3).and_then(|v| v.parse().ok()).unwrap_or(0),
            }
        })
        .collect();
    parts.sort_by_key(|p| p.offset);
    (scheme, parts)
}

/// The GPT labels on `disk` (`gpt/zfs0` gives `zfs0`).
pub fn parse_gpt_labels(conftxt: &str, disk: &str) -> Vec<String> {
    conftxt_subtree(conftxt, disk)
        .filter(|f| f.get(1) == Some(&"LABEL"))
        .filter_map(|f| {
            f.get(2)
                .copied()
                .unwrap_or_default()
                .strip_prefix("gpt/")
                .map(String::from)
        })
        .collect()
}

/// The disk the running system booted from (the USB stick holding the
/// live medium), which must never be offered as an install target.
pub fn boot_disk(runner: &dyn Runner, conftxt: &str) -> Option<String> {
    let mounts = runner.run("mount", &["-p"]).ok().filter(|o| o.success())?;
    let root = parse_root_device(&mounts.stdout)?;
    parse_conftxt_disk(conftxt, &root)
}

pub fn list_disks(runner: &dyn Runner) -> Result<Vec<Disk>> {
    let out = runner.run("sysctl", &["-n", "kern.disks"])?;
    if !out.success() {
        return Ok(Vec::new());
    }
    let conftxt = runner
        .run("sysctl", &["-n", "kern.geom.conftxt"])
        .ok()
        .filter(|o| o.success())
        .map(|o| o.stdout)
        .unwrap_or_default();
    let boot = boot_disk(runner, &conftxt);
    let mut disks = Vec::new();
    for name in parse_kern_disks(&out.stdout) {
        if boot.as_deref() == Some(name.as_str()) {
            continue;
        }
        let dev = format!("/dev/{name}");
        let (size_bytes, description) = match runner.run("diskinfo", &["-v", &dev]) {
            Ok(o) if o.success() => parse_diskinfo(&o.stdout),
            _ => (0, String::new()),
        };
        let (scheme, partitions) = parse_partitions(&conftxt, &name);
        disks.push(Disk {
            labels: parse_gpt_labels(&conftxt, &name),
            name,
            size_bytes,
            description,
            scheme,
            partitions,
        });
    }
    Ok(disks)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn kern_disks_filters_media() {
        assert_eq!(
            parse_kern_disks("vtbd1 vtbd0 cd0 ada0 md0 nda0\n"),
            vec!["ada0", "nda0", "vtbd0", "vtbd1"]
        );
        assert!(parse_kern_disks("").is_empty());
    }

    /// `kern.geom.conftxt` of the live medium written to a USB stick
    /// (`da0`), next to the install target `vtbd0`.
    const DD_STICK: &str = "\
0 DISK da0 8436797440 512 hd 255 sc 63
1 LABEL diskid/DISK-1-0000:00:03.0-1 8436797440 512 i 0 o 0
2 PART diskid/DISK-1-0000:00:03.0-1p2 8388608 512 i 2 o 40960 ty efi xs GPT xt c12a7328
2 PART diskid/DISK-1-0000:00:03.0-1p1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
1 LABEL iso9660/JUANITOS 8436797440 512 i 0 o 0
2 PART iso9660/JUANITOSp2 8388608 512 i 2 o 40960 ty efi xs GPT xt c12a7328
2 PART iso9660/JUANITOSp1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
1 PART da0p2 8388608 512 i 2 o 40960 ty efi xs GPT xt c12a7328
2 LABEL msdosfs/EFISYS 8388608 512 i 0 o 0
1 PART da0p1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
0 DISK cd0 0 2048 hd 0 sc 0
0 DISK vtbd0 21474836480 512 hd 16 sc 63
";

    /// The same ISO booted through Ventoy (as seen in QEMU): the root
    /// label sits on the `ventoy/IMAGE` provider, whose name does not
    /// contain the stick's name.
    const VENTOY_STICK: &str = "\
0 DISK da0 10737418240 512 hd 255 sc 63
1 LABEL diskid/DISK-1-0000:00:03.0-1 10737418240 512 i 0 o 0
2 PART diskid/DISK-1-0000:00:03.0-1s2 33554432 512 i 2 o 10703863808 ty efi xs MBR xt 239
2 PART diskid/DISK-1-0000:00:03.0-1s1 10702815232 512 i 1 o 1048576 ty ntfs xs MBR xt 7
1 PART da0s2 33554432 512 i 2 o 10703863808 ty efi xs MBR xt 239
2 LABEL msdosfs/VTOYEFI 33554432 512 i 0 o 0
1 PART da0s1 10702815232 512 i 1 o 1048576 ty ntfs xs MBR xt 7
2 LABEL ext2fs/Ventoy 10702815232 512 i 0 o 0
1 VENTOY ventoy/IMAGE 8436797440 512
2 LABEL iso9660/JUANITOS 8436797440 512 i 0 o 0
3 PART iso9660/JUANITOSp2 8388608 512 i 2 o 40960 ty efi xs GPT xt c12a7328
3 PART iso9660/JUANITOSp1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
2 PART ventoy/IMAGEp2 8388608 512 i 2 o 40960 ty efi xs GPT xt c12a7328
3 LABEL msdosfs/EFISYS 8388608 512 i 0 o 0
2 PART ventoy/IMAGEp1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
0 DISK cd0 0 2048 hd 0 sc 0
0 DISK vtbd0 21474836480 512 hd 16 sc 63
";

    #[test]
    fn boot_medium_is_never_an_install_target() {
        use juanitos_core::sys::{CmdOutput, MockRunner};
        let live = |conftxt: &str| {
            MockRunner::new()
                .respond(
                    "sysctl",
                    &["-n", "kern.disks"],
                    CmdOutput::ok("vtbd0 da0 cd0\n"),
                )
                .respond(
                    "sysctl",
                    &["-n", "kern.geom.conftxt"],
                    CmdOutput::ok(conftxt),
                )
                .respond(
                    "mount",
                    &["-p"],
                    CmdOutput::ok(
                        "/dev/iso9660/JUANITOS\t/\tcd9660\tro\t1 1\n\
                         tmpfs\t/tmp\ttmpfs\trw\t0 0\n",
                    ),
                )
        };
        for conftxt in [DD_STICK, VENTOY_STICK] {
            let m = live(conftxt);
            assert_eq!(boot_disk(&m, conftxt).as_deref(), Some("da0"));
            let names: Vec<String> = list_disks(&m)
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .collect();
            assert_eq!(names, vec!["vtbd0"]);
        }

        let root_on = |dev: &str| {
            MockRunner::new().respond("mount", &[], CmdOutput::ok(format!("{dev} / x rw 1 1\n")))
        };
        // Booted from a CD: nothing to hide among the disks.
        let cd = "0 DISK cd0 8436797440 2048 hd 0 sc 0\n\
                  1 LABEL iso9660/JUANITOS 8436797440 2048 i 0 o 0\n\
                  0 DISK ada0 500107862016 512 hd 16 sc 63\n";
        let iso = root_on("/dev/iso9660/JUANITOS");
        assert_eq!(boot_disk(&iso, cd).as_deref(), Some("cd0"));
        // An installed system on a plain partition; no labels at all.
        let nvme = root_on("/dev/nda0p2");
        assert_eq!(boot_disk(&nvme, LINUX_DISK).as_deref(), Some("nda0"));
        // A root GEOM does not know (a ZFS dataset), no conftxt (failing
        // sysctl), a failing mount: no disk is hidden rather than the
        // wrong one.
        assert_eq!(boot_disk(&root_on("zroot/ROOT/default"), DD_STICK), None);
        assert_eq!(boot_disk(&nvme, ""), None);
        assert_eq!(
            boot_disk(
                &MockRunner::new().respond("mount", &[], CmdOutput::failed(1, "")),
                DD_STICK
            ),
            None
        );
        assert_eq!(parse_root_device("tmpfs /tmp tmpfs rw 0 0\n"), None);
        assert_eq!(parse_root_device("\n/dev/ada0p2\n"), None);
    }

    #[test]
    fn listing_with_missing_or_failing_tools() {
        use juanitos_core::sys::{CmdOutput, MockRunner};
        assert!(list_disks(&MockRunner::new().missing("sysctl")).is_err());
        let no_disks = MockRunner::new().respond("sysctl", &[], CmdOutput::failed(1, ""));
        assert_eq!(list_disks(&no_disks).unwrap(), []);
        // No root in `mount -p`: no boot disk to hide. A disk `diskinfo`
        // cannot open is listed without size or description.
        let m = MockRunner::new()
            .respond("sysctl", &["-n", "kern.disks"], CmdOutput::ok("ada0\n"))
            .respond("mount", &[], CmdOutput::ok("tmpfs /tmp tmpfs rw 0 0\n"))
            .respond("diskinfo", &[], CmdOutput::failed(1, ""));
        assert_eq!(boot_disk(&m, DD_STICK), None);
        let unknown = Disk {
            name: "ada0".into(),
            ..Default::default()
        };
        assert_eq!(list_disks(&m).unwrap(), [unknown]);
    }

    /// A 512 GiB NVMe disk with a Linux install that was shrunk to make
    /// room: ESP, root, ~299 GiB free, swap at the end, then 4 GiB free.
    /// GEOM lists the partitions last to first.
    const LINUX_DISK: &str = "\
0 DISK nda0 549755813888 512 hd 0 sc 0
1 PART nda0p3 8589934592 512 i 3 o 536870912000 ty linux-swap xs GPT xt 0657fd6d
2 LABEL gptid/7c1d2e3f 8589934592 512 i 0 o 0
1 PART nda0p2 214748364800 512 i 2 o 315621376 ty linux-data xs GPT xt 0fc63daf
2 LABEL gpt/root 214748364800 512 i 0 o 0
1 PART nda0p1 314572800 512 i 1 o 1048576 ty efi xs GPT xt c12a7328
2 LABEL gpt/EFI 314572800 512 i 0 o 0
3 LABEL msdosfs/EFI 314572800 512 i 0 o 0
0 DISK da0 8436797440 512 hd 255 sc 63
1 PART da0p1 12800 512 i 1 o 17408 ty freebsd-boot xs GPT xt 83bd6b9d
2 LABEL gpt/zfs0 12800 512 i 0 o 0
";

    /// The disk above, as `list_disks` describes it: where an install
    /// next to Linux goes in the other modules' tests.
    pub(crate) fn linux_disk() -> Disk {
        let (scheme, partitions) = parse_partitions(LINUX_DISK, "nda0");
        Disk {
            name: "nda0".into(),
            size_bytes: 512 << 30,
            description: "NVMe".into(),
            scheme,
            partitions,
            labels: parse_gpt_labels(LINUX_DISK, "nda0"),
        }
    }

    #[test]
    fn partitions_and_labels_of_a_disk() {
        let d = linux_disk();
        assert_eq!(d.scheme.as_deref(), Some("GPT"));
        let parts: Vec<(&str, &str, u64, u64)> = d
            .partitions
            .iter()
            .map(|p| {
                (
                    p.name.as_str(),
                    p.kind.as_str(),
                    p.offset >> 20,
                    p.size >> 20,
                )
            })
            .collect();
        assert_eq!(
            parts,
            [
                ("nda0p1", "efi", 1, 300),
                ("nda0p2", "linux-data", 301, 200 << 10),
                ("nda0p3", "linux-swap", 500 << 10, 8 << 10),
            ]
        );
        // Labels of the partitions only, not of the filesystems in them
        // (msdosfs/) nor GPT ids, and not those of the next disk.
        assert_eq!(d.labels, ["root", "EFI"]);
        assert_eq!(parse_gpt_labels(LINUX_DISK, "da0"), ["zfs0"]);
        // The partitions under the ISO's labels (depth 2) are not the
        // stick's own.
        let (scheme, stick) = parse_partitions(DD_STICK, "da0");
        assert_eq!(scheme.as_deref(), Some("GPT"));
        let names: Vec<&str> = stick.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["da0p1", "da0p2"]);
        assert_eq!(
            parse_partitions(VENTOY_STICK, "da0").0.as_deref(),
            Some("MBR")
        );
        // A disk without partitions, a disk GEOM does not list, and lines
        // with missing or garbled values.
        assert_eq!(parse_partitions(DD_STICK, "vtbd0"), (None, vec![]));
        assert_eq!(parse_partitions(LINUX_DISK, "ada9"), (None, vec![]));
        let garbled = "0 DISK ada0 1 512\n1\n1 PART ada0p1 x 512 i 1 o\n";
        assert_eq!(
            parse_partitions(garbled, "ada0"),
            (
                None,
                vec![Partition {
                    name: "ada0p1".into(),
                    ..Default::default()
                }]
            )
        );
    }

    #[test]
    fn free_areas_are_aligned_gaps() {
        let d = linux_disk();
        assert_eq!(
            d.free_areas(),
            [(205_101 * MIB, 306_899 * MIB), (520_192 * MIB, 4_095 * MIB)]
        );
        // An empty disk is one area, the GPT headers aside; partitions off
        // the MiB grid round the gaps inwards; a partition inside another
        // (never on GPT, but cheap to tolerate) does not move the cursor
        // back; slivers under 1 MiB are not areas.
        let mut d = Disk {
            size_bytes: 100 * MIB,
            ..Default::default()
        };
        assert_eq!(d.free_areas(), [(MIB, 98 * MIB)]);
        let part = |offset, size| Partition {
            offset,
            size,
            ..Default::default()
        };
        d.partitions = vec![
            part(2 * MIB + 512, 10 * MIB),
            part(3 * MIB, MIB),
            part(12 * MIB + 512, 87 * MIB - 1024),
        ];
        assert_eq!(d.free_areas(), [(MIB, MIB)]);
        assert_eq!(Disk::default().free_areas(), []);
    }

    #[test]
    fn free_space_next_to_linux() {
        let d = linux_disk();
        let space = d.free_space(std::slice::from_ref(&d)).unwrap();
        assert_eq!(
            space,
            FreeSpace {
                esp: "nda0p1".into(),
                start_mib: 205_101,
                size_mib: 306_899,
            }
        );
        assert!((space.size_gib() - 299.7).abs() < 0.1);

        let reason = |d: &Disk, all: &[Disk]| d.free_space(all).unwrap_err();
        assert_eq!(
            reason(&Disk::default(), &[]),
            "the disk has no GPT partition table"
        );
        let mut mbr = d.clone();
        mbr.scheme = Some("MBR".into());
        assert_eq!(reason(&mbr, &[]), "the disk has no GPT partition table");
        let mut no_esp = d.clone();
        no_esp.partitions.remove(0);
        assert_eq!(reason(&no_esp, &[]), "the disk has no EFI system partition");
        // An earlier install, on this disk or on another one: its labels
        // would make /dev/gpt/zfs0 ambiguous.
        let mut chuck = Disk {
            name: "ada0".into(),
            labels: vec!["data".into(), ZFS_LABEL.into()],
            ..Default::default()
        };
        assert_eq!(
            reason(&d, &[d.clone(), chuck.clone()]),
            "ada0 already has a partition labelled zfs0 (an earlier install?)"
        );
        chuck.labels = vec![SWAP_LABEL.into()];
        assert!(reason(&d, &[chuck]).contains("labelled swap0"));
        // Too little room, or none at all.
        let mut small = d.clone();
        small.size_bytes = 216 << 30;
        small.partitions.pop();
        assert_eq!(
            reason(&small, &[]),
            "15.7 GiB of free space, JuanitOS needs 16 GiB"
        );
        small.size_bytes = 300 * MIB;
        small.partitions.truncate(1);
        assert_eq!(
            reason(&small, &[]),
            "0.0 GiB of free space, JuanitOS needs 16 GiB"
        );
    }

    #[test]
    fn listing_describes_what_is_on_each_disk() {
        use juanitos_core::sys::{CmdOutput, MockRunner};
        let m = MockRunner::new()
            .respond("sysctl", &["-n", "kern.disks"], CmdOutput::ok("nda0\n"))
            .respond(
                "sysctl",
                &["-n", "kern.geom.conftxt"],
                CmdOutput::ok(LINUX_DISK),
            )
            .respond("mount", &[], CmdOutput::ok("tmpfs /tmp tmpfs rw 0 0\n"))
            .respond(
                "diskinfo",
                &[],
                CmdOutput::ok("549755813888\t# mediasize in bytes\nNVMe\t# Disk descr.\n"),
            );
        assert_eq!(list_disks(&m).unwrap(), [linux_disk()]);
    }

    #[test]
    fn conftxt_resolves_providers_to_their_disk() {
        assert_eq!(parse_conftxt_disk(DD_STICK, "da0").as_deref(), Some("da0"));
        assert_eq!(
            parse_conftxt_disk(DD_STICK, "da0p2").as_deref(),
            Some("da0")
        );
        assert_eq!(
            parse_conftxt_disk(DD_STICK, "msdosfs/EFISYS").as_deref(),
            Some("da0")
        );
        assert_eq!(
            parse_conftxt_disk(DD_STICK, "vtbd0").as_deref(),
            Some("vtbd0")
        );
        assert_eq!(
            parse_conftxt_disk(VENTOY_STICK, "ventoy/IMAGE").as_deref(),
            Some("da0")
        );
        assert_eq!(
            parse_conftxt_disk(VENTOY_STICK, "iso9660/JUANITOSp2").as_deref(),
            Some("da0")
        );
        assert_eq!(parse_conftxt_disk(VENTOY_STICK, "nda0"), None);
        assert_eq!(parse_conftxt_disk("", "da0"), None);
        // Lines too short to be a provider are skipped; a provider listed
        // before any disk (never happens) resolves to nothing.
        assert_eq!(
            parse_conftxt_disk("garbage\n1 LABEL x/y 1 512\n0 DISK da0 1 512\n", "x/y"),
            None
        );
    }

    #[test]
    fn diskinfo_parsing() {
        let text = "/dev/ada0
\t512         \t# sectorsize
\t500107862016\t# mediasize in bytes (466G)
\t976773168   \t# mediasize in sectors
\t4096        \t# stripesize
\t0           \t# stripeoffset
\t969021      \t# Cylinders according to firmware.
\t16          \t# Heads according to firmware.
\t63          \t# Sectors according to firmware.
\tSamsung SSD 860 EVO 500GB\t# Disk descr.
\tS3Z9NB0K123456\t# Disk ident.
";
        let (size, descr) = parse_diskinfo(text);
        assert_eq!(size, 500107862016);
        assert_eq!(descr, "Samsung SSD 860 EVO 500GB");
        let d = Disk {
            name: "ada0".into(),
            size_bytes: size,
            description: descr,
            ..Default::default()
        };
        assert!((d.size_gib() - 465.76).abs() < 0.01);
    }
}
