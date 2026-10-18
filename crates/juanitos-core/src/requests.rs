//! Guest-initiated requests with a dom0 policy: the `qrexec` idea of
//! QubesOS, over the per-qube share ([`crate::share`]).
//!
//! A qube cannot reach another qube; it can only *ask dom0*. The one
//! service so far is `copy`: the guest exports a file to its outbox and
//! drops a request file in `/mnt/jos/req/` (`templates/agent/jos-copy`):
//!
//! ```text
//! copy <basename> <destination qube>
//! ```
//!
//! `jos serve` on dom0 polls every qube's request directory and decides
//! with the **policy** (`<config_dir>/policy`, one rule per line, first
//! match wins, `*` is a wildcard):
//!
//! ```text
//! # service  source  destination  verdict
//! copy       work    personal     allow
//! copy       *       vault        deny
//! copy       *       *            ask
//! ```
//!
//! `allow` moves the file into the destination's inbox and answers
//! `ok /mnt/jos/in/<src>/<basename>`; `deny` answers `denied: policy`
//! (`denied: operator` when settled by `jos requests deny`);
//! `ask` (the default) leaves the request pending for an operator to
//! settle with `jos requests approve|deny`. The answer is the file
//! `<id>.reply` next to the request. dom0 removes the request when it
//! answers, the outbox entry when it refuses, and the reply a minute
//! later (a guest user cannot unlink on the share, see [`crate::share`]).
//!
//! Requests are files, so the whole exchange is inspectable with `ls`,
//! survives a restart of the daemon, and needs no agent protocol beyond
//! the agent already mounting the share.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::config::{validate_name, QubeStore};
use crate::error::{Error, Result};
use crate::share::{self, validate_basename, Delivery, Share};
use crate::sys::Runner;

/// Name of the policy file under the configuration directory.
pub const POLICY_FILE: &str = "policy";
/// Suffix of the reply dom0 writes next to a request.
pub const REPLY_SUFFIX: &str = ".reply";
/// Suffix of a request still being written by the guest (`jos-copy`
/// writes `<id>.tmp` and renames it): not a request yet.
pub const TMP_SUFFIX: &str = ".tmp";
/// How long a reply stays before dom0 removes it (the guest cannot).
pub const REPLY_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// The only service so far.
pub const SERVICE_COPY: &str = "copy";

/// What the policy says about a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
    /// Leave it to the operator (`jos requests`).
    Ask,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Verdict::Allow => "allow",
            Verdict::Deny => "deny",
            Verdict::Ask => "ask",
        })
    }
}

impl FromStr for Verdict {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "allow" => Ok(Verdict::Allow),
            "deny" => Ok(Verdict::Deny),
            "ask" => Ok(Verdict::Ask),
            _ => Err(Error::Other(format!(
                "{s:?}: verdict must be allow, deny or ask"
            ))),
        }
    }
}

/// One policy line: `copy SRC DST VERDICT`, `*` matching any qube.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub src: String,
    pub dst: String,
    pub verdict: Verdict,
}

impl Rule {
    pub fn new(src: &str, dst: &str, verdict: Verdict) -> Result<Rule> {
        validate_pattern(src)?;
        validate_pattern(dst)?;
        Ok(Rule {
            src: src.to_string(),
            dst: dst.to_string(),
            verdict,
        })
    }

    fn matches(&self, src: &str, dst: &str) -> bool {
        (self.src == "*" || self.src == src) && (self.dst == "*" || self.dst == dst)
    }
}

fn validate_pattern(p: &str) -> Result<()> {
    if p == "*" {
        return Ok(());
    }
    validate_name(p)
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{SERVICE_COPY} {} {} {}",
            self.src, self.dst, self.verdict
        )
    }
}

impl FromStr for Rule {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let words: Vec<&str> = s.split_whitespace().collect();
        match words.as_slice() {
            [service, src, dst, verdict] if *service == SERVICE_COPY => {
                Rule::new(src, dst, verdict.parse()?)
            }
            _ => Err(Error::Other(format!(
                "{s:?}: a rule is `{SERVICE_COPY} SRC DST allow|deny|ask`"
            ))),
        }
    }
}

