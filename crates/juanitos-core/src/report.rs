//! Hardware report (`jos doctor --report FILE`).
//!
//! One text file holding the doctor checks and the raw facts needed to
//! diagnose a machine remotely: CPU and IOMMU, PCI devices, loaded
//! modules, loader variables, disks, the network interfaces and the boot
//! messages. Every fact is gathered independently; a tool that is missing
//! or fails is written into the report instead of aborting it, because the
//! machines that need a report most are the ones where things fail.

use crate::doctor::{Check, DMESG_BOOT};
use crate::dom0::RELEASE_INFO_PATH;
use crate::setup::LOADER_CONF;
use crate::sys::{shell_join, Runner};

/// Read-only commands whose output goes into the report, in order.
pub const COMMANDS: &[(&str, &[&str])] = &[
    ("uname", &["-a"]),
    ("freebsd-version", &["-kru"]),
    (
        "sysctl",
        &[
            "hw.model",
            "hw.ncpu",
            "hw.physmem",
            "hw.machine",
            "machdep.bootmethod",
        ],
    ),
    ("sysctl", &["hw.vmm"]),
    ("sysctl", &["hw.dri"]),
    ("kldstat", &[]),
    ("pciconf", &["-lv"]),
    ("kenv", &[]),
    ("sysctl", &["kern.disks"]),
    ("gpart", &["show"]),
    ("mount", &["-p"]),
    ("ifconfig", &["-a"]),
];

/// Files copied into the report as they are.
pub const FILES: &[&str] = &[RELEASE_INFO_PATH, LOADER_CONF, DMESG_BOOT];

/// Boot-message words that point at a problem (matched case-insensitively).
const PROBLEM_WORDS: &[&str] = &[
    "error",
    "fail",
    "mountroot",
    "panic",
    "timed out",
    "timeout",
    "unable",
    "cannot",
    "can't",
];

/// Boot-message words about what JuanitOS depends on: virtualisation, the
/// IOMMU, the framebuffer console and the GPU drivers.
const PLATFORM_WORDS: &[&str] = &[
    "vt-x:", "svm:", "dmar", "amd-vi", "ivhd", "vt(", "efifb", "vgapci", "drm", "i915", "nvidia",
];

/// Shown instead of identifiers that tie a report to one machine.
const REMOVED: &str = "(removed)";

/// Lines of `text` containing any of `words` (case-insensitive).
fn lines_with<'a>(text: &'a str, words: &[&str]) -> Vec<&'a str> {
    text.lines()
        .filter(|l| {
            let l = l.to_lowercase();
            words.iter().any(|w| l.contains(w))
        })
        .collect()
}

/// Boot messages that suggest something went wrong.
pub fn boot_problems(dmesg: &str) -> Vec<&str> {
    lines_with(dmesg, PROBLEM_WORDS)
}

/// Boot messages about virtualisation, the IOMMU and graphics.
pub fn platform_lines(dmesg: &str) -> Vec<&str> {
    lines_with(dmesg, PLATFORM_WORDS)
}

/// Drop serial numbers, UUIDs and asset tags: `kenv` lists the SMBIOS
/// ones (`smbios.system.serial="..."`) and disk drivers print `Serial
/// Number ...` in the boot messages. A report is meant to be shared.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    out
}

/// Parts of a `kenv` key that name a machine identifier.
const ID_KEYS: &[&str] = &["serial", "uuid", "asset", ".tag"];

fn redact_line(line: &str) -> String {
    if let Some((key, _)) = line.split_once('=') {
        let k = key.to_ascii_lowercase();
        if !k.contains(' ') && ID_KEYS.iter().any(|w| k.contains(w)) {
            return format!("{key}=\"{REMOVED}\"");
        }
    }
    // ASCII lowercasing keeps every byte offset valid in `line`.
    const SERIAL: &str = "serial number";
    match line.to_ascii_lowercase().find(SERIAL) {
        Some(at) => format!("{} {REMOVED}", line.get(..at + SERIAL.len()).unwrap_or("")),
        None => line.to_string(),
    }
}

