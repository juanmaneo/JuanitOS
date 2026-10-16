//! The login screen as a pure state machine: keys in, [`Action`]s out.
//! Nothing here touches the system, so every behaviour is unit-tested on
//! any host; [`crate::gui`] wires it to a display and a [`crate::gui::Backend`].

use jos_fb::keys::Key;

use crate::sessions::Session;

pub const MAX_USER: usize = 32;
pub const MAX_PASSWORD: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    User,
    Password,
    Session,
}

const FOCUS_ORDER: [Focus; 3] = [Focus::User, Focus::Password, Focus::Session];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Power {
    Off,
    Reboot,
}

impl Power {
    pub fn key(self) -> u8 {
        match self {
            Power::Off => 1,
            Power::Reboot => 2,
        }
    }

    pub fn verb(self) -> &'static str {
        match self {
            Power::Off => "power off",
            Power::Reboot => "reboot",
        }
    }
}

/// A login attempt. The password is wiped when the request is dropped.
#[derive(PartialEq, Eq)]
pub struct Request {
    pub user: String,
    pub password: String,
    pub session: Session,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("user", &self.user)
            .field("password", &"<hidden>")
            .field("session", &self.session.name)
            .finish()
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        wipe(&mut self.password);
    }
}

/// Overwrite a secret before its memory is freed.
pub fn wipe(s: &mut String) {
    let mut bytes = std::mem::take(s).into_bytes();
    bytes.fill(0);
    // Keep the zeroing from being optimised away as a dead store.
    std::hint::black_box(&bytes);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Login(Request),
    Power(Power),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    Info(String),
    Error(String),
}

#[derive(Debug)]
pub struct Greeter {
    pub hostname: String,
    pub user: String,
    pub password: String,
    /// Accounts offered with Left/Right in the user field.
    pub users: Vec<String>,
    pub sessions: Vec<Session>,
    pub session: usize,
    pub focus: Focus,
    pub message: Option<Message>,
    /// A power key pressed once, waiting for the confirming second press.
    pub armed: Option<Power>,
}

impl Drop for Greeter {
    fn drop(&mut self) {
        wipe(&mut self.password);
    }
}

impl Greeter {
    /// `users` pre-fills the user field with the first account (focus then
    /// starts on the password, the common case of a single-user dom0).
    pub fn new(hostname: &str, users: Vec<String>, sessions: Vec<Session>) -> Self {
        let sessions = if sessions.is_empty() {
            vec![Session::cosmic_default()]
        } else {
            sessions
        };
        let user = users.first().cloned().unwrap_or_default();
        let focus = if user.is_empty() {
            Focus::User
        } else {
            Focus::Password
        };
        Greeter {
            hostname: hostname.to_string(),
            user,
            password: String::new(),
            users,
            sessions,
            session: 0,
            focus,
            message: None,
            armed: None,
        }
    }

    pub fn current_session(&self) -> Session {
        self.sessions
            .get(self.session)
            .or_else(|| self.sessions.first())
            .cloned()
            .unwrap_or_else(Session::cosmic_default)
    }

    pub fn handle(&mut self, key: Key) -> Action {
        if let Key::F(n) = key {
            if let Some(p) = [Power::Off, Power::Reboot]
                .into_iter()
                .find(|p| p.key() == n)
            {
                return self.power_key(p);
            }
        }
        if self.armed.take().is_some() {
            self.message = None;
        }
        match key {
            Key::Tab | Key::Down => self.move_focus(1),
            Key::BackTab | Key::Up => self.move_focus(FOCUS_ORDER.len() - 1),
            Key::Left => self.cycle(false),
            Key::Right => self.cycle(true),
            Key::Char(' ') if self.focus == Focus::Session => self.cycle(true),
            Key::Char(c) => self.insert(c),
            Key::Backspace => {
                if let Some(f) = self.text_field() {
                    f.pop();
                }
            }
            Key::Ctrl('u') => {
                if let Some(f) = self.text_field() {
                    wipe(f);
                }
            }
            Key::Esc => {
                wipe(&mut self.password);
                self.message = None;
            }
            Key::Enter => return self.enter(),
            _ => {}
        }
        Action::None
    }

