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

/// GEOM provider carrying label `name` (`iso9660/JUANITOS`), from
/// `glabel status -s` lines (`iso9660/JUANITOS  N/A  da0`).
pub fn parse_glabel_provider(status: &str, name: &str) -> Option<String> {
    status.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        match f.as_slice() {
            [label, _, provider] if *label == name => Some(provider.to_string()),
            _ => None,
        }
    })
}

/// Whole-disk name of a provider: `da0p2` / `ada0s1a` -> `da0` / `ada0`.
pub fn disk_of(provider: &str) -> String {
    let trimmed = provider.trim_end_matches(|c: char| c.is_ascii_lowercase());
    let base = trimmed.trim_end_matches(|c: char| c.is_ascii_digit());
    for sep in ['p', 's'] {
        if let Some(disk) = base.strip_suffix(sep) {
            if disk.ends_with(|c: char| c.is_ascii_digit()) && base.len() < trimmed.len() {
                return disk.to_string();
            }
        }
    }
    provider.to_string()
}

/// The disk the running system booted from (the USB stick holding the
/// live medium), which must never be offered as an install target.
pub fn boot_disk(runner: &dyn Runner) -> Option<String> {
    let mounts = runner.run("mount", &["-p"]).ok().filter(|o| o.success())?;
    let root = parse_root_device(&mounts.stdout)?;
    let provider = if root.contains('/') {
        let labels = runner
            .run("glabel", &["status", "-s"])
            .ok()
            .filter(|o| o.success())?;
        parse_glabel_provider(&labels.stdout, &root)?
    } else {
        root
    };
    Some(disk_of(&provider))
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

    #[test]
    fn boot_medium_is_never_an_install_target() {
        use juanitos_core::sys::{CmdOutput, MockRunner};
        let m = MockRunner::new()
            .respond("sysctl", &[], CmdOutput::ok("vtbd0 da0 cd0\n"))
            .respond(
                "mount",
                &["-p"],
                CmdOutput::ok(
                    "/dev/iso9660/JUANITOS\t/\tcd9660\tro\t1 1\n\
                     tmpfs\t/tmp\ttmpfs\trw\t0 0\n",
                ),
            )
            .respond(
                "glabel",
                &[],
                CmdOutput::ok("msdosfs/EFISYS  N/A  da0p2\niso9660/JUANITOS  N/A  da0\n"),
            );
        assert_eq!(boot_disk(&m).as_deref(), Some("da0"));
        let names: Vec<String> = list_disks(&m)
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["vtbd0"]);

        // Booted from a CD: nothing to hide among the disks.
        let cd = MockRunner::new()
            .respond(
                "mount",
                &[],
                CmdOutput::ok("/dev/iso9660/JUANITOS / cd9660 ro 1 1\n"),
            )
            .respond("glabel", &[], CmdOutput::ok("iso9660/JUANITOS  N/A  cd0\n"));
        assert_eq!(boot_disk(&cd).as_deref(), Some("cd0"));
        // An installed system on a plain partition; no labels at all.
        let disk =
            MockRunner::new().respond("mount", &[], CmdOutput::ok("/dev/ada0p2 / ufs rw 1 1\n"));
        assert_eq!(boot_disk(&disk).as_deref(), Some("ada0"));
        let unlabelled = MockRunner::new()
            .respond(
                "mount",
                &[],
                CmdOutput::ok("/dev/iso9660/X / cd9660 ro 1 1\n"),
            )
            .respond("glabel", &[], CmdOutput::failed(1, ""));
        assert_eq!(boot_disk(&unlabelled), None);
        assert_eq!(
            boot_disk(&MockRunner::new().respond("mount", &[], CmdOutput::failed(1, ""))),
            None
        );
        assert_eq!(parse_root_device("tmpfs /tmp tmpfs rw 0 0\n"), None);
        assert_eq!(parse_glabel_provider("broken line\n", "broken"), None);
    }

    #[test]
    fn whole_disk_names() {
        assert_eq!(disk_of("da0"), "da0");
        assert_eq!(disk_of("da0p2"), "da0");
        assert_eq!(disk_of("nda10p13"), "nda10");
        assert_eq!(disk_of("ada0s1a"), "ada0");
        assert_eq!(disk_of("ada0s1"), "ada0");
        assert_eq!(disk_of("cd0"), "cd0");
        assert_eq!(disk_of("vtbd1"), "vtbd1");
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
