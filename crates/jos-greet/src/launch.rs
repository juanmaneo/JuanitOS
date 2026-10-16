//! What starting a desktop session means, as data: the account it runs
//! as, its environment, its command line. The FreeBSD backend executes
//! this plan (fork, `setusercontext(3)`, exec); building it is pure and
//! unit-tested.

use std::path::PathBuf;

use crate::sessions::Session;

/// The wrapper every session goes through (`freebsd/jos-session`): last
/// fixes to the environment, then `exec "$@"`.
pub const SESSION_WRAPPER: &str = "/usr/local/libexec/jos-session";

/// Parent of the per-user `XDG_RUNTIME_DIR`s (made by `rc.d/jos-login`).
pub const RUNTIME_ROOT: &str = "/var/run/user";

/// The PAM service (`/etc/pam.d/jos-greet`).
pub const PAM_SERVICE: &str = "jos-greet";

/// The fields of a passwd(5) entry the greeter needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

impl Account {
    /// Refuse accounts that must not get a desktop from the login screen.
    pub fn check(&self) -> Result<(), String> {
        if self.uid == 0 {
            return Err("root cannot log in here: use your own account".to_string());
        }
        if self.shell.ends_with("nologin") {
            return Err(format!("{} is not allowed to log in", self.name));
        }
        Ok(())
    }

    pub fn runtime_dir(&self) -> PathBuf {
        PathBuf::from(format!("{RUNTIME_ROOT}/{}", self.uid))
    }

    /// Where the session's stdout/stderr go (`jos-greet` shows it in the
    /// error message when the desktop exits with a failure).
    pub fn session_log(&self) -> PathBuf {
        self.runtime_dir().join("jos-session.log")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub program: String,
    pub args: Vec<String>,
    /// The whole environment (the child starts from an empty one).
    pub env: Vec<(String, String)>,
    pub cwd: String,
}

/// The session for `account`, on virtual terminal `vt` (1-based), with
/// the variables PAM modules exported (`NAME=value`) layered underneath
/// ours.
pub fn plan(account: &Account, session: &Session, vt: i32, pam_env: &[String]) -> Plan {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut set = |k: &str, v: String| {
        env.retain(|(key, _)| key != k);
        env.push((k.to_string(), v));
    };
    for kv in pam_env {
        if let Some((k, v)) = kv.split_once('=') {
            if !k.is_empty() {
                set(k, v.to_string());
            }
        }
    }
    set("HOME", account.home.clone());
    set("USER", account.name.clone());
    set("LOGNAME", account.name.clone());
    set("SHELL", account.shell.clone());
    set(
        "PATH",
        format!(
            "/sbin:/bin:/usr/sbin:/usr/bin:/usr/local/sbin:/usr/local/bin:{}/bin",
            account.home
        ),
    );
    set(
        "XDG_RUNTIME_DIR",
        account.runtime_dir().to_string_lossy().into_owned(),
    );
    set("XDG_SESSION_TYPE", "wayland".to_string());
    set("XDG_SESSION_CLASS", "user".to_string());
    set("XDG_CURRENT_DESKTOP", session.desktop.clone());
    set("XDG_SESSION_DESKTOP", session.desktop.to_lowercase());
    set("XDG_SEAT", "seat0".to_string());
    set("XDG_VTNR", vt.to_string());
    let cwd = if account.home.is_empty() {
        "/".to_string()
    } else {
        account.home.clone()
    };
    Plan {
        program: SESSION_WRAPPER.to_string(),
        args: session.exec.clone(),
        env,
        cwd,
    }
}

/// The message shown when a session ends badly.
pub fn exit_message(session: &str, code: Option<i32>, log: &str) -> Result<(), String> {
    match code {
        Some(0) => Ok(()),
        Some(c) => Err(format!("{session} exited with status {c} (log: {log})")),
        None => Err(format!("{session} was killed (log: {log})")),
    }
}

/// `ttyv8` for VT 9: what PAM_TTY and pam_securetty expect.
pub fn tty_name(vt: i32) -> String {
    format!("ttyv{}", vt - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn juan() -> Account {
        Account {
            name: "juan".into(),
            uid: 1001,
            gid: 1001,
            home: "/home/juan".into(),
            shell: "/bin/sh".into(),
        }
    }

    fn get<'a>(p: &'a Plan, k: &str) -> Option<&'a str> {
        p.env
            .iter()
            .find(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn plan_runs_the_wrapper_with_the_session() {
        let p = plan(
            &juan(),
            &Session::cosmic_default(),
            9,
            &[
                "LANG=C.UTF-8".into(),
                "HOME=/evil".into(),
                "junk".into(),
                "=x".into(),
            ],
        );
        assert_eq!(p.program, SESSION_WRAPPER);
        assert_eq!(p.args, vec!["/usr/local/bin/start-cosmic"]);
        assert_eq!(p.cwd, "/home/juan");
        assert_eq!(get(&p, "LANG"), Some("C.UTF-8"));
        assert_eq!(get(&p, "HOME"), Some("/home/juan"), "ours win over PAM's");
        assert_eq!(get(&p, "USER"), Some("juan"));
        assert_eq!(get(&p, "LOGNAME"), Some("juan"));
        assert_eq!(get(&p, "SHELL"), Some("/bin/sh"));
        assert!(get(&p, "PATH").unwrap().ends_with(":/home/juan/bin"));
        assert_eq!(get(&p, "XDG_RUNTIME_DIR"), Some("/var/run/user/1001"));
        assert_eq!(get(&p, "XDG_SESSION_TYPE"), Some("wayland"));
        assert_eq!(get(&p, "XDG_CURRENT_DESKTOP"), Some("COSMIC"));
        assert_eq!(get(&p, "XDG_SESSION_DESKTOP"), Some("cosmic"));
        assert_eq!(get(&p, "XDG_VTNR"), Some("9"));
        assert_eq!(get(&p, "XDG_SEAT"), Some("seat0"));
        assert_eq!(p.env.iter().filter(|(k, _)| k == "HOME").count(), 1);
        assert!(p.env.iter().all(|(k, _)| !k.is_empty()));
        let homeless = Account {
            home: String::new(),
            ..juan()
        };
        assert_eq!(plan(&homeless, &Session::cosmic_default(), 9, &[]).cwd, "/");
    }

    #[test]
    fn accounts_are_checked() {
        assert!(juan().check().is_ok());
        let root = Account { uid: 0, ..juan() };
        assert!(root.check().unwrap_err().contains("root"));
        let svc = Account {
            shell: "/usr/sbin/nologin".into(),
            ..juan()
        };
        assert!(svc.check().unwrap_err().contains("not allowed"));
        assert_eq!(
            juan().session_log(),
            PathBuf::from("/var/run/user/1001/jos-session.log")
        );
    }

    #[test]
    fn exit_messages_and_tty() {
        assert_eq!(exit_message("COSMIC", Some(0), "l"), Ok(()));
        assert_eq!(
            exit_message("COSMIC", Some(1), "/x.log"),
            Err("COSMIC exited with status 1 (log: /x.log)".into())
        );
        assert!(exit_message("COSMIC", None, "l")
            .unwrap_err()
            .contains("killed"));
        assert_eq!(tty_name(9), "ttyv8");
    }
}