    fn power_key(&mut self, p: Power) -> Action {
        if self.armed == Some(p) {
            self.armed = None;
            self.message = Some(Message::Info(format!("Going to {}...", p.verb())));
            return Action::Power(p);
        }
        self.armed = Some(p);
        self.message = Some(Message::Info(format!(
            "Press F{} again to {}",
            p.key(),
            p.verb()
        )));
        Action::None
    }

    fn move_focus(&mut self, step: usize) {
        let i = FOCUS_ORDER
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        if let Some(f) = FOCUS_ORDER.get((i + step) % FOCUS_ORDER.len()) {
            self.focus = *f;
        }
    }

    fn cycle(&mut self, forward: bool) {
        match self.focus {
            Focus::Session => {
                let n = self.sessions.len().max(1);
                self.session = if forward {
                    (self.session + 1) % n
                } else {
                    (self.session + n - 1) % n
                };
            }
            Focus::User if !self.users.is_empty() => {
                let n = self.users.len();
                let i = self.users.iter().position(|u| *u == self.user);
                let next = match (i, forward) {
                    (None, _) => 0,
                    (Some(i), true) => (i + 1) % n,
                    (Some(i), false) => (i + n - 1) % n,
                };
                if let Some(u) = self.users.get(next) {
                    self.user = u.clone();
                }
            }
            _ => {}
        }
    }

    fn text_field(&mut self) -> Option<&mut String> {
        match self.focus {
            Focus::User => Some(&mut self.user),
            Focus::Password => Some(&mut self.password),
            Focus::Session => None,
        }
    }

    fn insert(&mut self, c: char) {
        if c.is_control() {
            return;
        }
        match self.focus {
            // A login name: what pw(8) accepts, give or take.
            Focus::User
                if self.user.chars().count() < MAX_USER
                    && (c.is_ascii_alphanumeric() || "._-".contains(c)) =>
            {
                self.user.push(c);
            }
            Focus::Password if self.password.chars().count() < MAX_PASSWORD => {
                self.password.push(c);
            }
            _ => {}
        }
    }

    fn enter(&mut self) -> Action {
        if self.user.is_empty() {
            self.focus = Focus::User;
            self.message = Some(Message::Error("Enter a user name".to_string()));
            return Action::None;
        }
        if self.focus == Focus::User {
            self.focus = Focus::Password;
            return Action::None;
        }
        self.message = Some(Message::Info("Logging in...".to_string()));
        Action::Login(Request {
            user: self.user.clone(),
            password: std::mem::take(&mut self.password),
            session: self.current_session(),
        })
    }

    /// Authentication was refused: say why, ready for another try.
    pub fn login_failed(&mut self, why: &str) {
        wipe(&mut self.password);
        self.focus = Focus::Password;
        self.message = Some(Message::Error(why.to_string()));
    }

    /// The desktop session ended (log out, or it crashed).
    pub fn session_ended(&mut self, result: Result<(), String>) {
        wipe(&mut self.password);
        self.focus = Focus::Password;
        self.message = Some(match result {
            Ok(()) => Message::Info(format!("{} logged out", self.user)),
            Err(e) => Message::Error(e),
        });
    }
}

