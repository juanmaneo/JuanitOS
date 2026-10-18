//! The graphical installer's model: pages, fields, focus, validation.
//!
//! Inspired by Calamares (a sidebar of steps walked in order) and by
//! Anaconda's "nothing is written until you confirm" rule: every page
//! before **Summary** only edits this struct; the disk is touched once the
//! user ticks the erase confirmation and presses **Install**.
//!
//! The wizard is pure: it consumes [`Key`]s and installer [`Progress`]
//! and returns [`Action`]s. Drawing is in `ui.rs`, the event loop in
//! `gui.rs`; everything here is unit-tested without a screen.

use jos_fb::keys::Key;
use juanitos_core::doctor::{self, Check, Status};
use juanitos_core::pci::{PciAddr, PciDevice};

use crate::disks::Disk;
use crate::install::{InstallPlan, Progress};

/// Pages, in order (the sidebar).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Page {
    Welcome,
    Hardware,
    Disk,
    System,
    Passthrough,
    Users,
    Summary,
    Install,
    Finish,
}

pub const PAGES: [Page; 9] = [
    Page::Welcome,
    Page::Hardware,
    Page::Disk,
    Page::System,
    Page::Passthrough,
    Page::Users,
    Page::Summary,
    Page::Install,
    Page::Finish,
];

impl Page {
    /// Sidebar label.
    pub fn label(self) -> &'static str {
        match self {
            Page::Welcome => "Welcome",
            Page::Hardware => "Hardware",
            Page::Disk => "Disk",
            Page::System => "System",
            Page::Passthrough => "Passthrough",
            Page::Users => "Users",
            Page::Summary => "Summary",
            Page::Install => "Install",
            Page::Finish => "Finish",
        }
    }

    pub fn index(self) -> usize {
        PAGES.iter().position(|p| *p == self).unwrap_or(0)
    }

    fn next(self) -> Page {
        PAGES.get(self.index() + 1).copied().unwrap_or(self)
    }

    fn prev(self) -> Page {
        self.index()
            .checked_sub(1)
            .and_then(|i| PAGES.get(i))
            .copied()
            .unwrap_or(self)
    }
}

/// Focusable things on a page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    AcceptChecks,
    Disks,
    /// The Passthrough page's device list (Space ticks a device).
    Devices,
    Hostname,
    Swap,
    Desktop,
    RootPw,
    RootPw2,
    UserName,
    UserPw,
    UserPw2,
    ConfirmErase,
    Quit,
    Back,
    Next,
    Console,
    Reboot,
}

impl Widget {
    pub fn is_button(self) -> bool {
        matches!(
            self,
            Widget::Quit | Widget::Back | Widget::Next | Widget::Console | Widget::Reboot
        )
    }

    pub fn is_password(self) -> bool {
        matches!(
            self,
            Widget::RootPw | Widget::RootPw2 | Widget::UserPw | Widget::UserPw2
        )
    }

    pub fn is_checkbox(self) -> bool {
        matches!(
            self,
            Widget::AcceptChecks | Widget::Desktop | Widget::ConfirmErase
        )
    }
}

/// A single-line text input with a cursor (in characters).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextField {
    pub value: String,
    pub cursor: usize,
}

/// Longest value any field accepts (host names are limited to 63).
pub const MAX_FIELD_LEN: usize = 63;

impl TextField {
    pub fn new(value: &str) -> Self {
        TextField {
            value: value.to_string(),
            cursor: value.chars().count(),
        }
    }

    fn byte_at(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    pub fn len(&self) -> usize {
        self.value.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.value.is_empty()
    }

    pub fn insert(&mut self, c: char) {
        if self.len() >= MAX_FIELD_LEN || c.is_control() {
            return;
        }
        let at = self.byte_at(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_at(self.cursor);
        self.value.remove(at);
    }

    pub fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = self.byte_at(self.cursor);
            self.value.remove(at);
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    /// Text before the cursor (where the caret is drawn).
    pub fn before_cursor(&self) -> &str {
        self.value.get(..self.byte_at(self.cursor)).unwrap_or("")
    }
}

/// Where the installation is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    NotStarted,
    Running {
        index: usize,
        total: usize,
        title: String,
    },
    Succeeded,
    Failed(String),
}

/// What the event loop must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Start installing with this plan (in the background).
    Start(Box<InstallPlan>),
    Reboot,
    /// Leave the installer for the text console.
    Exit,
}