/// The policy: rules in order, first match wins, [`Verdict::Ask`] when
/// nothing matches.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Policy {
    pub rules: Vec<Rule>,
}

impl Policy {
    /// Parse the file format: one rule per line, `#` comments, blank
    /// lines ignored.
    pub fn parse(text: &str) -> Result<Policy> {
        let mut rules = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let rule: Rule = line
                .parse()
                .map_err(|e| Error::Other(format!("policy line {}: {e}", n + 1)))?;
            rules.push(rule);
        }
        Ok(Policy { rules })
    }

    /// The file format of [`Policy::parse`].
    pub fn render(&self) -> String {
        let mut out = String::from("# JuanitOS request policy: SERVICE SRC DST allow|deny|ask\n");
        for r in &self.rules {
            out.push_str(&r.to_string());
            out.push('\n');
        }
        out
    }

    /// Read `path`; a missing file is an empty policy (everything asks).
    pub fn load(path: &Path) -> Result<Policy> {
        match std::fs::read_to_string(path) {
            Ok(text) => Policy::parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Policy::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, self.render())?;
        Ok(())
    }

    pub fn decide(&self, src: &str, dst: &str) -> Verdict {
        self.rules
            .iter()
            .find(|r| r.matches(src, dst))
            .map_or(Verdict::Ask, |r| r.verdict)
    }
}

/// `<config_dir>/policy`.
pub fn policy_path(config_dir: &Path) -> PathBuf {
    config_dir.join(POLICY_FILE)
}

/// A request file found in a qube's `req/` directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The qube that asked.
    pub src: String,
    /// File name chosen by the guest (letters, digits, `.`, `-`, `_`).
    pub id: String,
    /// Entry in the source's outbox.
    pub base: String,
    /// Destination qube.
    pub dst: String,
}

impl Request {
    /// Parse the body of request `id` from qube `src`.
    pub fn parse(src: &str, id: &str, body: &str) -> Result<Request> {
        validate_name(src)?;
        validate_id(id)?;
        let line = body.lines().next().unwrap_or("").trim();
        let words: Vec<&str> = line.split_whitespace().collect();
        match words.as_slice() {
            [service, base, dst] if *service == SERVICE_COPY => {
                validate_basename(base)?;
                validate_name(dst)?;
                Ok(Request {
                    src: src.to_string(),
                    id: id.to_string(),
                    base: base.to_string(),
                    dst: dst.to_string(),
                })
            }
            _ => Err(Error::Other(format!(
                "{line:?}: a request is `{SERVICE_COPY} FILE QUBE`"
            ))),
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {SERVICE_COPY} {} -> {}",
            self.src, self.id, self.base, self.dst
        )
    }
}

/// A request id is a file name the guest chose: keep it to a safe set.
pub fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && !id.ends_with(REPLY_SUFFIX)
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Other(format!("{id:?}: not a valid request id")))
    }
}

/// What dom0 answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Delivered; the path the destination sees.
    Ok(String),
    Denied(String),
    Error(String),
}

impl fmt::Display for Reply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reply::Ok(path) => write!(f, "ok {path}"),
            Reply::Denied(why) => write!(f, "denied: {why}"),
            Reply::Error(why) => write!(f, "error: {why}"),
        }
    }
}

/// A request file as found on disk, before it is understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub src: String,
    pub id: String,
    pub request: std::result::Result<Request, String>,
}

/// What became of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    Delivered(Delivery),
    Denied,
    /// The policy says `ask`: left for `jos requests`.
    Pending,
    Failed(String),
}

/// The dom0 side: scans the shares, applies the policy, answers.
pub struct Broker<'a> {
    pub share: Share<'a>,
    pub store: &'a QubeStore,
    pub policy: Policy,
}

impl<'a> Broker<'a> {
    pub fn new(
        runner: &'a dyn Runner,
        cfg: &'a crate::GlobalConfig,
        store: &'a QubeStore,
        policy: Policy,
    ) -> Self {
        Broker {
            share: Share::new(runner, cfg),
            store,
            policy,
        }
    }

