// SPDX-License-Identifier: BSD-3-Clause
//! Keyboard layouts: the console keymaps `vt(4)` ships, which the
//! installer offers and loads (`kbdcontrol -l`) and the installed system
//! sets at boot (`keymap=` in `rc.conf`, applied by `rc.d/syscons`), and
//! the XKB layout the desktop is told to use for the same keyboard.
//!
//! The kernel's built-in keymap is `us`: a password typed on any other
//! keyboard without a keymap loaded is not what its keys say.

use crate::sys::Runner;
use crate::Result;

/// `kbdmap(1)`'s index of the keymaps: `file.kbd:lang:description`, one
/// line per language.
pub const INDEX_PATH: &str = "/usr/share/vt/keymaps/INDEX.keymaps";

/// The keymap compiled into the kernel, in effect until another is
/// loaded.
pub const DEFAULT_KEYMAP: &str = "us";

/// The terminal `kbdcontrol` is pointed at: the keymap belongs to the
/// keyboard, which every terminal shares, so the first one does
/// (`rc.d/syscons` uses it too).
pub const CONSOLE_TTY: &str = "/dev/ttyv0";

/// A console keymap: its name as `kbdcontrol -l` and `rc.conf` take it
/// (`fr.macbook`) and the index's English description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    pub name: String,
    pub description: String,
}

impl Keymap {
    pub fn new(name: &str, description: &str) -> Self {
        Keymap {
            name: name.to_string(),
            description: description.to_string(),
        }
    }
}

/// The keymaps `INDEX.keymaps` describes in English (lines without a
/// language are English too), sorted by description. `MENU` and `FONT`
/// lines are kbdmap(1)'s own.
pub fn parse_index(text: &str) -> Vec<Keymap> {
    let mut maps: Vec<Keymap> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.splitn(3, ':');
        let (Some(file), Some(lang), Some(description)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(name) = file.strip_suffix(".kbd") else {
            continue;
        };
        let english = lang.is_empty() || lang.split(',').any(|l| l == "en");
        if !english || maps.iter().any(|m| m.name == name) {
            continue;
        }
        maps.push(Keymap::new(name, description.trim()));
    }
    maps.sort_by_key(|m| m.description.to_lowercase());
    maps
}

/// [`parse_index`], with the kernel's default keymap first when the
/// index does not describe it (or there is no index at all): there is
/// always something to pick, and something to go back to.
pub fn keymaps(index: &str) -> Vec<Keymap> {
    let mut maps = parse_index(index);
    if !maps.iter().any(|m| m.name == DEFAULT_KEYMAP) {
        maps.insert(0, Keymap::new(DEFAULT_KEYMAP, "United States of America"));
    }
    maps
}

/// Load `keymap` into the console keyboard, as `rc.d/syscons` does at
/// boot: `kbdcontrol` acts on the terminal on its standard input.
pub fn load(runner: &dyn Runner, keymap: &str) -> Result<()> {
    runner
        .run_on_tty("kbdcontrol", &["-l", keymap], CONSOLE_TTY)
        .and_then(|out| {
            if out.success() {
                Ok(())
            } else {
                Err(crate::Error::Command {
                    program: format!("kbdcontrol -l {keymap}"),
                    code: out.status,
                    stderr: out.stderr.trim().to_string(),
                })
            }
        })
}

/// An XKB layout and variant (`fr`, `mac`), what Wayland compositors
/// take through `XKB_DEFAULT_LAYOUT` and `XKB_DEFAULT_VARIANT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xkb {
    pub layout: String,
    pub variant: String,
}

impl Xkb {
    /// The environment a session gets so its compositor uses this layout
    /// unless the user configured another in the desktop's settings
    /// (libxkbcommon reads these when it is given no names).
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = vec![("XKB_DEFAULT_LAYOUT".to_string(), self.layout.clone())];
        if !self.variant.is_empty() {
            env.push(("XKB_DEFAULT_VARIANT".to_string(), self.variant.clone()));
        }
        env
    }
}