#[derive(Debug, Clone)]
pub struct Wizard {
    pub page: Page,
    /// Index into [`Wizard::widgets`] of the current page.
    pub focus: usize,
    base: InstallPlan,
    pub checks: Vec<Check>,
    pub disks: Vec<Disk>,
    pub disk_index: usize,
    /// Passthrough candidates (`pci::passthrough_candidates`).
    pub devices: Vec<PciDevice>,
    pub device_index: usize,
    /// Devices to reserve; may hold addresses that are not candidates
    /// (given with `--pci`), which are kept as they are.
    pub reserved: Vec<PciAddr>,
    pub hostname: TextField,
    pub swap: TextField,
    pub desktop: bool,
    pub root_pw: TextField,
    pub root_pw2: TextField,
    pub user_name: TextField,
    pub user_pw: TextField,
    pub user_pw2: TextField,
    pub accept_checks: bool,
    pub confirm_erase: bool,
    /// Validation message for the current page.
    pub error: Option<String>,
    pub install: InstallState,
    /// Steps completed so far, for the Install page.
    pub log: Vec<String>,
}

impl Wizard {
    /// `base` provides the defaults (pool, hostname, swap, dist directory,
    /// a preselected disk...) that the pages then edit.
    pub fn new(
        base: InstallPlan,
        checks: Vec<Check>,
        disks: Vec<Disk>,
        devices: Vec<PciDevice>,
    ) -> Self {
        let disk_index = disks.iter().position(|d| d.name == base.disk).unwrap_or(0);
        let mut w = Wizard {
            reserved: base.pptdevs.clone(),
            devices,
            device_index: 0,
            page: Page::Welcome,
            focus: 0,
            hostname: TextField::new(&base.hostname),
            swap: TextField::new(&base.swap_gb.to_string()),
            desktop: base.gui,
            root_pw: TextField::new(base.root_password.as_deref().unwrap_or("")),
            root_pw2: TextField::new(base.root_password.as_deref().unwrap_or("")),
            user_name: TextField::new(base.user.as_ref().map(|u| u.0.as_str()).unwrap_or("")),
            user_pw: TextField::new(base.user.as_ref().map(|u| u.1.as_str()).unwrap_or("")),
            user_pw2: TextField::new(base.user.as_ref().map(|u| u.1.as_str()).unwrap_or("")),
            base,
            checks,
            disks,
            disk_index,
            accept_checks: false,
            confirm_erase: false,
            error: None,
            install: InstallState::NotStarted,
            log: Vec::new(),
        };
        w.focus = w.initial_focus();
        w
    }

    pub fn checks_passed(&self) -> bool {
        doctor::all_passed(&self.checks)
    }

    /// Focusable widgets of the current page, in Tab order.
    pub fn widgets(&self) -> Vec<Widget> {
        use Widget::*;
        match self.page {
            Page::Welcome => vec![Quit, Next],
            Page::Hardware if self.checks_passed() => vec![Back, Next],
            Page::Hardware => vec![AcceptChecks, Back, Next],
            Page::Disk if self.disks.is_empty() => vec![Back],
            Page::Disk => vec![Disks, Back, Next],
            Page::System => vec![Hostname, Swap, Desktop, Back, Next],
            Page::Passthrough if self.devices.is_empty() => vec![Back, Next],
            Page::Passthrough => vec![Devices, Back, Next],
            Page::Users => vec![RootPw, RootPw2, UserName, UserPw, UserPw2, Back, Next],
            Page::Summary => vec![ConfirmErase, Back, Next],
            Page::Install => vec![],
            Page::Finish if self.install == InstallState::Succeeded => vec![Console, Reboot],
            Page::Finish => vec![Console],
        }
    }

    pub fn focused(&self) -> Option<Widget> {
        self.widgets().get(self.focus).copied()
    }

    /// First input of a page, or its primary button.
    fn initial_focus(&self) -> usize {
        let widgets = self.widgets();
        widgets
            .iter()
            .position(|w| !w.is_button())
            .unwrap_or(widgets.len().saturating_sub(1))
    }

    fn goto(&mut self, page: Page) {
        self.page = page;
        self.error = None;
        self.focus = self.initial_focus();
    }

    fn move_focus(&mut self, forward: bool) {
        let n = self.widgets().len();
        if n == 0 {
            return;
        }
        self.focus = if forward {
            (self.focus + 1) % n
        } else {
            (self.focus + n - 1) % n
        };
    }

    pub fn field(&self, w: Widget) -> Option<&TextField> {
        Some(match w {
            Widget::Hostname => &self.hostname,
            Widget::Swap => &self.swap,
            Widget::RootPw => &self.root_pw,
            Widget::RootPw2 => &self.root_pw2,
            Widget::UserName => &self.user_name,
            Widget::UserPw => &self.user_pw,
            Widget::UserPw2 => &self.user_pw2,
            _ => return None,
        })
    }

    fn field_mut(&mut self, w: Widget) -> Option<&mut TextField> {
        Some(match w {
            Widget::Hostname => &mut self.hostname,
            Widget::Swap => &mut self.swap,
            Widget::RootPw => &mut self.root_pw,
            Widget::RootPw2 => &mut self.root_pw2,
            Widget::UserName => &mut self.user_name,
            Widget::UserPw => &mut self.user_pw,
            Widget::UserPw2 => &mut self.user_pw2,
            _ => return None,
        })
    }

