// SPDX-License-Identifier: BSD-3-Clause
//! The first-boot wizard's state: the pages, what is focused, the choices
//! (the card for `sys-net`, the app qubes, their template) and the
//! progress of `jos setup`. Pure: keys and clicks come in, [`Action`]s
//! for the event loop go out, so every path is tested without a screen.

use jos_fb::keys::Key;
use juanitos_core::ifaces::{self, IfaceKind, NetIface};
use juanitos_core::setup::{self, AppSpec};
use juanitos_core::template;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Page {
    Welcome,
    Network,
    Qubes,
    Summary,
    Progress,
    Done,
}

/// The pages in order, as the sidebar lists them.
pub const PAGES: [Page; 6] = [
    Page::Welcome,
    Page::Network,
    Page::Qubes,
    Page::Summary,
    Page::Progress,
    Page::Done,
];

impl Page {
    pub fn label(self) -> &'static str {
        match self {
            Page::Welcome => "Welcome",
            Page::Network => "Network",
            Page::Qubes => "Qubes",
            Page::Summary => "Summary",
            Page::Progress => "Creating",
            Page::Done => "Done",
        }
    }
}

/// Something that can have the focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Widget {
    /// The list of network cards.
    Ifaces,
    Rescan,
    Passthrough,
    /// The list of app qubes (ticks).
    Apps,
    /// The list of templates.
    Templates,
    Gpu,
    Skip,
    Back,
    Next,
    Later,
    Retry,
    Finish,
    Reboot,
}

impl Widget {
    /// Buttons of the bottom bar, with their label.
    pub fn button_label(self) -> Option<&'static str> {
        match self {
            Widget::Skip => Some("Skip"),
            Widget::Back => Some("Back"),
            Widget::Later => Some("Later"),
            Widget::Retry => Some("Retry"),
            Widget::Finish => Some("Log in"),
            Widget::Reboot => Some("Reboot"),
            _ => None,
        }
    }
}

/// What a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Widget(Widget),
    Iface(usize),
    App(usize),
    Template(usize),
}

/// How the user left the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Finished or skipped: do not show the wizard again.
    Done,
    /// Setup failed and the user will deal with it later: show it again at
    /// the next boot.
    Later,
    /// Done, and the reserved devices need a reboot.
    Reboot,
}

/// What the event loop has to do after a key or a click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    None,
    /// Load the USB network drivers and list the cards again.
    Rescan,
    /// Run `jos` with these arguments.
    Start(Vec<String>),
    Leave(Outcome),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunState {
    Idle,
    /// Step `index` of `total` (1-based; 0 before the first one).
    Running {
        index: usize,
        total: usize,
        step: String,
    },
    Succeeded,
    Failed(String),
}

/// A template the app qubes can be cloned from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    pub name: &'static str,
    pub summary: &'static str,
    /// Its image is in the dom0's cache: it builds without a download.
    pub local: bool,
}

/// Recipes that make app qubes.
const APP_TEMPLATES: &[&str] = &[setup::APP_TEMPLATE, setup::GPU_TEMPLATE];

#[derive(Debug, Clone)]
pub struct Wizard {
    pub page: Page,
    pub ifaces: Vec<NetIface>,
    pub iface: Option<usize>,
    /// PCI passthrough works on this host.
    pub iommu: bool,
    /// Pass the card through rather than bridge it (when it can be).
    pub passthrough: bool,
    /// Every proposed app qube, ticked or not.
    pub apps: Vec<(AppSpec, bool)>,
    pub app: usize,
    pub templates: Vec<Template>,
    pub template: usize,
    pub gpu: bool,
    pub(crate) focus: usize,
    pub run: RunState,
    /// Steps done.
    pub log: Vec<String>,
    /// The last line `jos setup` printed that was not a step.
    pub detail: String,
    /// `jos setup` asked for a reboot.
    pub reboot: bool,
    pub error: Option<String>,
}