fn heading(out: &mut String, title: &str) {
    out.push_str("\n==== ");
    out.push_str(title);
    out.push_str(" ====\n");
}

fn push_lines(out: &mut String, lines: &[&str]) {
    if lines.is_empty() {
        out.push_str("(none)\n");
    }
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
}

/// What a command printed, or why there is nothing.
fn command_output(runner: &dyn Runner, program: &str, args: &[&str]) -> String {
    match runner.run(program, args) {
        Ok(out) if out.success() => out.stdout,
        Ok(out) => {
            let mut s = format!("(exit code {:?})\n", out.status);
            s.push_str(&out.stdout);
            s.push_str(&out.stderr);
            s
        }
        Err(e) => format!("(not run: {e})\n"),
    }
}

/// Build the report. `read` returns a file's contents, `None` when it
/// cannot be read (the CLI passes `std::fs::read_to_string`).
pub fn build(
    runner: &dyn Runner,
    checks: &[Check],
    read: &dyn Fn(&str) -> Option<String>,
) -> String {
    let mut out = format!(
        "JuanitOS hardware report (jos {})\n",
        env!("CARGO_PKG_VERSION")
    );

    heading(&mut out, "doctor");
    for c in checks {
        out.push_str(&c.to_string());
        out.push('\n');
    }

    let dmesg = read(DMESG_BOOT);
    let dmesg = dmesg.as_deref().unwrap_or("");
    heading(&mut out, &format!("possible problems in {DMESG_BOOT}"));
    push_lines(&mut out, &boot_problems(dmesg));
    heading(&mut out, &format!("platform lines in {DMESG_BOOT}"));
    push_lines(&mut out, &platform_lines(dmesg));

    for (program, args) in COMMANDS {
        heading(&mut out, &shell_join(program, args));
        out.push_str(&command_output(runner, program, args));
    }

    for path in FILES {
        heading(&mut out, path);
        match read(path) {
            Some(text) => out.push_str(&text),
            None => out.push_str("(could not be read)\n"),
        }
    }

    redact(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;
    use crate::error::{Error, Result};
    use crate::sys::{CmdOutput, MockRunner};

    const DMESG: &str = "\
CPU: Intel(R) Core(TM) Ultra 7 165H (2995.20-MHz K8-class CPU)
  VT-x: PAT,HLT,MTF,PAUSE,EPT,UG,VPID
DMAR: Found table at 0x7a3d0000
VT(efifb): resolution 2560x1600
nda0: Serial Number S6XNNS0W123456
Mounting from cd9660:/dev/iso9660/JUANITOS failed with error 19.
mountroot>
";

    fn checks() -> Vec<Check> {
        vec![
            Check {
                name: "iommu".into(),
                status: Status::Ok,
                detail: "hw.vmm.iommu.enable=1".into(),
            },
            Check {
                name: "zpool".into(),
                status: Status::Fail,
                detail: "pool zroot not found".into(),
            },
        ]
    }

    fn files(path: &str) -> Option<String> {
        match path {
            DMESG_BOOT => Some(DMESG.into()),
            LOADER_CONF => Some("vmm_load=\"YES\"\n".into()),
            _ => None,
        }
    }

    #[test]
    fn highlights_problems_and_platform_lines() {
        assert_eq!(
            boot_problems(DMESG),
            vec![
                "Mounting from cd9660:/dev/iso9660/JUANITOS failed with error 19.",
                "mountroot>",
            ]
        );
        let platform = platform_lines(DMESG);
        assert!(platform.iter().any(|l| l.contains("VT-x: PAT")));
        assert!(platform.iter().any(|l| l.starts_with("DMAR")));
        assert!(platform.iter().any(|l| l.starts_with("VT(efifb)")));
        assert!(!platform.iter().any(|l| l.contains("mountroot")));
        assert!(boot_problems("").is_empty());
    }

    #[test]
    fn identifiers_are_removed() {
        let text = redact(
            "smbios.system.serial=\"PF3ABCDE\"\n\
             smbios.system.uuid=\"4c4c4544-0000\"\n\
             smbios.chassis.tag=\"ASSET-1\"\n\
             smbios.system.product=\"21FV\"\n\
             nda0: Serial Number S6XNNS0W123456\n\
             ugen0.2: <Kingstön> SERIAL NUMBER 0123\n\
             vfs.root.mountfrom=\"zfs:zroot/ROOT/default\"\n\
             stage = 2\n",
        );
        assert!(!text.contains("PF3ABCDE"), "{text}");
        assert!(!text.contains("4c4c4544"), "{text}");
        assert!(!text.contains("ASSET-1"), "{text}");
        assert!(!text.contains("S6XNNS0W"), "{text}");
        assert!(
            text.contains("smbios.system.serial=\"(removed)\""),
            "{text}"
        );
        assert!(text.contains("nda0: Serial Number (removed)"), "{text}");
        assert!(text.contains("smbios.system.product=\"21FV\""), "{text}");
        assert!(text.contains("zfs:zroot/ROOT/default"), "{text}");
        assert!(
            text.contains("<Kingstön> SERIAL NUMBER (removed)"),
            "{text}"
        );
        assert!(text.contains("stage = 2"), "{text}");
    }

    #[test]
    fn report_has_every_section_and_records_failures() {
        let m = MockRunner::new()
            .respond(
                "uname",
                &["-a"],
                CmdOutput::ok("FreeBSD lab 15.1-RELEASE amd64\n"),
            )
            .respond(
                "kenv",
                &[],
                CmdOutput::ok("hw.vmm.iommu.enable=\"1\"\nsmbios.system.serial=\"PF3ABCDE\"\n"),
            )
            .respond(
                "gpart",
                &["show"],
                CmdOutput::failed(1, "gpart: No such geom\n"),
            );
        let r = build(&m, &checks(), &files);

        assert!(r.starts_with("JuanitOS hardware report (jos "), "{r}");
        assert!(r.contains("[FAIL] zpool"), "{r}");
        assert!(
            r.contains("==== uname -a ====\nFreeBSD lab 15.1-RELEASE"),
            "{r}"
        );
        assert!(r.contains("==== gpart show ====\n(exit code Some(1))\ngpart: No such geom"));
        assert!(r.contains("hw.vmm.iommu.enable=\"1\""), "{r}");
        assert!(!r.contains("PF3ABCDE"), "{r}");
        assert!(r.contains("failed with error 19"), "{r}");
        assert!(!r.contains("S6XNNS0W"), "{r}");
        assert!(
            r.contains("==== /boot/loader.conf ====\nvmm_load=\"YES\""),
            "{r}"
        );
        assert!(
            r.contains(&format!(
                "==== {RELEASE_INFO_PATH} ====\n(could not be read)"
            )),
            "{r}"
        );
        for (program, args) in COMMANDS {
            assert!(r.contains(&format!("==== {} ====", shell_join(program, args))));
        }
        assert_eq!(m.calls().len(), COMMANDS.len());
    }

    /// A host where nothing can be run and nothing read.
    struct NoTools;

    impl Runner for NoTools {
        fn run(&self, program: &str, _: &[&str]) -> Result<CmdOutput> {
            Err(Error::Other(format!("running {program}: not found")))
        }
        fn run_with_input(&self, program: &str, args: &[&str], _: &str) -> Result<CmdOutput> {
            self.run(program, args)
        }
    }

    #[test]
    fn a_bare_host_still_gets_a_report() {
        assert!(NoTools.run_with_input("tee", &[], "").is_err());
        let r = build(&NoTools, &[], &|_| None);
        assert!(r.contains("==== uname -a ====\n(not run: running uname: not found)"));
        assert!(r.contains(&format!(
            "==== possible problems in {DMESG_BOOT} ====\n(none)"
        )));
        assert!(r.contains(&format!("==== {DMESG_BOOT} ====\n(could not be read)")));
    }
}