    pub fn checkbox(&self, w: Widget) -> bool {
        match w {
            Widget::AcceptChecks => self.accept_checks,
            Widget::Desktop => self.desktop,
            Widget::ConfirmErase => self.confirm_erase,
            _ => false,
        }
    }

    fn toggle(&mut self, w: Widget) {
        match w {
            Widget::AcceptChecks => self.accept_checks = !self.accept_checks,
            Widget::Desktop => self.desktop = !self.desktop,
            Widget::ConfirmErase => self.confirm_erase = !self.confirm_erase,
            _ => {}
        }
    }

    /// Characters a field accepts.
    fn accepts(w: Widget, c: char) -> bool {
        match w {
            Widget::Swap => c.is_ascii_digit(),
            Widget::Hostname => c.is_ascii_alphanumeric() || c == '-' || c == '.',
            Widget::UserName => {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
            }
            _ => !c.is_control(),
        }
    }

    pub fn selected_disk(&self) -> Option<&Disk> {
        self.disks.get(self.disk_index)
    }

    pub fn is_reserved(&self, addr: PciAddr) -> bool {
        self.reserved.contains(&addr)
    }

    fn toggle_device(&mut self) {
        let Some(addr) = self.devices.get(self.device_index).map(|d| d.addr) else {
            return;
        };
        if self.is_reserved(addr) {
            self.reserved.retain(|a| *a != addr);
        } else {
            self.reserved.push(addr);
        }
    }

    /// Every GPU is reserved: the dom0 will have no display of its own.
    pub fn reserves_every_gpu(&self) -> bool {
        let mut gpus = self.devices.iter().filter(|d| d.is_gpu()).peekable();
        gpus.peek().is_some() && gpus.all(|d| self.is_reserved(d.addr))
    }

    fn reserved_sorted(&self) -> Vec<PciAddr> {
        let mut r = self.reserved.clone();
        r.sort();
        r.dedup();
        r
    }

    /// Why the current page cannot be left forward, if it cannot.
    pub fn page_error(&self) -> Option<String> {
        match self.page {
            Page::Hardware if !self.checks_passed() && !self.accept_checks => {
                Some("Some checks failed: tick \"Install anyway\" to continue regardless.".into())
            }
            Page::Disk if self.disks.is_empty() => {
                Some("No disk found: JuanitOS needs a whole disk to install to.".into())
            }
            Page::System => validate_system(&self.hostname.value, &self.swap.value).err(),
            Page::Users => validate_users(
                &self.root_pw.value,
                &self.root_pw2.value,
                &self.user_name.value,
                &self.user_pw.value,
                &self.user_pw2.value,
            )
            .err(),
            Page::Summary if !self.confirm_erase => {
                Some("Tick the confirmation: the selected disk will be erased.".into())
            }
            _ => None,
        }
    }

    pub fn can_advance(&self) -> bool {
        self.page_error().is_none()
    }

    /// The plan the pages describe.
    pub fn plan(&self) -> InstallPlan {
        let user = (!self.user_name.is_empty())
            .then(|| (self.user_name.value.clone(), self.user_pw.value.clone()));
        InstallPlan {
            disk: self
                .selected_disk()
                .map(|d| d.name.clone())
                .unwrap_or_default(),
            hostname: self.hostname.value.clone(),
            swap_gb: self.swap.value.parse().unwrap_or(0),
            gui: self.desktop,
            root_password: Some(self.root_pw.value.clone()),
            user,
            pptdevs: self.reserved_sorted(),
            ..self.base.clone()
        }
    }