impl Wizard {
    /// `local`: the recipes whose image is in the cache
    /// ([`template::local_images`]).
    pub fn new(ifaces: Vec<NetIface>, iommu: bool, local: &[String]) -> Self {
        let templates: Vec<Template> = template::RECIPES
            .iter()
            .filter(|r| APP_TEMPLATES.contains(&r.name))
            .map(|r| Template {
                name: r.name,
                summary: r.summary,
                local: local.iter().any(|l| l == r.name),
            })
            .collect();
        let template = templates
            .iter()
            .position(|t| t.name == setup::APP_TEMPLATE)
            .unwrap_or(0);
        let mut w = Wizard {
            page: Page::Welcome,
            ifaces: Vec::new(),
            iface: None,
            iommu,
            passthrough: true,
            apps: setup::default_apps()
                .into_iter()
                .map(|a| (a, true))
                .collect(),
            app: 0,
            templates,
            template,
            gpu: true,
            focus: 0,
            run: RunState::Idle,
            log: Vec::new(),
            detail: String::new(),
            reboot: false,
            error: None,
        };
        w.set_ifaces(ifaces);
        w.go(Page::Welcome);
        w
    }

    /// A new list of cards (a rescan): the selection follows its card by
    /// name, else the preferred one is selected.
    pub fn set_ifaces(&mut self, ifaces: Vec<NetIface>) {
        let focused = self.focused();
        let kept = self.selected_iface().map(|i| i.name.clone());
        self.iface = kept
            .and_then(|name| ifaces.iter().position(|i| i.name == name))
            .or_else(|| ifaces::preferred(&ifaces, None));
        self.ifaces = ifaces;
        // The list or the passthrough tick may have appeared or gone: the
        // focus stays on its widget, else in range.
        self.focus = self.focus.min(self.widgets().len().saturating_sub(1));
        if let Some(w) = focused {
            self.focus_on(w);
        }
    }

    pub fn selected_iface(&self) -> Option<&NetIface> {
        self.iface.and_then(|i| self.ifaces.get(i))
    }

    /// The selected card is a PCI one and the IOMMU can hand it over.
    pub fn can_passthrough(&self) -> bool {
        self.iommu
            && matches!(
                self.selected_iface().map(|i| i.kind),
                Some(IfaceKind::Pci(_))
            )
    }

    pub fn passes_through(&self) -> bool {
        self.can_passthrough() && self.passthrough
    }

    /// The focusable widgets of the current page, in Tab order.
    pub fn widgets(&self) -> Vec<Widget> {
        use Widget::*;
        match self.page {
            Page::Welcome => vec![Skip, Next],
            Page::Network => {
                let mut v = Vec::new();
                if !self.ifaces.is_empty() {
                    v.push(Ifaces);
                }
                v.push(Rescan);
                if self.can_passthrough() {
                    v.push(Passthrough);
                }
                v.extend([Back, Next]);
                v
            }
            Page::Qubes => vec![Apps, Templates, Gpu, Back, Next],
            Page::Summary => vec![Back, Next],
            Page::Progress => Vec::new(),
            Page::Done => match (&self.run, self.reboot) {
                (RunState::Succeeded, true) => vec![Finish, Reboot],
                (RunState::Succeeded, false) => vec![Finish],
                _ => vec![Later, Retry],
            },
        }
    }

    pub fn focused(&self) -> Option<Widget> {
        self.widgets().get(self.focus).copied()
    }

    pub fn can_advance(&self) -> bool {
        self.page != Page::Network || self.selected_iface().is_some_and(NetIface::usable)
    }

