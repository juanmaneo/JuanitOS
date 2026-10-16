//! Desktop sessions the greeter offers: the `.desktop` files in
//! `/usr/local/share/wayland-sessions` (COSMIC installs `cosmic.desktop`
//! there), as display managers read them.

use std::path::Path;

pub const SESSIONS_DIR: &str = "/usr/local/share/wayland-sessions";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// `Name=`: what the greeter shows.
    pub name: String,
    /// `Exec=` split into words, field codes (`%f`, ...) dropped.
    pub exec: Vec<String>,
    /// `DesktopNames=` (first entry) for `XDG_CURRENT_DESKTOP`.
    pub desktop: String,
}

impl Session {
    /// What a dom0 runs when no session file is installed (yet).
    pub fn cosmic_default() -> Self {
        Session {
            name: "COSMIC".to_string(),
            exec: vec!["/usr/local/bin/start-cosmic".to_string()],
            desktop: "COSMIC".to_string(),
        }
    }
}

/// Parse the `[Desktop Entry]` group of a `.desktop` file. `None` for
/// entries a display manager must skip (`Hidden`, `NoDisplay`, no `Exec`).
pub fn parse_desktop(text: &str) -> Option<Session> {
    let mut in_entry = false;
    let mut name = None;
    let mut exec = None;
    let mut desktop = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "DesktopNames" => desktop = value.split(';').next().map(str::to_string),
            "Hidden" | "NoDisplay" if value == "true" => return None,
            _ => {}
        }
    }
    let exec: Vec<String> = exec?
        .split_whitespace()
        .filter(|w| !w.starts_with('%'))
        .map(str::to_string)
        .collect();
    if exec.is_empty() {
        return None;
    }
    let name = name.unwrap_or_else(|| exec.first().cloned().unwrap_or_default());
    let desktop = desktop
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| name.clone());
    Some(Session {
        name,
        exec,
        desktop,
    })
}

/// Every usable session in `dir`, sorted by file name; the COSMIC default
/// when there is none, so the greeter always has something to start.
pub fn discover(dir: &Path) -> Vec<Session> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let sessions: Vec<Session> = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|t| parse_desktop(&t))
        .collect();
    if sessions.is_empty() {
        vec![Session::cosmic_default()]
    } else {
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COSMIC: &str = "[Desktop Entry]\nName=COSMIC\nComment=This session logs you into the COSMIC desktop\n\
        Comment[sv]=Denna session\nExec=/usr/local/bin/start-cosmic\nType=Application\nDesktopNames=COSMIC\n";

    #[test]
    fn parses_cosmic_desktop_file() {
        assert_eq!(parse_desktop(COSMIC), Some(Session::cosmic_default()));
    }

    #[test]
    fn field_codes_other_groups_and_defaults() {
        let s = parse_desktop(
            "# c\n[Desktop Entry]\n# comment\nbogus line\nExec = sway --unsupported-gpu %U\n\
             [Desktop Action x]\nName=Not me\n",
        )
        .unwrap();
        assert_eq!(s.exec, vec!["sway", "--unsupported-gpu"]);
        assert_eq!(s.name, "sway");
        assert_eq!(s.desktop, "sway");
        let s = parse_desktop("[Desktop Entry]\nName=Foo\nExec=foo\nDesktopNames=A;B;\n").unwrap();
        assert_eq!(s.desktop, "A");
        let s = parse_desktop("[Desktop Entry]\nName=Foo\nExec=foo\nDesktopNames=\n").unwrap();
        assert_eq!(s.desktop, "Foo");
    }

    #[test]
    fn hidden_and_broken_entries_are_skipped() {
        assert_eq!(
            parse_desktop("[Desktop Entry]\nName=X\nExec=x\nHidden=true\n"),
            None
        );
        assert_eq!(
            parse_desktop("[Desktop Entry]\nName=X\nExec=x\nNoDisplay=true\n"),
            None
        );
        assert!(parse_desktop("[Desktop Entry]\nName=X\nExec=x\nNoDisplay=false\n").is_some());
        assert_eq!(parse_desktop("[Desktop Entry]\nName=X\n"), None);
        assert_eq!(parse_desktop("[Desktop Entry]\nExec=%f\n"), None);
        assert_eq!(parse_desktop("[Other]\nExec=x\n"), None);
    }

    #[test]
    fn discovers_sorted_sessions_or_falls_back() {
        let dir = std::env::temp_dir().join(format!("jos-greet-sessions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(discover(&dir), vec![Session::cosmic_default()]);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.desktop"), COSMIC).unwrap();
        std::fs::write(
            dir.join("a.desktop"),
            "[Desktop Entry]\nName=Sway\nExec=sway\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("c.desktop"),
            "[Desktop Entry]\nName=Gone\nHidden=true\n",
        )
        .unwrap();
        std::fs::write(dir.join("readme.txt"), COSMIC).unwrap();
        let names: Vec<String> = discover(&dir).into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["Sway", "COSMIC"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