    /// Label of the forward button on this page.
    pub fn next_label(&self) -> &'static str {
        match self.page {
            Page::Welcome => "Start",
            Page::Summary => "Install",
            _ => "Next",
        }
    }

    fn next(&mut self) -> Action {
        if let Some(e) = self.page_error() {
            self.error = Some(e);
            return Action::None;
        }
        if self.page == Page::Summary {
            let plan = self.plan();
            if let Err(e) = plan.validate() {
                self.error = Some(e.to_string());
                return Action::None;
            }
            self.install = InstallState::Running {
                index: 0,
                total: crate::install::STEP_TITLES.len(),
                title: "Starting".into(),
            };
            self.log.clear();
            self.goto(Page::Install);
            return Action::Start(Box::new(plan));
        }
        self.goto(self.page.next());
        Action::None
    }

    fn back(&mut self) {
        if self.page > Page::Welcome && self.page < Page::Install {
            self.goto(self.page.prev());
        }
    }

    fn activate(&mut self, w: Widget) -> Action {
        match w {
            Widget::Next => self.next(),
            Widget::Back => {
                self.back();
                Action::None
            }
            Widget::Quit | Widget::Console => Action::Exit,
            Widget::Reboot => Action::Reboot,
            _ if w.is_checkbox() => {
                self.toggle(w);
                Action::None
            }
            // Enter in a field or the disk list: on to the next widget, or
            // the next page from the last input.
            _ => {
                let next_is_button = self
                    .widgets()
                    .get(self.focus + 1)
                    .is_none_or(|n| n.is_button());
                if next_is_button {
                    self.next()
                } else {
                    self.move_focus(true);
                    Action::None
                }
            }
        }
    }

    /// Handle one key press.
    pub fn handle(&mut self, key: Key) -> Action {
        if self.page == Page::Install {
            // Nothing to do while the disk is being written.
            return Action::None;
        }
        let Some(w) = self.focused() else {
            return Action::None;
        };
        if !matches!(key, Key::Char(_) | Key::Backspace | Key::Delete) {
            // Typing keeps the error visible until the user moves on.
            self.error = None;
        }
        match key {
            Key::Tab => self.move_focus(true),
            Key::BackTab => self.move_focus(false),
            Key::Esc => self.back(),
            Key::Enter => return self.activate(w),
            Key::Up | Key::Down if w == Widget::Disks => {
                self.disk_index = list_step(self.disk_index, self.disks.len(), key == Key::Up);
            }
            Key::Up | Key::Down if w == Widget::Devices => {
                self.device_index =
                    list_step(self.device_index, self.devices.len(), key == Key::Up);
            }
            Key::Char(' ') if w == Widget::Devices => self.toggle_device(),
            Key::Up => self.move_focus(false),
            Key::Down => self.move_focus(true),
            Key::Left | Key::Right if w.is_button() => self.move_focus(key == Key::Right),
            Key::Char(' ') if w.is_checkbox() => self.toggle(w),
            Key::Char(' ') if w.is_button() => return self.activate(w),
            Key::Char(c) if Self::accepts(w, c) => {
                if let Some(f) = self.field_mut(w) {
                    f.insert(c);
                }
            }
            _ => {
                if let Some(f) = self.field_mut(w) {
                    match key {
                        Key::Backspace => f.backspace(),
                        Key::Delete => f.delete(),
                        Key::Left => f.left(),
                        Key::Right => f.right(),
                        Key::Home => f.home(),
                        Key::End => f.end(),
                        _ => {}
                    }
                }
            }
        }
        Action::None
    }

    /// Installer progress from the background thread.
    pub fn on_progress(&mut self, p: Progress) {
        match p {
            Progress::Step {
                index,
                total,
                title,
            } => {
                if let InstallState::Running { title: prev, .. } = &self.install {
                    if index > 0 {
                        self.log.push(prev.clone());
                    }
                }
                self.install = InstallState::Running {
                    index,
                    total,
                    title,
                };
            }
            Progress::Done => {
                if let InstallState::Running { title, .. } = &self.install {
                    self.log.push(title.clone());
                }
            }
        }
    }

    /// The background installation ended.
    pub fn finished(&mut self, result: Result<(), String>) {
        self.install = match result {
            Ok(()) => InstallState::Succeeded,
            Err(e) => InstallState::Failed(e),
        };
        self.goto(Page::Finish);
    }

    /// Fraction of the installation done, 0..=1000 (per mille).
    pub fn progress_permille(&self) -> u32 {
        match &self.install {
            InstallState::NotStarted => 0,
            InstallState::Running { index, total, .. } => ((*index as u32) * 1000)
                .checked_div(*total as u32)
                .unwrap_or(0),
            InstallState::Succeeded | InstallState::Failed(_) => 1000,
        }
    }

    /// Key/value rows of the Summary page.
    pub fn summary_rows(&self) -> Vec<(&'static str, String)> {
        let p = self.plan();
        let disk = match self.selected_disk() {
            Some(d) => format!(
                "/dev/{}  {:.1} GiB  {}",
                d.name,
                d.size_gib(),
                d.description
            ),
            None => "(none)".into(),
        };
        vec![
            ("Disk", disk),
            (
                "Layout",
                format!(
                    "GPT: {} MiB EFI, {} ZFS pool \"{}\"",
                    p.esp_mb,
                    if p.swap_gb > 0 {
                        format!("{} GiB swap,", p.swap_gb)
                    } else {
                        "no swap,".into()
                    },
                    p.pool
                ),
            ),
            ("Hostname", p.hostname.clone()),
            (
                "Desktop",
                if p.gui { "COSMIC" } else { "console only" }.into(),
            ),
            ("Passthrough", p.passthrough_text()),
            ("Root", "password set".into()),
            (
                "User",
                p.user
                    .as_ref()
                    .map(|(n, _)| format!("{n} (wheel, video)"))
                    .unwrap_or_else(|| "(none)".into()),
            ),
        ]
    }

    /// Number of failed / warning checks.
    pub fn check_counts(&self) -> (usize, usize) {
        let count = |s: Status| self.checks.iter().filter(|c| c.status == s).count();
        (count(Status::Fail), count(Status::Warn))
    }
}

/// Move a list selection one row up or down, staying within `len` rows.
fn list_step(index: usize, len: usize, up: bool) -> usize {
    if up {
        index.saturating_sub(1)
    } else {
        (index + 1).min(len.saturating_sub(1))
    }
}

