//! Per-qube supervisor: keeps a guest booted across reboots.
//!
//! `jos start <name>` spawns `jos supervise <name>` in the background; the
//! supervisor owns the qube's taps and runs bhyve in a loop:
//!
//! * exit 0 (guest reset)        → boot again
//! * exit 1 / 2 (poweroff/halt)  → finish cleanly
//! * exit 3 (triple fault)       → finish, reported as such
//! * anything else               → finish with an error
//!
//! Its pid is kept in `<run_dir>/<name>.pid` so that `jos kill` can stop
//! the supervisor as well as the VM. Output goes to stdout/stderr; the
//! launcher redirects both into `<run_dir>/<name>.log`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::bhyve::{build_destroy_command, build_poweroff_command, GuestExit, Vm};
use crate::config::{GlobalConfig, Qube};
use crate::error::{Error, Result};
use crate::fsops::FsOps;
use crate::net::Net;
use crate::sys::Runner;

pub fn pidfile(cfg: &GlobalConfig, name: &str) -> PathBuf {
    Path::new(&cfg.run_dir).join(format!("{name}.pid"))
}

pub fn logfile(cfg: &GlobalConfig, name: &str) -> PathBuf {
    Path::new(&cfg.run_dir).join(format!("{name}.log"))
}

/// Parse the contents of a pidfile.
pub fn parse_pid(text: &str) -> Option<u32> {
    text.trim().parse().ok()
}

/// The supervisor pid recorded for `name`, if a pidfile exists.
pub fn read_pid(cfg: &GlobalConfig, name: &str) -> Option<u32> {
    std::fs::read_to_string(pidfile(cfg, name))
        .ok()
        .and_then(|t| parse_pid(&t))
}

/// Whether the process `pid` is alive, asked via `kill -0` so it goes
/// through the [`Runner`] like everything else.
pub fn process_alive(runner: &dyn Runner, pid: u32) -> bool {
    runner
        .run("kill", &["-0", &pid.to_string()])
        .map(|o| o.success())
        .unwrap_or(false)
}

/// Outcome of a supervisor run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// How the guest finally exited.
    pub exit: GuestExit,
    /// Number of guest boots performed (at least one).
    pub boots: u32,
}

/// Called before every guest boot (e.g. to regenerate a firewall's config
/// disk so rule changes apply on the next reboot).
pub type PreBoot<'a> = Box<dyn Fn(&Qube) -> Result<()> + 'a>;

pub struct Supervisor<'a> {
    pub runner: &'a dyn Runner,
    pub cfg: &'a GlobalConfig,
    pub fs: FsOps,
    /// Stop after this many guest-initiated reboots (`None` = unlimited).
    pub max_restarts: Option<u32>,
    /// Pause between a reset and the next boot.
    pub restart_delay: Duration,
    pub pre_boot: Option<PreBoot<'a>>,
}

impl<'a> Supervisor<'a> {
    pub fn new(runner: &'a dyn Runner, cfg: &'a GlobalConfig, dry_run: bool) -> Self {
        Supervisor {
            runner,
            cfg,
            fs: FsOps::new(dry_run),
            // A dry run cannot observe a real guest exit, so boot only once.
            max_restarts: if dry_run { Some(0) } else { None },
            restart_delay: Duration::from_secs(1),
            pre_boot: None,
        }
    }

    fn log(&self, name: &str, msg: &str) {
        println!("[jos supervise {name}] {msg}");
    }

    /// Run the boot loop for `q` in the foreground. Returns once the guest
    /// has powered off or halted (or the restart budget is spent).
    pub fn run(&self, q: &Qube) -> Result<Report> {
        if q.is_template() {
            return Err(Error::Other(format!(
                "{} is a template; clone it into an app qube instead",
                q.name
            )));
        }
        let pid = pidfile(self.cfg, &q.name);
        self.fs
            .mkdir_p(Path::new(&self.cfg.run_dir))
            .and_then(|()| self.fs.write(&pid, &format!("{}\n", std::process::id())))?;

        let result = self.boot_loop(q);

        let _ = Net::new(self.runner).destroy_taps(&q.name);
        let _ = self.fs.remove_file(&pid);
        match &result {
            Ok(r) => self.log(
                &q.name,
                &format!("finished after {} boot(s): {}", r.boots, r.exit),
            ),
            Err(e) => self.log(&q.name, &format!("failed: {e}")),
        }
        result
    }