    pub fn next_label(&self) -> &'static str {
        if self.page == Page::Summary {
            "Create qubes"
        } else {
            "Next"
        }
    }

    /// Show `page`, focused on its last button (Next, or the one that
    /// finishes).
    fn go(&mut self, page: Page) {
        self.page = page;
        self.focus = self.widgets().len().saturating_sub(1);
        self.error = None;
    }

    fn back(&mut self) {
        let prev = match self.page {
            Page::Network => Some(Page::Welcome),
            Page::Qubes => Some(Page::Network),
            Page::Summary => Some(Page::Qubes),
            _ => None,
        };
        if let Some(p) = prev {
            self.go(p);
        }
    }

    fn forward(&mut self) {
        let next = match self.page {
            Page::Welcome => Page::Network,
            Page::Network => Page::Qubes,
            _ => Page::Summary,
        };
        self.go(next);
    }

    fn focus_by(&mut self, delta: isize) {
        let n = self.widgets().len() as isize;
        if n > 0 {
            self.focus = (self.focus as isize + delta).rem_euclid(n) as usize;
        }
    }

    fn focus_on(&mut self, w: Widget) {
        if let Some(i) = self.widgets().iter().position(|x| *x == w) {
            self.focus = i;
        }
    }

    /// Up / Down: the cursor of a list, else the focus.
    fn arrow(&mut self, delta: isize) {
        let moved = |i: usize, len: usize| (i as isize + delta).clamp(0, len as isize - 1) as usize;
        match self.focused() {
            Some(Widget::Ifaces) => {
                self.iface = Some(moved(self.iface.unwrap_or(0), self.ifaces.len()));
            }
            Some(Widget::Apps) => self.app = moved(self.app, self.apps.len()),
            Some(Widget::Templates) => self.template = moved(self.template, self.templates.len()),
            _ => self.focus_by(delta),
        }
    }

    /// Why Next is refused (only the Network page refuses it).
    fn blocker(&self) -> &'static str {
        if self.ifaces.is_empty() {
            "No network card: plug in a USB Ethernet adapter, then Rescan."
        } else {
            "Wi-Fi cannot be bridged to sys-net yet: pick an Ethernet card."
        }
    }

    pub fn handle(&mut self, key: Key) -> Action {
        self.error = None;
        match key {
            Key::Tab => self.focus_by(1),
            Key::BackTab => self.focus_by(-1),
            Key::Up => self.arrow(-1),
            Key::Down => self.arrow(1),
            Key::Esc => self.back(),
            Key::Char(' ') => return self.press(),
            Key::Enter => {
                let w = self
                    .focused()
                    .filter(|w| w.button_label().is_some() || *w == Widget::Rescan)
                    .unwrap_or(Widget::Next);
                return self.activate(w);
            }
            _ => {}
        }
        Action::None
    }

    /// Space: tick what is focused, or press it.
    fn press(&mut self) -> Action {
        match self.focused() {
            Some(w) => self.activate(w),
            None => Action::None,
        }
    }

    pub fn click(&mut self, target: Target) -> Action {
        self.error = None;
        match target {
            Target::Widget(w) => {
                self.focus_on(w);
                return self.activate(w);
            }
            Target::Iface(i) => {
                self.focus_on(Widget::Ifaces);
                self.iface = Some(i);
            }
            Target::App(i) => {
                self.focus_on(Widget::Apps);
                self.app = i;
                self.toggle_app();
            }
            Target::Template(i) => {
                self.focus_on(Widget::Templates);
                self.template = i;
            }
        }
        Action::None
    }

    fn toggle_app(&mut self) {
        if let Some((_, on)) = self.apps.get_mut(self.app) {
            *on = !*on;
        }
    }

    fn activate(&mut self, w: Widget) -> Action {
        if !self.widgets().contains(&w) {
            return Action::None;
        }
        match w {
            Widget::Next if !self.can_advance() => self.error = Some(self.blocker().to_string()),
            Widget::Next if self.page == Page::Summary => return self.start(),
            Widget::Next => self.forward(),
            Widget::Retry => return self.start(),
            Widget::Back => self.back(),
            Widget::Rescan => return Action::Rescan,
            Widget::Skip | Widget::Finish => return Action::Leave(Outcome::Done),
            Widget::Reboot => return Action::Leave(Outcome::Reboot),
            Widget::Later => return Action::Leave(Outcome::Later),
            Widget::Passthrough => self.passthrough = !self.passthrough,
            Widget::Gpu => self.gpu = !self.gpu,
            Widget::Apps => self.toggle_app(),
            Widget::Ifaces | Widget::Templates => {}
        }
        Action::None
    }

    fn start(&mut self) -> Action {
        self.run = RunState::Running {
            index: 0,
            total: 0,
            step: "Planning".to_string(),
        };
        self.log.clear();
        self.detail.clear();
        self.reboot = false;
        self.go(Page::Progress);
        Action::Start(self.setup_args())
    }

    /// The `jos` command line of the choices.
    pub fn setup_args(&self) -> Vec<String> {
        let mut a = vec!["setup".to_string()];
        if let Some(i) = self.selected_iface() {
            a.extend(["--uplink".to_string(), i.name.clone()]);
        }
        if !self.passes_through() {
            a.push("--no-passthrough".to_string());
        }
        if !self.gpu {
            a.push("--no-gpu".to_string());
        }
        for (app, _) in self.apps.iter().filter(|(_, on)| *on) {
            a.extend(["--app".to_string(), app.to_string()]);
        }
        if let Some(t) = self.templates.get(self.template) {
            a.extend(["--app-template".to_string(), t.name.to_string()]);
        }
        a
    }

    /// The Summary page: what each qube will be.
    pub fn summary_rows(&self) -> Vec<(&'static str, String)> {
        let net = match self.selected_iface() {
            Some(NetIface {
                name,
                kind: IfaceKind::Pci(addr),
                ..
            }) if self.passes_through() => {
                format!("owns {name}'s card ({addr}), after a reboot")
            }
            Some(i) => format!("bridged to {} ({})", i.name, i.description),
            None => "no network card".to_string(),
        };
        let apps: Vec<String> = self
            .apps
            .iter()
            .filter(|(_, on)| *on)
            .map(|(a, _)| a.to_string())
            .collect();
        let apps = if apps.is_empty() {
            "none".to_string()
        } else {
            apps.join(", ")
        };
        let template = match self.templates.get(self.template) {
            Some(t) if t.local => t.name.to_string(),
            Some(t) => format!("{} (its image is not on this disk)", t.name),
            None => "none".to_string(),
        };
        let gpu = if self.gpu {
            "if an NVIDIA card is reserved"
        } else {
            "no"
        };
        vec![
            ("sys-net", net),
            (
                "sys-firewall",
                "between sys-net and the app qubes".to_string(),
            ),
            ("App qubes", apps),
            ("Template", template),
            ("gpu qube", gpu.to_string()),
            ("Command", format!("jos {}", self.setup_args().join(" "))),
        ]
    }

    /// A line `jos setup` printed.
    pub fn on_line(&mut self, line: &str) {
        if let Some((index, total, step)) = setup::parse_progress(line) {
            self.done_step();
            self.run = RunState::Running {
                index,
                total,
                step: step.to_string(),
            };
            self.detail.clear();
            return;
        }
        if line.starts_with("reboot ") {
            self.reboot = true;
        }
        let line = line.trim();
        if !line.is_empty() {
            self.detail = line.to_string();
        }
    }

    /// The step in progress went into the log.
    fn done_step(&mut self) {
        if let RunState::Running { index, step, .. } = &self.run {
            if *index > 0 {
                self.log.push(step.clone());
            }
        }
    }

    pub fn finished(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.done_step();
                self.run = RunState::Succeeded;
            }
            Err(e) => self.run = RunState::Failed(e),
        }
        self.go(Page::Done);
    }

    pub fn progress_permille(&self) -> u32 {
        match &self.run {
            RunState::Running { index, total, .. } if *total > 0 => {
                (index.saturating_sub(1) * 1000 / total) as u32
            }
            RunState::Succeeded => 1000,
            _ => 0,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use juanitos_core::pci::PciAddr;

    pub(crate) fn nic(name: &str, kind: IfaceKind, link: Option<bool>) -> NetIface {
        NetIface {
            name: name.into(),
            mac: Some("00:a0:98:00:00:01".into()),
            link,
            kind,
            description: match kind {
                IfaceKind::Usb => "USB Ethernet adapter",
                IfaceKind::Wifi => "Wi-Fi (not supported as an uplink yet)",
                IfaceKind::Pci(_) => "PCI Ethernet controller",
                IfaceKind::Other => "Ethernet",
            }
            .into(),
        }
    }

    pub(crate) fn sample_ifaces() -> Vec<NetIface> {
        vec![
            nic("em0", IfaceKind::Pci(PciAddr::new(0, 31, 6)), Some(false)),
            nic("ue0", IfaceKind::Usb, Some(true)),
            nic("wlan0", IfaceKind::Wifi, None),
        ]
    }

    fn local() -> Vec<String> {
        vec!["fbsd15-dev".into(), "fbsd15-net".into()]
    }

    fn keys(w: &mut Wizard, keys: &[Key]) -> Vec<Action> {
        keys.iter().map(|k| w.handle(*k)).collect()
    }

    #[test]
    fn labels() {
        let labels: Vec<&str> = PAGES.iter().map(|p| p.label()).collect();
        assert_eq!(
            labels,
            ["Welcome", "Network", "Qubes", "Summary", "Creating", "Done"]
        );
        assert_eq!(Widget::Finish.button_label(), Some("Log in"));
        assert_eq!(Widget::Apps.button_label(), None);
    }

    #[test]
    fn the_keyboard_walks_to_the_setup() {
        let mut w = Wizard::new(sample_ifaces(), false, &local());
        // The USB adapter with a cable is preselected; the fbsd15 template.
        assert_eq!(w.selected_iface().unwrap().name, "ue0");
        assert_eq!(w.templates[w.template].name, "fbsd15-dev");
        assert_eq!(w.focused(), Some(Widget::Next));
        assert_eq!(w.next_label(), "Next");
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Network);
        // No IOMMU: no passthrough tick.
        assert_eq!(
            w.widgets(),
            [Widget::Ifaces, Widget::Rescan, Widget::Back, Widget::Next]
        );
        // Tab wraps around to the list, Shift+Tab back; Up picks em0.
        w.handle(Key::Tab);
        assert_eq!(w.focused(), Some(Widget::Ifaces));
        keys(&mut w, &[Key::BackTab, Key::Tab]);
        assert_eq!(w.focused(), Some(Widget::Ifaces));
        keys(&mut w, &[Key::Up, Key::Up]);
        assert_eq!(w.selected_iface().unwrap().name, "em0");
        // Down past the end stays on the last card: Wi-Fi is refused.
        keys(&mut w, &[Key::Down, Key::Down, Key::Down]);
        assert_eq!(w.selected_iface().unwrap().name, "wlan0");
        assert!(!w.can_advance());
        assert_eq!(w.handle(Key::Enter), Action::None);
        assert_eq!(w.page, Page::Network);
        assert!(w.error.as_deref().unwrap().contains("Wi-Fi"));
        // Any key clears the message.
        w.handle(Key::Char('x'));
        assert_eq!(w.error, None);
        w.handle(Key::Up);
        assert_eq!(w.handle(Key::Enter), Action::None);
        assert_eq!(w.page, Page::Qubes);
        // Untick personal, pick the Debian template, no gpu qube.
        assert_eq!(w.focused(), Some(Widget::Next));
        keys(&mut w, &[Key::Tab, Key::Down, Key::Char(' ')]);
        assert!(!w.apps[1].1);
        keys(&mut w, &[Key::Tab, Key::Up, Key::Tab, Key::Char(' ')]);
        assert_eq!(w.templates[w.template].name, "debian-13-dev");
        assert!(!w.gpu);
        // Space on the template list does nothing more.
        keys(&mut w, &[Key::BackTab, Key::Char(' ')]);
        assert_eq!(w.templates[w.template].name, "debian-13-dev");
        w.handle(Key::Enter);
        assert_eq!(w.page, Page::Summary);
        assert_eq!(w.next_label(), "Create qubes");
        let rows = w.summary_rows();
        assert_eq!(
            rows[0].1, "bridged to ue0 (USB Ethernet adapter)",
            "{rows:?}"
        );
        assert_eq!(rows[2].1, "work, untrusted, vault:offline");
        assert_eq!(rows[3].1, "debian-13-dev (its image is not on this disk)");
        assert_eq!(rows[4].1, "no");
        let args = "setup --uplink ue0 --no-passthrough --no-gpu --app work \
                    --app untrusted --app vault:offline --app-template debian-13-dev";
        assert_eq!(rows[5].1, format!("jos {args}"));
        // Esc goes back, Enter comes again; Space presses Create qubes.
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Qubes);
        w.handle(Key::Enter);
        let expected: Vec<String> = args.split(' ').map(String::from).collect();
        assert_eq!(w.handle(Key::Char(' ')), Action::Start(expected));
        assert_eq!(w.page, Page::Progress);
        assert_eq!(w.focused(), None);
        // Nothing to press while it runs.
        assert_eq!(
            keys(
                &mut w,
                &[Key::Enter, Key::Char(' '), Key::Esc, Key::Tab, Key::Down]
            ),
            vec![Action::None; 5]
        );
        assert_eq!(w.page, Page::Progress);
    }

    #[test]
    fn skip_back_and_the_welcome_page() {
        let mut w = Wizard::new(sample_ifaces(), false, &local());
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Welcome);
        w.handle(Key::BackTab);
        assert_eq!(w.handle(Key::Enter), Action::Leave(Outcome::Done));
        // Clicking Back where there is none does nothing.
        assert_eq!(w.click(Target::Widget(Widget::Back)), Action::None);
        assert_eq!(w.page, Page::Welcome);
        w.page = Page::Qubes;
        w.handle(Key::Esc);
        assert_eq!(w.page, Page::Network);
    }

    #[test]
    fn passthrough_needs_an_iommu_and_a_pci_card() {
        let mut w = Wizard::new(sample_ifaces(), true, &local());
        w.click(Target::Widget(Widget::Next));
        assert!(!w.can_passthrough());
        w.click(Target::Iface(0));
        assert_eq!(w.focused(), Some(Widget::Ifaces));
        assert!(w.passes_through());
        assert_eq!(
            w.widgets(),
            [
                Widget::Ifaces,
                Widget::Rescan,
                Widget::Passthrough,
                Widget::Back,
                Widget::Next
            ]
        );
        assert_eq!(
            w.summary_rows()[0].1,
            "owns em0's card (0/31/6), after a reboot"
        );
        assert!(w
            .setup_args()
            .join(" ")
            .starts_with("setup --uplink em0 --app"));
        w.click(Target::Widget(Widget::Passthrough));
        assert!(!w.passes_through());
        assert_eq!(
            w.summary_rows()[0].1,
            "bridged to em0 (PCI Ethernet controller)"
        );
        assert_eq!(w.click(Target::Widget(Widget::Rescan)), Action::Rescan);
        // A rescan keeps em0 selected, even at another place.
        let mut again = sample_ifaces();
        again.insert(0, nic("axe0", IfaceKind::Other, None));
        w.set_ifaces(again);
        assert_eq!(w.selected_iface().unwrap().name, "em0");
        assert_eq!(w.focused(), Some(Widget::Rescan));
        // em0 is gone: the preferred card instead, and the focused tick
        // went with it.
        w.click(Target::Widget(Widget::Passthrough));
        w.set_ifaces(vec![nic("ue1", IfaceKind::Usb, None)]);
        assert_eq!(w.selected_iface().unwrap().name, "ue1");
        assert_eq!(w.focused(), Some(Widget::Back));
    }

    #[test]
    fn no_card_until_a_rescan_finds_one() {
        let mut w = Wizard::new(Vec::new(), false, &local());
        w.handle(Key::Enter);
        assert_eq!(w.widgets(), [Widget::Rescan, Widget::Back, Widget::Next]);
        assert_eq!(w.handle(Key::Enter), Action::None);
        assert!(w.error.as_deref().unwrap().contains("USB Ethernet"));
        assert_eq!(w.summary_rows()[0].1, "no network card");
        assert!(!w.setup_args().contains(&"--uplink".to_string()));
        w.handle(Key::BackTab);
        w.handle(Key::BackTab);
        assert_eq!(w.handle(Key::Enter), Action::Rescan);
        w.set_ifaces(vec![nic("ue0", IfaceKind::Usb, Some(true))]);
        assert_eq!(w.focused(), Some(Widget::Rescan));
        assert!(w.can_advance());
        w.click(Target::Widget(Widget::Back));
        assert_eq!(w.page, Page::Welcome);
    }

    #[test]
    fn clicks_tick_and_pick() {
        let mut w = Wizard::new(sample_ifaces(), false, &local());
        w.page = Page::Qubes;
        for i in 0..4 {
            w.click(Target::App(i));
        }
        assert_eq!(w.focused(), Some(Widget::Apps));
        assert_eq!(w.summary_rows()[2].1, "none");
        w.click(Target::Template(1));
        assert_eq!(w.focused(), Some(Widget::Templates));
        assert_eq!(w.summary_rows()[3].1, "fbsd15-dev");
        assert_eq!(w.summary_rows()[4].1, "if an NVIDIA card is reserved");
        w.click(Target::Widget(Widget::Gpu));
        assert!(!w.gpu);
        w.click(Target::Widget(Widget::Apps));
        assert!(w.apps[3].1);
        // No app under the cursor: nothing to tick.
        w.app = w.apps.len();
        w.click(Target::Widget(Widget::Apps));
        assert!(w.apps.iter().filter(|(_, on)| *on).count() == 1);
        // A template list without a selection (no recipe found).
        w.templates.clear();
        assert_eq!(w.summary_rows()[3].1, "none");
        assert!(!w.setup_args().contains(&"--app-template".to_string()));
    }

    #[test]
    fn progress_follows_the_setup_lines() {
        let mut w = Wizard::new(sample_ifaces(), false, &local());
        w.page = Page::Summary;
        assert_eq!(w.handle(Key::Enter), Action::Start(w.setup_args()));
        assert_eq!(w.progress_permille(), 0);
        // A rescan while nothing has the focus keeps it that way.
        w.set_ifaces(sample_ifaces());
        assert_eq!(w.focused(), None);
        w.on_line("- sys-net exists");
        assert_eq!(w.detail, "- sys-net exists");
        w.on_line("==> [1/4] create sys-firewall");
        assert_eq!(w.detail, "");
        assert_eq!(w.progress_permille(), 0);
        w.on_line("   ");
        w.on_line("==> [2/4] start sys-firewall");
        assert_eq!(w.progress_permille(), 250);
        assert_eq!(w.log, ["create sys-firewall"]);
        w.on_line("reboot to hand the reserved devices to their qubes");
        assert!(w.reboot);
        w.finished(Ok(()));
        assert_eq!(w.log, ["create sys-firewall", "start sys-firewall"]);
        assert_eq!(w.progress_permille(), 1000);
        assert_eq!(w.page, Page::Done);
        assert_eq!(w.widgets(), [Widget::Finish, Widget::Reboot]);
        assert_eq!(w.handle(Key::Enter), Action::Leave(Outcome::Reboot));
        w.handle(Key::Tab);
        assert_eq!(w.handle(Key::Enter), Action::Leave(Outcome::Done));
        w.reboot = false;
        assert_eq!(w.widgets(), [Widget::Finish]);
    }

    #[test]
    fn a_failed_setup_can_be_retried_or_left() {
        let mut w = Wizard::new(sample_ifaces(), false, &local());
        w.page = Page::Summary;
        w.handle(Key::Enter);
        // Nothing to do: no step at all.
        w.finished(Ok(()));
        assert!(w.log.is_empty());
        w.run = RunState::Running {
            index: 0,
            total: 0,
            step: String::new(),
        };
        w.finished(Err("sys-net: no image".into()));
        assert_eq!(w.run, RunState::Failed("sys-net: no image".into()));
        assert_eq!(w.progress_permille(), 0);
        assert_eq!(w.focused(), Some(Widget::Retry));
        w.handle(Key::Tab);
        assert_eq!(w.handle(Key::Char(' ')), Action::Leave(Outcome::Later));
        assert_eq!(
            w.click(Target::Widget(Widget::Retry)),
            Action::Start(w.setup_args())
        );
        assert_eq!(w.page, Page::Progress);
    }
}