    /// Every request file under `<share_dir>/*/req/`, sorted by qube then
    /// id. Reads the file system directly (it is dom0's own state).
    pub fn scan(&self) -> Result<Vec<Found>> {
        let root = Path::new(&self.share.cfg.share_dir);
        let mut found = Vec::new();
        let qubes = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(e) => return Err(e.into()),
        };
        for qube in qubes {
            let qube = qube?;
            let src = qube.file_name().to_string_lossy().into_owned();
            if validate_name(&src).is_err() {
                continue;
            }
            let req_dir = qube.path().join(share::REQUESTS);
            let entries = match std::fs::read_dir(&req_dir) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                let entry = entry?;
                let id = entry.file_name().to_string_lossy().into_owned();
                if id.starts_with('.')
                    || id.ends_with(REPLY_SUFFIX)
                    || id.ends_with(TMP_SUFFIX)
                    || !entry.path().is_file()
                {
                    continue;
                }
                let request = match validate_id(&id) {
                    Err(e) => Err(e.to_string()),
                    Ok(()) => match std::fs::read_to_string(entry.path()) {
                        Ok(body) => Request::parse(&src, &id, &body).map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    },
                };
                found.push(Found {
                    src: src.clone(),
                    id,
                    request,
                });
            }
        }
        found.sort_by(|a, b| (&a.src, &a.id).cmp(&(&b.src, &b.id)));
        Ok(found)
    }

    /// Settle one found request: the policy decides unless `verdict`
    /// overrides it (an operator's `approve`/`deny`). Everything but
    /// `Pending` answers the guest and removes the request file.
    pub fn settle(&self, found: &Found, verdict: Option<Verdict>) -> Result<Disposition> {
        let request = match &found.request {
            Ok(r) => r,
            Err(why) => {
                self.answer(&found.src, &found.id, &Reply::Error(why.clone()))?;
                return Ok(Disposition::Failed(why.clone()));
            }
        };
        let by = if verdict.is_some() {
            "operator"
        } else {
            "policy"
        };
        let verdict = verdict.unwrap_or_else(|| self.policy.decide(&request.src, &request.dst));
        match verdict {
            Verdict::Ask => Ok(Disposition::Pending),
            Verdict::Deny => {
                self.share.discard_export(&request.src, &request.base)?;
                self.answer(&request.src, &request.id, &Reply::Denied(by.into()))?;
                Ok(Disposition::Denied)
            }
            Verdict::Allow => {
                let delivered = self
                    .store
                    .get(&request.dst)
                    .and_then(|dst| self.share.deliver(&request.src, &request.base, &dst));
                match delivered {
                    Ok(d) => {
                        self.answer(&request.src, &request.id, &Reply::Ok(d.guest_path.clone()))?;
                        Ok(Disposition::Delivered(d))
                    }
                    Err(e) => {
                        let why = e.to_string();
                        self.share.discard_export(&request.src, &request.base)?;
                        self.answer(&request.src, &request.id, &Reply::Error(why.clone()))?;
                        Ok(Disposition::Failed(why))
                    }
                }
            }
        }
    }

    /// One pass over every request, then a sweep of the replies older
    /// than [`REPLY_TTL`]: what `jos serve` does each interval.
    pub fn serve_once(&self) -> Result<Vec<(Found, Disposition)>> {
        let mut done = Vec::new();
        for found in self.scan()? {
            let disposition = self.settle(&found, None)?;
            done.push((found, disposition));
        }
        self.sweep_replies(std::time::SystemTime::now())?;
        Ok(done)
    }

    /// Remove replies the guest has had long enough to read (it polls
    /// every second; it cannot remove them itself).
    pub fn sweep_replies(&self, now: std::time::SystemTime) -> Result<()> {
        let root = Path::new(&self.share.cfg.share_dir);
        let qubes = match std::fs::read_dir(root) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for qube in qubes {
            let qube = qube?;
            let name = qube.file_name().to_string_lossy().into_owned();
            if validate_name(&name).is_err() {
                continue;
            }
            let entries = match std::fs::read_dir(qube.path().join(share::REQUESTS)) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            for entry in entries {
                let entry = entry?;
                let file = entry.file_name().to_string_lossy().into_owned();
                if !file.ends_with(REPLY_SUFFIX) {
                    continue;
                }
                let age = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| now.duration_since(m).ok());
                if age.is_some_and(|a| a >= REPLY_TTL) {
                    let path = format!("{}/{file}", self.share.requests(&name));
                    self.share.runner.run_checked("rm", &["-f", &path])?;
                }
            }
        }
        Ok(())
    }

    /// Find a pending request by qube and id.
    pub fn find(&self, src: &str, id: &str) -> Result<Found> {
        self.scan()?
            .into_iter()
            .find(|f| f.src == src && f.id == id)
            .ok_or_else(|| Error::Other(format!("{src}: no pending request {id}")))
    }

    /// Write `<id>.reply` and remove the request, through the runner so a
    /// dry run shows it.
    fn answer(&self, src: &str, id: &str, reply: &Reply) -> Result<()> {
        let dir = self.share.requests(src);
        let reply_file = format!("{dir}/{id}{REPLY_SUFFIX}");
        let request_file = format!("{dir}/{id}");
        self.share
            .runner
            .run_with_input_checked("tee", &[&reply_file], &format!("{reply}\n"))?;
        self.share
            .runner
            .run_checked("rm", &["-f", &request_file])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GlobalConfig, OsType, Qube, QubeKind};
    use crate::sys::{CmdOutput, MockRunner};

    #[test]
    fn verdicts_and_rules_parse_and_print() {
        assert_eq!("allow".parse::<Verdict>().unwrap(), Verdict::Allow);
        assert_eq!("deny".parse::<Verdict>().unwrap(), Verdict::Deny);
        assert_eq!("ask".parse::<Verdict>().unwrap(), Verdict::Ask);
        assert!("maybe".parse::<Verdict>().is_err());
        assert_eq!(Verdict::Ask.to_string(), "ask");
        let r: Rule = "copy work * allow".parse().unwrap();
        assert_eq!(r, Rule::new("work", "*", Verdict::Allow).unwrap());
        assert_eq!(r.to_string(), "copy work * allow");
        assert!("copy work".parse::<Rule>().is_err());
        assert!("exec work web allow".parse::<Rule>().is_err());
        assert!("copy Work web allow".parse::<Rule>().is_err());
        assert!(Rule::new("*", "../x", Verdict::Deny).is_err());
    }

    #[test]
    fn policy_first_match_wins_and_defaults_to_ask() {
        let p = Policy::parse(
            "# comment\n\ncopy work personal allow # trailing\ncopy * vault deny\ncopy work * deny\n",
        )
        .unwrap();
        assert_eq!(p.rules.len(), 3);
        assert_eq!(p.decide("work", "personal"), Verdict::Allow);
        assert_eq!(p.decide("work", "vault"), Verdict::Deny);
        assert_eq!(p.decide("work", "web"), Verdict::Deny);
        assert_eq!(p.decide("web", "work"), Verdict::Ask);
        assert_eq!(Policy::default().decide("a", "b"), Verdict::Ask);
        let err = Policy::parse("copy a b allow\nnonsense\n").unwrap_err();
        assert!(err.to_string().contains("policy line 2"), "{err}");
        let again = Policy::parse(&p.render()).unwrap();
        assert_eq!(again, p);
    }

    #[test]
    fn policy_file_roundtrip_and_missing_file() {
        let dir = temp_dir("policy");
        let path = policy_path(&dir);
        assert_eq!(Policy::load(&path).unwrap(), Policy::default());
        let p = Policy {
            rules: vec![Rule::new("a", "b", Verdict::Allow).unwrap()],
        };
        p.save(&path).unwrap();
        assert_eq!(Policy::load(&path).unwrap(), p);
        std::fs::write(&path, "garbage\n").unwrap();
        assert!(Policy::load(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn request_parsing_and_ids() {
        let r = Request::parse("work", "42-1", "copy notes.txt web\nignored\n").unwrap();
        assert_eq!(
            r,
            Request {
                src: "work".into(),
                id: "42-1".into(),
                base: "notes.txt".into(),
                dst: "web".into()
            }
        );
        assert_eq!(r.to_string(), "work 42-1: copy notes.txt -> web");
        assert!(Request::parse("work", "1", "copy ../etc web").is_err());
        assert!(Request::parse("work", "1", "copy a Web").is_err());
        assert!(Request::parse("work", "1", "exec ls web").is_err());
        assert!(Request::parse("work", "1", "").is_err());
        assert!(Request::parse("work", "bad/id", "copy a b").is_err());
        assert!(Request::parse("Work", "1", "copy a b").is_err());
        assert!(validate_id("1.reply").is_err());
        assert!(validate_id(".hidden").is_err());
        assert!(validate_id("").is_err());
        assert!(validate_id(&"x".repeat(65)).is_err());
        assert!(validate_id("a.b-c_9").is_ok());
        assert_eq!(Reply::Ok("/p".into()).to_string(), "ok /p");
        assert_eq!(Reply::Denied("policy".into()).to_string(), "denied: policy");
        assert_eq!(Reply::Error("x".into()).to_string(), "error: x");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jos-requests-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Fixture {
        dir: PathBuf,
        cfg: GlobalConfig,
        store: QubeStore,
    }

    fn fixture() -> Fixture {
        let dir = temp_dir("broker");
        let cfg = GlobalConfig {
            share_dir: dir.join("share").to_string_lossy().into_owned(),
            ..GlobalConfig::default()
        };
        let store = QubeStore::new(dir.join("qubes"));
        for name in ["work", "web"] {
            store
                .save(&Qube::new(name, QubeKind::App, OsType::Freebsd))
                .unwrap();
        }
        store
            .save(&Qube::new("rx", QubeKind::App, OsType::Redox))
            .unwrap();
        Fixture { dir, cfg, store }
    }

    fn drop_request(f: &Fixture, src: &str, id: &str, body: &str) {
        let req = Path::new(&f.cfg.share_dir).join(src).join(share::REQUESTS);
        std::fs::create_dir_all(&req).unwrap();
        std::fs::write(req.join(id), body).unwrap();
    }

    #[test]
    fn scan_finds_requests_and_reports_bad_ones() {
        let f = fixture();
        let runner = MockRunner::new();
        let broker = Broker::new(&runner, &f.cfg, &f.store, Policy::default());
        assert!(broker.scan().unwrap().is_empty(), "no share dir yet");
        drop_request(&f, "work", "b", "copy notes web\n");
        drop_request(&f, "work", "a", "copy ../x web\n");
        drop_request(&f, "work", "a.reply", "ok\n");
        drop_request(&f, "work", ".tmp", "copy x web\n");
        // Being written by the guest: a half-written one would otherwise
        // be read empty, failed and removed under the writer's feet.
        drop_request(&f, "work", "c.tmp", "");
        drop_request(&f, "web", "bad id", "copy x work\n");
        std::fs::create_dir_all(Path::new(&f.cfg.share_dir).join("Not-A-Qube")).unwrap();
        // A qube whose guest never made a request has no req/ yet.
        std::fs::create_dir_all(Path::new(&f.cfg.share_dir).join("vault")).unwrap();
        let found = broker.scan().unwrap();
        let ids: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.src.as_str(), f.id.as_str()))
            .collect();
        assert_eq!(ids, vec![("web", "bad id"), ("work", "a"), ("work", "b")]);
        assert!(found[0]
            .request
            .as_ref()
            .unwrap_err()
            .contains("request id"));
        assert!(found[1].request.as_ref().unwrap_err().contains("file name"));
        assert_eq!(found[2].request.as_ref().unwrap().dst, "web");
        let f2 = broker.find("work", "b").unwrap();
        assert_eq!(f2.id, "b");
        assert!(broker.find("work", "zzz").is_err());
        std::fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn serve_applies_the_policy() {
        let f = fixture();
        let policy = Policy::parse("copy work * allow\ncopy web * deny\n").unwrap();
        drop_request(&f, "work", "1", "copy notes web\n");
        drop_request(&f, "web", "2", "copy x work\n");
        drop_request(&f, "work", "3", "copy notes rx\n");
        drop_request(&f, "work", "4", "nonsense\n");
        let runner = MockRunner::new();
        let broker = Broker::new(&runner, &f.cfg, &f.store, policy);
        let done = broker.serve_once().unwrap();
        let summary: Vec<(String, String)> = done
            .iter()
            .map(|(f, d)| {
                (
                    f.id.clone(),
                    match d {
                        Disposition::Delivered(d) => format!("ok {}", d.guest_path),
                        Disposition::Denied => "denied".into(),
                        Disposition::Pending => "pending".into(),
                        Disposition::Failed(e) => format!("failed {e}"),
                    },
                )
            })
            .collect();
        assert_eq!(summary[0], ("2".to_string(), "denied".to_string()));
        assert_eq!(
            summary[1],
            ("1".to_string(), "ok /mnt/jos/in/work/notes".to_string())
        );
        assert_eq!(summary[2].0, "3");
        assert!(summary[2].1.contains("no dom0 share"), "{:?}", summary[2]);
        assert_eq!(summary[3].0, "4");
        assert!(summary[3].1.starts_with("failed"), "{:?}", summary[3]);
        let calls = runner.calls();
        let share = &f.cfg.share_dir;
        assert!(
            calls
                .iter()
                .any(|c| c == &format!("mv {share}/work/out/notes {share}/web/in/work/notes")),
            "{calls:#?}"
        );
        assert!(
            calls.contains(&format!("tee {share}/work/req/1.reply")),
            "{calls:#?}"
        );

        assert!(
            calls.contains(&format!("rm -f {share}/work/req/1")),
            "{calls:#?}"
        );
        assert!(
            calls.contains(&format!("tee {share}/web/req/2.reply")),
            "{calls:#?}"
        );
        // Refused or failed: the export is discarded for the guest.
        assert!(
            calls.contains(&format!("rm -rf {share}/web/out/x")),
            "{calls:#?}"
        );
        assert!(
            calls.contains(&format!("rm -rf {share}/work/out/notes")),
            "{calls:#?}"
        );
        assert!(
            calls.contains(&format!("tee {share}/work/req/4.reply")),
            "{calls:#?}"
        );
        std::fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn old_replies_are_swept() {
        let f = fixture();
        let runner = MockRunner::new();
        let broker = Broker::new(&runner, &f.cfg, &f.store, Policy::default());
        broker.sweep_replies(std::time::SystemTime::now()).unwrap();
        drop_request(&f, "work", "1.reply", "ok /x\n");
        drop_request(&f, "work", "2", "copy a web\n");
        let now = std::time::SystemTime::now();
        broker.sweep_replies(now).unwrap();
        assert!(runner.calls().is_empty(), "fresh replies stay");
        broker.sweep_replies(now + REPLY_TTL * 2).unwrap();
        assert_eq!(
            runner.calls(),
            vec![format!("rm -f {}/work/req/1.reply", f.cfg.share_dir)]
        );
        std::fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn pending_requests_wait_for_the_operator() {
        let f = fixture();
        drop_request(&f, "work", "1", "copy notes web\n");
        drop_request(&f, "work", "2", "copy notes nowhere\n");
        let runner = MockRunner::new();
        let broker = Broker::new(&runner, &f.cfg, &f.store, Policy::default());
        let done = broker.serve_once().unwrap();
        assert!(done.iter().all(|(_, d)| *d == Disposition::Pending));
        assert!(runner.calls().is_empty());
        let one = broker.find("work", "1").unwrap();
        assert_eq!(
            broker.settle(&one, Some(Verdict::Deny)).unwrap(),
            Disposition::Denied
        );
        let two = broker.find("work", "2").unwrap();
        match broker.settle(&two, Some(Verdict::Allow)).unwrap() {
            Disposition::Failed(why) => assert!(why.contains("nowhere"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            broker.settle(&one, Some(Verdict::Allow)).unwrap(),
            Disposition::Delivered(_)
        ));
        std::fs::remove_dir_all(&f.dir).unwrap();
    }

    #[test]
    fn answer_failures_surface() {
        let f = fixture();
        drop_request(&f, "work", "1", "copy notes web\n");
        let runner = MockRunner::new().respond("tee", &[], CmdOutput::failed(1, "read-only"));
        let broker = Broker::new(
            &runner,
            &f.cfg,
            &f.store,
            Policy::parse("copy * * deny\n").unwrap(),
        );
        assert!(broker.serve_once().is_err());
        std::fs::remove_dir_all(&f.dir).unwrap();
    }
}
