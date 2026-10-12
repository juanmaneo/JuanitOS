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
pub fn parse_kern_disks(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter(|d| !d.starts_with("cd") && !d.starts_with("md"))
        .map(String::from)
        .collect()
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

pub fn list_disks(runner: &dyn Runner) -> Result<Vec<Disk>> {
    let out = runner.run("sysctl", &["-n", "kern.disks"])?;
    if !out.success() {
        return Ok(Vec::new());
    }
    let mut disks = Vec::new();
    for name in parse_kern_disks(&out.stdout) {
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
            parse_kern_disks("vtbd0 cd0 ada0 md0 nda0\n"),
            vec!["vtbd0", "ada0", "nda0"]
        );
        assert!(parse_kern_disks("").is_empty());
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