/// Login names of the people using this dom0, from `/etc/passwd`: uids
/// from 1000 (where pw(8) starts) up to `nobody`, with a real shell.
pub fn human_users(passwd: &str) -> Vec<String> {
    passwd
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            let name = f.first()?;
            let uid: u32 = f.get(2)?.parse().ok()?;
            let shell = f.get(6).copied().unwrap_or("");
            let real_shell = !shell.is_empty() && !shell.ends_with("nologin");
            ((1000..65534).contains(&uid) && real_shell && !name.is_empty())
                .then(|| name.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sway() -> Session {
        Session {
            name: "Sway".into(),
            exec: vec!["sway".into()],
            desktop: "sway".into(),
        }
    }

    fn greeter() -> Greeter {
        Greeter::new(
            "box",
            vec!["juan".into(), "ana".into()],
            vec![Session::cosmic_default(), sway()],
        )
    }

    fn typed(g: &mut Greeter, s: &str) {
        for c in s.chars() {
            assert_eq!(g.handle(Key::Char(c)), Action::None);
        }
    }

    #[test]
    fn prefilled_user_logs_in_with_the_password() {
        let mut g = greeter();
        assert_eq!(g.user, "juan");
        assert_eq!(g.focus, Focus::Password);
        typed(&mut g, "s3cret");
        assert_eq!(g.password, "s3cret");
        let Action::Login(req) = g.handle(Key::Enter) else {
            panic!("expected a login")
        };
        assert_eq!(req.user, "juan");
        assert_eq!(req.password, "s3cret");
        assert_eq!(req.session, Session::cosmic_default());
        assert!(g.password.is_empty(), "the greeter keeps no copy");
        assert_eq!(g.message, Some(Message::Info("Logging in...".into())));
        let dbg = format!("{req:?}");
        assert!(!dbg.contains("s3cret") && dbg.contains("<hidden>"), "{dbg}");
    }

    #[test]
    fn empty_user_is_refused_and_enter_moves_on() {
        let mut g = Greeter::new("box", vec![], vec![]);
        assert_eq!(g.focus, Focus::User);
        assert_eq!(g.sessions, vec![Session::cosmic_default()]);
        assert_eq!(g.handle(Key::Enter), Action::None);
        assert!(matches!(g.message, Some(Message::Error(_))));
        typed(&mut g, "ana bad!");
        assert_eq!(
            g.user, "anabad",
            "spaces and punctuation are not login names"
        );
        assert_eq!(g.handle(Key::Enter), Action::None);
        assert_eq!(g.focus, Focus::Password);
        // Users cycle only when known.
        g.focus = Focus::User;
        g.handle(Key::Right);
        assert_eq!(g.user, "anabad");
        // Submitting from the session selector works too.
        g.focus = Focus::Session;
        assert!(matches!(g.handle(Key::Enter), Action::Login(_)));
    }

    #[test]
    fn focus_moves_both_ways() {
        let mut g = greeter();
        g.handle(Key::Tab);
        assert_eq!(g.focus, Focus::Session);
        g.handle(Key::Down);
        assert_eq!(g.focus, Focus::User);
        g.handle(Key::BackTab);
        assert_eq!(g.focus, Focus::Session);
        g.handle(Key::Up);
        assert_eq!(g.focus, Focus::Password);
    }

    #[test]
    fn sessions_and_users_cycle() {
        let mut g = greeter();
        g.focus = Focus::Session;
        g.handle(Key::Right);
        assert_eq!(g.current_session().name, "Sway");
        g.handle(Key::Char(' '));
        assert_eq!(g.current_session().name, "COSMIC");
        g.handle(Key::Left);
        assert_eq!(g.current_session().name, "Sway");
        typed(&mut g, "x");
        assert!(g.password.is_empty() && g.user == "juan");
        g.handle(Key::Backspace);

        g.focus = Focus::User;
        g.handle(Key::Right);
        assert_eq!(g.user, "ana");
        g.handle(Key::Right);
        assert_eq!(g.user, "juan");
        g.handle(Key::Left);
        assert_eq!(g.user, "ana");
        g.handle(Key::Ctrl('u'));
        assert_eq!(g.user, "");
        g.handle(Key::Left);
        assert_eq!(g.user, "juan", "an unknown name restarts the list");
        // Arrows in the password field do nothing.
        g.focus = Focus::Password;
        g.handle(Key::Left);
        assert_eq!(g.user, "juan");
        // Out-of-range index still yields a session.
        g.session = 99;
        assert_eq!(g.current_session().name, "COSMIC");
    }

    #[test]
    fn editing_limits_and_escape() {
        let mut g = greeter();
        typed(&mut g, "ab");
        g.handle(Key::Backspace);
        assert_eq!(g.password, "a");
        g.handle(Key::Char('\u{7}'));
        assert_eq!(g.password, "a");
        g.handle(Key::Esc);
        assert!(g.password.is_empty() && g.message.is_none());
        typed(&mut g, &"p".repeat(MAX_PASSWORD + 5));
        assert_eq!(g.password.len(), MAX_PASSWORD);
        g.handle(Key::Ctrl('u'));
        assert!(g.password.is_empty());
        g.focus = Focus::User;
        g.handle(Key::Ctrl('u'));
        typed(&mut g, &"u".repeat(MAX_USER + 5));
        assert_eq!(g.user.len(), MAX_USER);
        // Unbound keys are ignored.
        assert_eq!(g.handle(Key::PageUp), Action::None);
        assert_eq!(g.handle(Key::F(9)), Action::None);
        g.focus = Focus::Session;
        g.handle(Key::Backspace);
        g.handle(Key::Ctrl('u'));
        assert_eq!(g.user.len(), MAX_USER);
    }

    #[test]
    fn power_keys_need_a_second_press() {
        let mut g = greeter();
        assert_eq!(g.handle(Key::F(1)), Action::None);
        assert_eq!(g.armed, Some(Power::Off));
        assert!(matches!(&g.message, Some(Message::Info(m)) if m.contains("F1 again")));
        // Another key disarms.
        g.handle(Key::Char('a'));
        assert_eq!(g.armed, None);
        assert_eq!(g.message, None);
        assert_eq!(g.handle(Key::F(1)), Action::None);
        assert_eq!(g.handle(Key::F(1)), Action::Power(Power::Off));
        // Switching power key re-arms the new one.
        g.handle(Key::F(1));
        assert_eq!(g.handle(Key::F(2)), Action::None);
        assert_eq!(g.armed, Some(Power::Reboot));
        assert_eq!(g.handle(Key::F(2)), Action::Power(Power::Reboot));
        assert!(matches!(&g.message, Some(Message::Info(m)) if m.contains("reboot")));
    }

    #[test]
    fn results_reset_the_password() {
        let mut g = greeter();
        typed(&mut g, "x");
        g.login_failed("Login incorrect");
        assert!(g.password.is_empty());
        assert_eq!(g.message, Some(Message::Error("Login incorrect".into())));
        g.focus = Focus::User;
        g.session_ended(Ok(()));
        assert_eq!(g.focus, Focus::Password);
        assert_eq!(g.message, Some(Message::Info("juan logged out".into())));
        g.session_ended(Err("COSMIC exited with status 1".into()));
        assert!(matches!(g.message, Some(Message::Error(_))));
    }

    #[test]
    fn wipe_clears() {
        let mut s = String::from("secret");
        wipe(&mut s);
        assert!(s.is_empty());
    }

    #[test]
    fn humans_from_passwd() {
        let passwd = "# comment\nroot:*:0:0:Charlie &:/root:/bin/sh\n\
            nobody:*:65534:65534:Unprivileged user:/nonexistent:/usr/sbin/nologin\n\
            juan:*:1001:1001:Juan:/home/juan:/bin/sh\n\
            svc:*:1002:1002:svc:/var/empty:/usr/sbin/nologin\n\
            noshell:*:1003:1003::/home/x:\n\
            broken\n\
            ana:*:1004:1004:Ana:/home/ana:/usr/local/bin/zsh\n";
        assert_eq!(human_users(passwd), vec!["juan", "ana"]);
    }
}
