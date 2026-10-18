//! Disk discovery on the live medium (`kern.disks` + `diskinfo`).

use juanitos_core::sys::Runner;
use juanitos_core::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    /// Device name without `/dev/`, e.g. `ada0`, `nda0`, `vtbd0`.
    pub name: String,
    pub size_bytes: u64,
    pub description: String,
}

impl Disk {
    pub fn size_gib(&self) -> f64 {
        self.size_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
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

/// The disk the running system booted from (the USB stick holding the
/// live medium), which must never be offered as an install target.
pub fn boot_disk(runner: &dyn Runner) -> Option<String> {
    let mounts = runner.run("mount", &["-p"]).ok().filter(|o| o.success())?;
    let root = parse_root_device(&mounts.stdout)?;
    let geom = runner
        .run("sysctl", &["-n", "kern.geom.conftxt"])
        .ok()
        .filter(|o| o.success())?;
    parse_conftxt_disk(&geom.stdout, &root)
}

pub fn list_disks(runner: &dyn Runner) -> Result<Vec<Disk>> {
    let out = runner.run("sysctl", &["-n", "kern.disks"])?;
    if !out.success() {
        return Ok(Vec::new());
    }
    let boot = boot_disk(runner);
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
        disks.push(Disk {
            name,
            size_bytes,
            description,
        });
    }
    Ok(disks)
}

#[cfg(test)]
mod tests {
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
            assert_eq!(boot_disk(&m).as_deref(), Some("da0"));
            let names: Vec<String> = list_disks(&m)
                .unwrap()
                .into_iter()
                .map(|d| d.name)
                .collect();
            assert_eq!(names, vec!["vtbd0"]);
        }

        // Booted from a CD: nothing to hide among the disks.
        let cd = MockRunner::new()
            .respond(
                "mount",
                &[],
                CmdOutput::ok("/dev/iso9660/JUANITOS / cd9660 ro 1 1\n"),
            )
            .respond(
                "sysctl",
                &[],
                CmdOutput::ok(
                    "0 DISK cd0 8436797440 2048 hd 0 sc 0\n\
                     1 LABEL iso9660/JUANITOS 8436797440 2048 i 0 o 0\n\
                     0 DISK ada0 500107862016 512 hd 16 sc 63\n",
                ),
            );
        assert_eq!(boot_disk(&cd).as_deref(), Some("cd0"));
        // An installed system on a plain partition; no labels at all.
        let disk = MockRunner::new()
            .respond("mount", &[], CmdOutput::ok("/dev/ada0p2 / ufs rw 1 1\n"))
            .respond(
                "sysctl",
                &[],
                CmdOutput::ok(
                    "0 DISK ada0 500107862016 512 hd 16 sc 63\n\
                     1 PART ada0p2 499000000000 512 i 2 o 1048576 ty freebsd-ufs xs GPT xt x\n\
                     1 PART ada0p1 1048576 512 i 1 o 20480 ty efi xs GPT xt x\n",
                ),
            );
        assert_eq!(boot_disk(&disk).as_deref(), Some("ada0"));
        // A root GEOM does not know (a ZFS dataset), a failing sysctl, a
        // failing mount: no disk is hidden rather than the wrong one.
        let zfs = MockRunner::new()
            .respond(
                "mount",
                &[],
                CmdOutput::ok("zroot/ROOT/default / zfs rw 1 1\n"),
            )
            .respond("sysctl", &[], CmdOutput::ok(DD_STICK));
        assert_eq!(boot_disk(&zfs), None);
        let no_geom = MockRunner::new()
            .respond("mount", &[], CmdOutput::ok("/dev/ada0p2 / ufs rw 1 1\n"))
            .respond("sysctl", &[], CmdOutput::failed(1, ""));
        assert_eq!(boot_disk(&no_geom), None);
        assert_eq!(
            boot_disk(&MockRunner::new().respond("mount", &[], CmdOutput::failed(1, ""))),
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
        assert_eq!(boot_disk(&m), None);
        let unknown = Disk {
            name: "ada0".into(),
            size_bytes: 0,
            description: String::new(),
        };
        assert_eq!(list_disks(&m).unwrap(), [unknown]);
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
        };
        assert!((d.size_gib() - 465.76).abs() < 0.01);
    }
}