/// Hostname and swap size as typed on the System page.
pub fn validate_system(hostname: &str, swap: &str) -> Result<(), String> {
    if hostname.is_empty() {
        return Err("The hostname must not be empty.".into());
    }
    if hostname.starts_with(['-', '.']) || hostname.ends_with(['-', '.']) {
        return Err("The hostname must start and end with a letter or a digit.".into());
    }
    match swap.parse::<u64>() {
        Ok(n) if n <= 512 => Ok(()),
        Ok(_) => Err("Swap is limited to 512 GiB.".into()),
        Err(_) => Err("The swap size must be a number of GiB (0 for none).".into()),
    }
}

/// Passwords and the optional user, as typed on the Users page.
pub fn validate_users(
    root: &str,
    root2: &str,
    user: &str,
    pw: &str,
    pw2: &str,
) -> Result<(), String> {
    if root.is_empty() {
        return Err("Choose a root password.".into());
    }
    if root != root2 {
        return Err("The root passwords do not match.".into());
    }
    if user.is_empty() {
        return Ok(());
    }
    let first_ok = user
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    if !first_ok || user.len() > 32 {
        return Err("User names start with a lowercase letter (32 characters at most).".into());
    }
    if matches!(user, "root" | "toor" | "daemon" | "operator" | "nobody") {
        return Err(format!(
            "\"{user}\" is a system account; choose another name."
        ));
    }
    if pw.is_empty() {
        return Err(format!("Choose a password for {user}."));
    }
    if pw != pw2 {
        return Err(format!("The passwords for {user} do not match."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disks() -> Vec<Disk> {
        vec![
            Disk {
                name: "ada0".into(),
                size_bytes: 500 << 30,
                description: "Samsung SSD".into(),
            },
            Disk {
                name: "nda0".into(),
                size_bytes: 1 << 40,
                description: "WD NVMe".into(),
            },
        ]
    }

    fn ok_checks() -> Vec<Check> {
        vec![Check {
            name: "kernel module vmm".into(),
            status: Status::Ok,
            detail: "loaded".into(),
        }]
    }

    fn failed_checks() -> Vec<Check> {
        vec![
            Check {
                name: "iommu".into(),
                status: Status::Fail,
                detail: "disabled".into(),
            },
            Check {
                name: "ppt devices".into(),
                status: Status::Warn,
                detail: "none".into(),
            },
        ]
    }

    fn device(addr: PciAddr, vendor: u16, class: u32) -> PciDevice {
        PciDevice {
            addr,
            driver: "none0".into(),
            vendor,
            device: 0x1234,
            class,
            description: None,
        }
    }

    /// Intel iGPU, NVIDIA dGPU + audio, Wi-Fi.
    fn devices() -> Vec<PciDevice> {
        vec![
            device(PciAddr::new(0, 2, 0), 0x8086, 0x030000),
            device(PciAddr::new(0, 20, 3), 0x8086, 0x028000),
            device(PciAddr::new(1, 0, 0), 0x10de, 0x030000),
            device(PciAddr::new(1, 0, 1), 0x10de, 0x040300),
        ]
    }

    fn wizard() -> Wizard {
        Wizard::new(InstallPlan::default(), ok_checks(), disks(), devices())
    }

    fn typ(w: &mut Wizard, s: &str) {
        for c in s.chars() {
            w.handle(Key::Char(c));
        }
    }

    /// Fill every page up to the Summary.
    fn to_summary(w: &mut Wizard) {
        w.handle(Key::Enter); // Welcome -> Hardware
        w.handle(Key::Enter); // Hardware -> Disk
        w.handle(Key::Down); // nda0
        w.handle(Key::Enter); // Disk -> System (Enter in the list = next)
        assert_eq!(w.page, Page::System);
        for _ in 0..4 {
            w.handle(Key::Tab); // Swap, Desktop, Back, Next
        }
        assert_eq!(w.focused(), Some(Widget::Next));
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Passthrough);
        w.handle(Key::Enter); // the list or Next: on to Users
        assert_eq!(w.page, Page::Users);
        typ(w, "rootpw");
        w.handle(Key::Enter);
        typ(w, "rootpw");
        w.handle(Key::Enter);
        typ(w, "alice");
        w.handle(Key::Enter);
        typ(w, "pw");
        w.handle(Key::Enter);
        typ(w, "pw");
        w.handle(Key::Enter); // last field: Next
        assert_eq!(w.page, Page::Summary, "{:?}", w.error);
    }

    #[test]
    fn pages_and_labels() {
        assert_eq!(Page::Welcome.next(), Page::Hardware);
        assert_eq!(Page::Finish.next(), Page::Finish);
        assert_eq!(Page::Welcome.prev(), Page::Welcome);
        assert_eq!(Page::Users.prev(), Page::Passthrough);
        assert_eq!(Page::Passthrough.prev(), Page::System);
        assert_eq!(Page::Passthrough.label(), "Passthrough");
        for (i, p) in PAGES.iter().enumerate() {
            assert_eq!(p.index(), i);
            assert!(!p.label().is_empty());
        }
    }

    #[test]
    fn welcome_focuses_start_and_quit_exits() {
        let mut w = wizard();
        assert_eq!(w.focused(), Some(Widget::Next));
        assert_eq!(w.next_label(), "Start");
        w.handle(Key::Left);
        assert_eq!(w.focused(), Some(Widget::Quit));
        assert_eq!(w.handle(Key::Enter), Action::Exit);
        // Esc on the first page does nothing.
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Welcome);
    }

    #[test]
    fn walk_to_install_and_finish() {
        let mut w = wizard();
        to_summary(&mut w);
        assert_eq!(w.focused(), Some(Widget::ConfirmErase));
        assert_eq!(w.next_label(), "Install");
        // Install without confirming: refused with a message.
        w.handle(Key::Tab);
        w.handle(Key::Tab);
        assert_eq!(w.focused(), Some(Widget::Next));
        assert_eq!(w.handle(Key::Enter), Action::None);
        assert!(w.error.as_deref().unwrap_or("").contains("erased"));
        assert!(!w.can_advance());
        w.handle(Key::BackTab);
        w.handle(Key::BackTab);
        w.handle(Key::Char(' '));
        assert!(w.confirm_erase);
        assert!(w.can_advance());
        w.handle(Key::Tab);
        w.handle(Key::Tab);
        let Action::Start(plan) = w.handle(Key::Enter) else {
            panic!("expected Start")
        };
        assert_eq!(plan.disk, "nda0");
        assert_eq!(plan.root_password.as_deref(), Some("rootpw"));
        assert_eq!(plan.user, Some(("alice".into(), "pw".into())));
        assert_eq!(w.page, Page::Install);
        assert!(w.widgets().is_empty());
        assert_eq!(w.focused(), None);
        // Keys are ignored while installing.
        assert_eq!(w.handle(Key::Esc), Action::None);
        assert_eq!(w.page, Page::Install);

        w.on_progress(Progress::Step {
            index: 0,
            total: 4,
            title: "one".into(),
        });
        assert_eq!(w.progress_permille(), 0);
        w.on_progress(Progress::Step {
            index: 2,
            total: 4,
            title: "three".into(),
        });
        assert_eq!(w.progress_permille(), 500);
        assert_eq!(w.log, vec!["one".to_string()]);
        w.on_progress(Progress::Done);
        assert_eq!(w.log, vec!["one".to_string(), "three".to_string()]);
        w.finished(Ok(()));
        assert_eq!(w.page, Page::Finish);
        assert_eq!(w.progress_permille(), 1000);
        assert_eq!(w.focused(), Some(Widget::Reboot));
        assert_eq!(w.handle(Key::Enter), Action::Reboot);
        w.handle(Key::Left);
        assert_eq!(w.handle(Key::Char(' ')), Action::Exit);
    }

    #[test]
    fn failure_offers_only_the_console() {
        let mut w = wizard();
        w.finished(Err("gpart: busy".into()));
        assert_eq!(w.page, Page::Finish);
        assert_eq!(w.install, InstallState::Failed("gpart: busy".into()));
        assert_eq!(w.widgets(), vec![Widget::Console]);
        assert_eq!(w.handle(Key::Enter), Action::Exit);
        // Back is not possible from Finish.
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Finish);
    }

    #[test]
    fn failed_checks_must_be_accepted() {
        let mut w = Wizard::new(InstallPlan::default(), failed_checks(), disks(), vec![]);
        assert_eq!(w.check_counts(), (1, 1));
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Hardware);
        assert_eq!(w.focused(), Some(Widget::AcceptChecks));
        w.handle(Key::Tab);
        w.handle(Key::Tab);
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Hardware);
        assert!(w.error.is_some());
        w.handle(Key::Tab); // error cleared on navigation
        assert!(w.error.is_none());
        assert_eq!(w.focused(), Some(Widget::AcceptChecks));
        w.handle(Key::Enter); // toggles
        assert!(w.accept_checks);
        w.handle(Key::Up); // wraps to Next
        assert_eq!(w.focused(), Some(Widget::Next));
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Disk);
        // Esc goes back.
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Hardware);
    }

    #[test]
    fn no_disks_blocks_the_disk_page() {
        let mut w = Wizard::new(InstallPlan::default(), ok_checks(), vec![], vec![]);
        w.handle(Key::Enter);
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Disk);
        assert_eq!(w.widgets(), vec![Widget::Back]);
        assert!(w.page_error().unwrap().contains("No disk"));
        assert!(w.selected_disk().is_none());
        assert!(w.summary_rows()[0].1.contains("none"));
        assert_eq!(w.plan().disk, "");
    }

    #[test]
    fn disk_list_navigation_and_preselection() {
        let base = InstallPlan {
            disk: "nda0".into(),
            ..Default::default()
        };
        let mut w = Wizard::new(base, ok_checks(), disks(), vec![]);
        assert_eq!(w.disk_index, 1);
        w.handle(Key::Enter);
        w.handle(Key::Enter);
        assert_eq!(w.focused(), Some(Widget::Disks));
        w.handle(Key::Down);
        assert_eq!(w.disk_index, 1, "clamped at the end");
        w.handle(Key::Up);
        w.handle(Key::Up);
        assert_eq!(w.disk_index, 0);
        assert_eq!(w.selected_disk().map(|d| d.name.as_str()), Some("ada0"));
    }

    #[test]
    fn passthrough_page_ticks_devices() {
        let base = InstallPlan {
            pptdevs: vec![PciAddr::new(1, 0, 0), PciAddr::new(7, 0, 0)],
            ..Default::default()
        };
        let mut w = Wizard::new(base, ok_checks(), disks(), devices());
        w.goto(Page::Passthrough);
        assert_eq!(w.widgets(), [Widget::Devices, Widget::Back, Widget::Next]);
        assert_eq!(w.focused(), Some(Widget::Devices));
        assert!(w.is_reserved(PciAddr::new(1, 0, 0)));
        assert!(!Widget::Devices.is_button() && !Widget::Devices.is_checkbox());

        // Down to the NVIDIA audio, tick it; Up is clamped at the top.
        for _ in 0..5 {
            w.handle(Key::Down);
        }
        assert_eq!(w.device_index, 3, "clamped at the end");
        w.handle(Key::Char(' '));
        assert!(w.is_reserved(PciAddr::new(1, 0, 1)));
        for _ in 0..5 {
            w.handle(Key::Up);
        }
        assert_eq!(w.device_index, 0);
        assert!(!w.reserves_every_gpu());
        w.handle(Key::Char(' ')); // the iGPU too: no display left
        assert!(w.reserves_every_gpu());
        w.handle(Key::Char(' '));
        assert!(!w.is_reserved(PciAddr::new(0, 2, 0)));

        // Addresses that are not candidates (from --pci) are kept, sorted.
        assert_eq!(
            w.plan().pptdevs,
            [
                PciAddr::new(1, 0, 0),
                PciAddr::new(1, 0, 1),
                PciAddr::new(7, 0, 0)
            ]
        );

        // Enter in the list moves on.
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Users);

        // Without candidates the page only has its buttons.
        let mut empty = Wizard::new(InstallPlan::default(), ok_checks(), disks(), vec![]);
        empty.goto(Page::Passthrough);
        assert_eq!(empty.widgets(), [Widget::Back, Widget::Next]);
        assert!(!empty.reserves_every_gpu());
        empty.toggle_device();
        assert!(empty.reserved.is_empty());
        assert!(empty.plan().pptdevs.is_empty());
    }

    #[test]
    fn system_page_validation_and_filters() {
        let mut w = wizard();
        w.goto(Page::System);
        assert_eq!(w.focused(), Some(Widget::Hostname));
        // Illegal characters are not inserted.
        typ(&mut w, "!_ ");
        assert_eq!(w.hostname.value, "juanitos");
        for _ in 0..8 {
            w.handle(Key::Backspace);
        }
        assert!(w.page_error().unwrap().contains("empty"));
        typ(&mut w, "-box");
        assert!(w.page_error().unwrap().contains("start and end"));
        w.handle(Key::Home);
        w.handle(Key::Delete);
        assert_eq!(w.hostname.value, "box");
        w.handle(Key::Tab);
        assert_eq!(w.focused(), Some(Widget::Swap));
        typ(&mut w, "x9");
        assert_eq!(w.swap.value, "49");
        assert!(w.can_advance());
        typ(&mut w, "99");
        assert!(w.page_error().unwrap().contains("512"));
        w.handle(Key::End);
        for _ in 0..4 {
            w.handle(Key::Backspace);
        }
        assert!(w.page_error().unwrap().contains("number"));
        typ(&mut w, "0");
        w.handle(Key::Tab);
        assert_eq!(w.focused(), Some(Widget::Desktop));
        w.handle(Key::Char(' '));
        assert!(!w.desktop);
        assert!(w.checkbox(Widget::Desktop) == w.desktop);
        let p = w.plan();
        assert_eq!(p.swap_gb, 0);
        assert!(!p.gui);
        assert_eq!(p.hostname, "box");
    }

    #[test]
    fn users_page_validation() {
        assert!(validate_users("", "", "", "", "").is_err());
        assert!(validate_users("a", "b", "", "", "")
            .unwrap_err()
            .contains("root passwords"));
        assert!(validate_users("a", "a", "", "", "").is_ok());
        assert!(validate_users("a", "a", "9lives", "p", "p").is_err());
        assert!(validate_users("a", "a", "toor", "p", "p")
            .unwrap_err()
            .contains("system account"));
        assert!(validate_users("a", "a", "alice", "", "")
            .unwrap_err()
            .contains("Choose a password"));
        assert!(validate_users("a", "a", "alice", "p", "q")
            .unwrap_err()
            .contains("do not match"));
        assert!(validate_users("a", "a", "alice", "p", "p").is_ok());
        assert!(validate_users("a", "a", &"j".repeat(33), "p", "p").is_err());

        let mut w = wizard();
        w.goto(Page::Users);
        // User names only take lowercase, digits, '_' and '-'.
        w.focus = 2;
        typ(&mut w, "Ju an");
        assert_eq!(w.user_name.value, "uan");
        w.handle(Key::Enter);
        assert_eq!(w.focused(), Some(Widget::UserPw));
        // Enter in the last field tries the next page and reports why not.
        w.focus = 4;
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Users);
        assert!(w.error.is_some());
        // Typing keeps the message; moving clears it.
        w.handle(Key::Char('x'));
        assert!(w.error.is_some());
        w.handle(Key::Tab);
        assert!(w.error.is_none());
    }

    #[test]
    fn text_field_editing() {
        let mut f = TextField::new("héllo");
        assert_eq!(f.cursor, 5);
        assert_eq!(f.len(), 5);
        f.left();
        f.left();
        f.left();
        f.left();
        assert_eq!(f.before_cursor(), "h");
        f.delete();
        assert_eq!(f.value, "hllo");
        f.insert('e');
        assert_eq!(f.value, "hello");
        f.backspace();
        f.backspace();
        f.backspace();
        assert_eq!(f.value, "llo");
        assert_eq!(f.cursor, 0);
        f.right();
        f.right();
        f.right();
        f.right();
        assert_eq!(f.cursor, 3);
        f.delete();
        f.home();
        f.insert('\n');
        assert_eq!(f.value, "llo");
        f.end();
        for _ in 0..100 {
            f.insert('x');
        }
        assert_eq!(f.len(), MAX_FIELD_LEN);
        assert!(!f.is_empty());
        assert!(TextField::default().is_empty());
    }

    #[test]
    fn base_plan_prefills_fields() {
        let base = InstallPlan {
            hostname: "box".into(),
            swap_gb: 8,
            root_password: Some("r".into()),
            user: Some(("u".into(), "p".into())),
            gui: false,
            ..Default::default()
        };
        let w = Wizard::new(base, ok_checks(), disks(), vec![]);
        assert_eq!(w.hostname.value, "box");
        assert_eq!(w.swap.value, "8");
        assert_eq!(w.root_pw2.value, "r");
        assert_eq!(w.user_pw2.value, "p");
        assert!(!w.desktop);
        let p = w.plan();
        assert_eq!(p.pool, "zroot");
        assert_eq!(p.user, Some(("u".into(), "p".into())));
    }

    #[test]
    fn summary_rows_describe_the_plan() {
        let mut w = wizard();
        to_summary(&mut w);
        let rows = w.summary_rows();
        let get = |k: &str| rows.iter().find(|(n, _)| *n == k).map(|(_, v)| v.clone());
        assert!(get("Disk").unwrap().contains("/dev/nda0"));
        assert!(get("Layout").unwrap().contains("4 GiB swap"));
        assert_eq!(get("Desktop").unwrap(), "COSMIC");
        assert_eq!(get("Passthrough").unwrap(), "none");
        assert!(get("User").unwrap().contains("alice"));
        w.reserved = vec![PciAddr::new(1, 0, 1), PciAddr::new(1, 0, 0)];
        let rows = w.summary_rows();
        assert!(rows
            .iter()
            .any(|(k, v)| *k == "Passthrough" && v == "1/0/0 1/0/1"));
        w.swap = TextField::new("0");
        w.user_name = TextField::default();
        let rows = w.summary_rows();
        assert!(rows.iter().any(|(_, v)| v.contains("no swap")));
        assert!(rows.iter().any(|(k, v)| *k == "User" && v == "(none)"));
    }

    #[test]
    fn widget_kinds() {
        assert!(Widget::Next.is_button());
        assert!(!Widget::Disks.is_button());
        assert!(Widget::RootPw.is_password());
        assert!(!Widget::UserName.is_password());
        assert!(Widget::ConfirmErase.is_checkbox());
        let w = wizard();
        assert!(w.field(Widget::Next).is_none());
        assert!(w.field(Widget::UserPw2).is_some());
        assert!(!w.checkbox(Widget::Next));
    }

    #[test]
    fn invalid_plan_is_caught_before_install() {
        let base = InstallPlan {
            pool: "Bad Pool".into(),
            ..Default::default()
        };
        let mut w = Wizard::new(base, ok_checks(), disks(), vec![]);
        to_summary(&mut w);
        w.confirm_erase = true;
        w.focus = 2;
        assert_eq!(w.handle(Key::Enter), Action::None);
        assert!(w.error.as_deref().unwrap_or("").contains("pool"));
        assert_eq!(w.page, Page::Summary);
    }
}