/// The XKB layout closest to a console keymap (`fr.macbook` is `fr` with
/// the `mac` variant), or `None` for the few that have no XKB
/// counterpart: the keymap's first dotted component names the country,
/// the others its flavour. Accent ("acc") keymaps are the plain XKB
/// layouts, which have dead keys; "noacc" is `nodeadkeys`.
pub fn xkb_for(keymap: &str) -> Option<Xkb> {
    let name = keymap.strip_suffix(".kbd").unwrap_or(keymap);
    let mut parts = name.split('.');
    let (layout, mut variant) = match parts.next().unwrap_or("") {
        "" | "centraleuropean" | "nordic" => return None,
        "uk" => ("gb", ""),
        "latinamerican" => ("latam", ""),
        "ch-fr" => ("ch", "fr"),
        "ca-fr" => ("ca", ""),
        "ca-multi" => ("ca", "multix"),
        "colemak" => ("us", "colemak"),
        "colemak-dh" => ("us", "colemak_dh"),
        base => (base, ""),
    };
    for part in parts {
        let found = match part {
            "dvorakl" => "dvorak-l",
            "dvorakr" => "dvorak-r",
            "dvorakp" => "dvp",
            p if p.starts_with("dvorak") => "dvorak",
            "macbook" if layout == "ch" => "de_mac",
            "macbook" => "mac",
            "noacc" => "nodeadkeys",
            "phonetic" | "bds" | "f" | "bepo" | "intl" => part,
            "win" => "winkeys",
            _ => continue,
        };
        variant = found;
        break;
    }
    Some(Xkb {
        layout: layout.to_string(),
        variant: variant.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::{CmdOutput, MockRunner};

    /// A cut of `/usr/share/vt/keymaps/INDEX.keymaps`.
    pub const SAMPLE_INDEX: &str = "\
# database for kbdmap(1)
#
# Format <file>:<lang>:<description>

# Language support: MENU, FONT
MENU:en:Choose your keyboard layout
MENU:de:Wählen Sie Ihre Tastaturbelegung
FONT:en:vgarom-8x16.fnt

be.kbd:en:Belgian
be.kbd:de:Belgisch
be.acc.kbd:en:Belgian (accent keys)
br.kbd:en:Brazilian (accent keys)
br.kbd:pt,pt_BR:Brasileiro (com acentos)
fr.kbd:en:French
fr.kbd:fr:Français
fr.macbook.kbd:en:French (MacBook/MacBook Pro) (accent keys)
fr.dvorak.kbd:en:French Dvorak-like
uk.kbd:en:United Kingdom
us.kbd:en:United States of America
us.kbd:de:US-amerikanisch
us.dvorak.kbd:en:United States of America dvorak
us.macbook.kbd:en:United States of America (MacBook/MacBook Pro) ISO-8859-1
fr.kbd::French again
malformed line
nofile:en:no keymap file
";

    #[test]
    fn the_index_gives_the_english_names_sorted() {
        let maps = parse_index(SAMPLE_INDEX);
        let names: Vec<&str> = maps.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "be",
                "be.acc",
                "br",
                "fr",
                "fr.macbook",
                "fr.dvorak",
                "uk",
                "us",
                "us.macbook",
                "us.dvorak"
            ]
        );
        let fr = maps.iter().find(|m| m.name == "fr").unwrap();
        assert_eq!(fr.description, "French", "the first English line wins");
        assert_eq!(
            maps.iter().find(|m| m.name == "br").unwrap().description,
            "Brazilian (accent keys)"
        );
        assert_eq!(parse_index(""), Vec::<Keymap>::new());
    }

    #[test]
    fn the_default_keymap_is_always_offered() {
        let maps = keymaps("");
        assert_eq!(maps, [Keymap::new("us", "United States of America")]);
        let maps = keymaps("fr.kbd:en:French\n");
        assert_eq!(maps[0].name, "us");
        assert_eq!(maps[1].name, "fr");
        let maps = keymaps(SAMPLE_INDEX);
        assert_eq!(maps.iter().filter(|m| m.name == "us").count(), 1);
        assert_eq!(maps.len(), 10);
    }

    #[test]
    fn loading_goes_through_the_console_terminal() {
        let m = MockRunner::new();
        load(&m, "fr.macbook").unwrap();
        assert_eq!(m.calls(), ["kbdcontrol -l fr.macbook"]);
        let m = MockRunner::new().respond(
            "kbdcontrol",
            &["-l", "xx"],
            CmdOutput::failed(1, "kbdcontrol: keymap file not found\n"),
        );
        let err = load(&m, "xx").unwrap_err().to_string();
        assert!(err.contains("kbdcontrol -l xx"), "{err}");
        assert!(err.contains("not found"), "{err}");
        let m = MockRunner::new().missing("kbdcontrol");
        assert!(load(&m, "us").is_err());
    }

    #[test]
    fn xkb_layouts_follow_the_keymap_names() {
        let xkb = |layout: &str, variant: &str| {
            Some(Xkb {
                layout: layout.into(),
                variant: variant.into(),
            })
        };
        assert_eq!(xkb_for("us"), xkb("us", ""));
        assert_eq!(xkb_for("us.kbd"), xkb("us", ""));
        assert_eq!(xkb_for("fr.macbook"), xkb("fr", "mac"));
        assert_eq!(xkb_for("fr.acc"), xkb("fr", ""));
        assert_eq!(xkb_for("fr.dvorak.acc"), xkb("fr", "dvorak"));
        assert_eq!(xkb_for("uk.macbook"), xkb("gb", "mac"));
        assert_eq!(xkb_for("uk.capsctrl"), xkb("gb", ""));
        assert_eq!(xkb_for("ch.macbook.acc"), xkb("ch", "de_mac"));
        assert_eq!(xkb_for("ch-fr.acc"), xkb("ch", "fr"));
        assert_eq!(xkb_for("ca-fr"), xkb("ca", ""));
        assert_eq!(xkb_for("de.noacc"), xkb("de", "nodeadkeys"));
        assert_eq!(xkb_for("us.dvorakl"), xkb("us", "dvorak-l"));
        assert_eq!(xkb_for("us.dvorakr"), xkb("us", "dvorak-r"));
        assert_eq!(xkb_for("us.dvorakp"), xkb("us", "dvp"));
        assert_eq!(xkb_for("colemak.acc"), xkb("us", "colemak"));
        assert_eq!(xkb_for("colemak-dh.acc"), xkb("us", "colemak_dh"));
        assert_eq!(xkb_for("ca-multi"), xkb("ca", "multix"));
        assert_eq!(xkb_for("fr.bepo"), xkb("fr", "bepo"));
        assert_eq!(xkb_for("us.intl.acc"), xkb("us", "intl"));
        assert_eq!(xkb_for("us.dvorakx"), xkb("us", "dvorak"));
        assert_eq!(xkb_for("latinamerican.acc"), xkb("latam", ""));
        assert_eq!(xkb_for("bg.phonetic"), xkb("bg", "phonetic"));
        assert_eq!(xkb_for("tr.f"), xkb("tr", "f"));
        assert_eq!(xkb_for("ru.win"), xkb("ru", "winkeys"));
        assert_eq!(xkb_for("centraleuropean.qwerty"), None);
        assert_eq!(xkb_for("nordic.asus-eee"), None);
        assert_eq!(xkb_for(""), None);
    }

    #[test]
    fn xkb_environment() {
        let fr = xkb_for("fr.macbook").unwrap();
        assert_eq!(
            fr.env(),
            [
                ("XKB_DEFAULT_LAYOUT".to_string(), "fr".to_string()),
                ("XKB_DEFAULT_VARIANT".to_string(), "mac".to_string())
            ]
        );
        let us = xkb_for("us").unwrap();
        assert_eq!(
            us.env(),
            [("XKB_DEFAULT_LAYOUT".to_string(), "us".to_string())]
        );
    }
}