    fn boot_loop(&self, q: &Qube) -> Result<Report> {
        let vm = Vm::new(self.runner, self.cfg);
        let mut boots = 0u32;
        loop {
            boots += 1;
            self.log(&q.name, &format!("boot #{boots}"));
            if let Some(hook) = &self.pre_boot {
                hook(q)?;
            }
            let exit = vm.run_once(q)?;
            self.log(&q.name, &format!("guest exited: {exit}"));
            if !exit.wants_restart() {
                return Ok(Report { exit, boots });
            }
            if self.max_restarts.is_some_and(|max| boots > max) {
                self.log(&q.name, "restart budget exhausted; not rebooting");
                return Ok(Report { exit, boots });
            }
            std::thread::sleep(self.restart_delay);
        }
    }

    /// Hard stop of a supervised qube: poweroff and destroy the VM (so a
    /// running supervisor sees a clean exit), terminate a still-living
    /// supervisor, then drop the taps and the pidfile.
    pub fn kill(&self, name: &str) -> Result<()> {
        let _ = build_poweroff_command(name).run(self.runner);
        let _ = build_destroy_command(name).run(self.runner);
        if let Some(pid) = read_pid(self.cfg, name) {
            if pid != std::process::id() && process_alive(self.runner, pid) {
                let _ = self.runner.run("kill", &["-TERM", &pid.to_string()]);
            }
        }
        Net::new(self.runner)
            .destroy_taps(name)
            .and_then(|()| self.fs.remove_file(&pidfile(self.cfg, name)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{OsType, QubeKind};
    use crate::sys::{CmdOutput, MockRunner};

    fn test_cfg(dir: &Path) -> GlobalConfig {
        GlobalConfig {
            run_dir: dir.to_string_lossy().into_owned(),
            ..GlobalConfig::default()
        }
    }

    fn tmp_run_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jos-supervisor-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn quiet<'a>(dry_run: bool, runner: &'a MockRunner, cfg: &'a GlobalConfig) -> Supervisor<'a> {
        let mut s = Supervisor::new(runner, cfg, dry_run);
        s.restart_delay = Duration::ZERO;
        s
    }

    #[test]
    fn paths_and_pid_parsing() {
        let cfg = GlobalConfig::default();
        assert_eq!(
            pidfile(&cfg, "work"),
            PathBuf::from("/var/run/juanitos").join("work.pid")
        );
        assert_eq!(
            logfile(&cfg, "work"),
            PathBuf::from("/var/run/juanitos").join("work.log")
        );
        assert_eq!(parse_pid("1234\n"), Some(1234));
        assert_eq!(parse_pid("  42 "), Some(42));
        assert_eq!(parse_pid("nope"), None);
        assert_eq!(parse_pid(""), None);
    }

    #[test]
    fn poweroff_ends_loop_after_one_boot() {
        let dir = tmp_run_dir("poweroff");
        let cfg = test_cfg(&dir);
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(1, ""));
        let mut q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        q.netvm = None;
        let sup = quiet(false, &m, &cfg);
        let r = sup.run(&q).unwrap();
        assert_eq!(r.exit, GuestExit::Poweroff);
        assert_eq!(r.boots, 1);
        let calls = m.calls();
        assert_eq!(calls.iter().filter(|c| c.starts_with("bhyve ")).count(), 1);
        assert!(calls.contains(&"bhyvectl --destroy --vm=work".to_string()));
        assert!(calls.contains(&"ifconfig tap-work destroy".to_string()));
        assert!(!pidfile(&cfg, "work").exists(), "pidfile cleaned up");
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn reboot_restarts_until_budget_spent() {
        let dir = tmp_run_dir("reboot");
        let cfg = test_cfg(&dir);
        // Exit 0 = guest reset, forever; the budget bounds the loop.
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::ok(""));
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let mut sup = quiet(false, &m, &cfg);
        sup.max_restarts = Some(2);
        let r = sup.run(&q).unwrap();
        assert_eq!(r.exit, GuestExit::Reboot);
        assert_eq!(r.boots, 3);
        let calls = m.calls();
        assert_eq!(calls.iter().filter(|c| c.starts_with("bhyve ")).count(), 3);
        assert_eq!(
            calls
                .iter()
                .filter(|c| *c == "bhyvectl --destroy --vm=work")
                .count(),
            3
        );
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn error_exit_propagates_and_cleans_up() {
        let dir = tmp_run_dir("error");
        let cfg = test_cfg(&dir);
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::failed(4, "boom"));
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let err = quiet(false, &m, &cfg).run(&q).unwrap_err();
        assert!(
            err.to_string().contains("(exit code Some(4)): boom"),
            "{err}"
        );
        assert!(!pidfile(&cfg, "work").exists());
        assert!(m.calls().contains(&"ifconfig tap-work destroy".to_string()));
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn no_run_directory_no_boot() {
        let dir = tmp_run_dir("blocked");
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        let cfg = test_cfg(&file.join("run"));
        let m = MockRunner::new();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        assert!(quiet(false, &m, &cfg).run(&q).is_err());
        assert_eq!(m.calls(), Vec::<String>::new(), "nothing run");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dry_run_boots_once_and_touches_no_files() {
        let cfg = test_cfg(Path::new("/nonexistent/juanitos-run"));
        // The mock answers 0 (reset) but a dry run must not loop.
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::ok(""));
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let r = quiet(true, &m, &cfg).run(&q).unwrap();
        assert_eq!(r.boots, 1);
        assert_eq!(r.exit, GuestExit::Reboot);
    }

    #[test]
    fn pre_boot_hook_runs_before_each_boot() {
        use std::cell::Cell;
        let dir = tmp_run_dir("hook");
        let cfg = test_cfg(&dir);
        let m = MockRunner::new().respond("bhyve", &[], CmdOutput::ok(""));
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let count = Cell::new(0u32);
        let mut sup = quiet(false, &m, &cfg);
        sup.max_restarts = Some(1);
        sup.pre_boot = Some(Box::new(|_q| {
            count.set(count.get() + 1);
            Ok(())
        }));
        let r = sup.run(&q).unwrap();
        assert_eq!(r.boots, 2);
        drop(sup);
        assert_eq!(count.get(), 2);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn pre_boot_failure_aborts() {
        let dir = tmp_run_dir("hook-fail");
        let cfg = test_cfg(&dir);
        let m = MockRunner::new();
        let q = Qube::new("work", QubeKind::App, OsType::Freebsd);
        let mut sup = quiet(false, &m, &cfg);
        sup.pre_boot = Some(Box::new(|_q| Err(Error::Other("no config".into()))));
        assert!(sup.run(&q).is_err());
        assert!(!m.calls().iter().any(|c| c.starts_with("bhyve ")));
        assert!(!pidfile(&cfg, "work").exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn refuses_templates() {
        let cfg = GlobalConfig::default();
        let m = MockRunner::new();
        let t = Qube::new("tmpl", QubeKind::Template, OsType::Freebsd);
        assert!(quiet(true, &m, &cfg).run(&t).is_err());
        assert!(m.calls().is_empty());
    }

    #[test]
    fn kill_terminates_live_supervisor() {
        let dir = tmp_run_dir("kill");
        let cfg = test_cfg(&dir);
        std::fs::write(pidfile(&cfg, "work"), "4242\n").unwrap();
        let m = MockRunner::new().respond("kill", &["-0", "4242"], CmdOutput::ok(""));
        quiet(false, &m, &cfg).kill("work").unwrap();
        let calls = m.calls();
        assert_eq!(calls[0], "bhyvectl --force-poweroff --vm=work");
        assert_eq!(calls[1], "bhyvectl --destroy --vm=work");
        assert!(calls.contains(&"kill -TERM 4242".to_string()));
        assert!(calls.contains(&"ifconfig tap-work destroy".to_string()));
        assert!(!pidfile(&cfg, "work").exists());
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn kill_skips_dead_supervisor() {
        let dir = tmp_run_dir("kill-dead");
        let cfg = test_cfg(&dir);
        std::fs::write(pidfile(&cfg, "work"), "4242\n").unwrap();
        let m = MockRunner::new().respond("kill", &["-0", "4242"], CmdOutput::failed(1, ""));
        quiet(false, &m, &cfg).kill("work").unwrap();
        assert!(!m.calls().contains(&"kill -TERM 4242".to_string()));
        assert!(!pidfile(&cfg, "work").exists());
        let _ = std::fs::remove_dir(&dir);
    }
}
