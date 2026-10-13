//! Line coverage per crate from an LCOV report, and the ratcheting gate.
//!
//! The floors live in `coverage.toml` at the repository root. A run fails
//! when the total or any crate is below its floor; `--ratchet` raises the
//! floors to what was measured (rounded down to 0.1 %, never lowered), so
//! coverage can only go up, one commit at a time.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Lines found / hit for one crate (or the total).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lines {
    pub found: u64,
    pub hit: u64,
}

impl Lines {
    pub fn percent(self) -> f64 {
        if self.found == 0 {
            100.0
        } else {
            self.hit as f64 * 100.0 / self.found as f64
        }
    }

    fn add(&mut self, other: Lines) {
        self.found += other.found;
        self.hit += other.hit;
    }
}

/// Crate name of a source file: the directory after `crates/` (or
/// `xtask`), `None` for anything else (dependencies, generated code).
pub fn crate_of(path: &str) -> Option<String> {
    let path = path.replace('\\', "/");
    if let Some(rest) = path.rsplit_once("/crates/").map(|(_, r)| r) {
        return rest.split('/').next().map(String::from);
    }
    path.contains("/xtask/src/").then(|| "xtask".to_string())
}

/// Sum `LF`/`LH` records of an LCOV file per crate.
pub fn parse_lcov(text: &str) -> BTreeMap<String, Lines> {
    let mut per_crate: BTreeMap<String, Lines> = BTreeMap::new();
    let mut current: Option<String> = None;
    let mut lines = Lines::default();
    for line in text.lines() {
        if let Some(path) = line.strip_prefix("SF:") {
            current = crate_of(path);
            lines = Lines::default();
        } else if let Some(n) = line.strip_prefix("LF:") {
            lines.found = n.trim().parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix("LH:") {
            lines.hit = n.trim().parse().unwrap_or(0);
        } else if line == "end_of_record" {
            if let Some(name) = current.take() {
                per_crate.entry(name).or_default().add(lines);
            }
        }
    }
    per_crate
}

pub fn total(per_crate: &BTreeMap<String, Lines>) -> Lines {
    let mut t = Lines::default();
    for l in per_crate.values() {
        t.add(*l);
    }
    t
}

/// The gate configuration (`coverage.toml`).
#[derive(Debug, Clone, PartialEq)]
pub struct Floors {
    pub total: f64,
    /// Floor for a crate that has none yet.
    pub new_crate: f64,
    /// Points left below the measured value when ratcheting, so a test
    /// skipped on some host (no OVMF, no Unix sockets) does not fail CI.
    pub slack: f64,
    pub crates: BTreeMap<String, f64>,
}

impl Default for Floors {
    fn default() -> Self {
        Floors {
            total: 0.0,
            new_crate: 60.0,
            slack: 0.5,
            crates: BTreeMap::new(),
        }
    }
}

fn number(v: &toml::Value) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

impl Floors {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let table: toml::Table = text.parse()?;
        let mut f = Floors::default();
        if let Some(gate) = table.get("gate").and_then(|g| g.as_table()) {
            if let Some(v) = gate.get("total").and_then(number) {
                f.total = v;
            }
            if let Some(v) = gate.get("new_crate").and_then(number) {
                f.new_crate = v;
            }
            if let Some(v) = gate.get("slack").and_then(number) {
                f.slack = v;
            }
        }
        if let Some(crates) = table.get("crates").and_then(|c| c.as_table()) {
            for (name, v) in crates {
                let Some(v) = number(v) else {
                    anyhow::bail!("coverage.toml: crates.{name} must be a number");
                };
                f.crates.insert(name.clone(), v);
            }
        }
        Ok(f)
    }

    pub fn render(&self) -> String {
        let mut s = String::from(
            "# Line-coverage floors (%) checked by `cargo xtask coverage`.\n\
             #\n\
             # The gate fails when the total or a crate drops below its floor.\n\
             # `cargo xtask coverage --ratchet` raises the floors to the measured\n\
             # values (rounded down, never lowered): coverage only goes up.\n\
             # Commit the updated file together with the tests that earned it.\n\n\
             [gate]\n",
        );
        let _ = writeln!(s, "total = {:.1}", self.total);
        let _ = writeln!(s, "# Floor for a crate that is not listed below yet.");
        let _ = writeln!(s, "new_crate = {:.1}", self.new_crate);
        let _ = writeln!(
            s,
            "# Points kept below the measurement when ratcheting (host variance)."
        );
        let _ = writeln!(s, "slack = {:.1}\n", self.slack);
        s.push_str("[crates]\n");
        for (name, v) in &self.crates {
            let _ = writeln!(s, "{} = {:.1}", toml_key(name), v);
        }
        s
    }

    pub fn floor_of(&self, name: &str) -> f64 {
        self.crates.get(name).copied().unwrap_or(self.new_crate)
    }

    /// Raise every floor to the measured value minus the slack (rounded
    /// down to 0.1); floors are never lowered.
    pub fn ratchet(&mut self, per_crate: &BTreeMap<String, Lines>) -> bool {
        let mut changed = false;
        let slack = self.slack;
        let mut raise = |floor: &mut f64, measured: f64| {
            let m = ((measured - slack).max(0.0) * 10.0).floor() / 10.0;
            if m > *floor {
                *floor = m;
                changed = true;
            }
        };
        raise(&mut self.total, total(per_crate).percent());
        for (name, lines) in per_crate {
            let floor = self.crates.entry(name.clone()).or_insert(0.0);
            raise(floor, lines.percent());
        }
        changed
    }
}

fn toml_key(name: &str) -> String {
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        format!("{name:?}")
    }
}

/// One line of the report.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub lines: Lines,
    pub floor: f64,
}

impl Row {
    /// A hair of tolerance so float printing never fails a run.
    pub fn passes(&self) -> bool {
        self.lines.percent() + 0.05 >= self.floor
    }
}

/// Compare a measurement with the floors: the rows (crates then total)
/// and whether everything passes.
pub fn check(per_crate: &BTreeMap<String, Lines>, floors: &Floors) -> (Vec<Row>, bool) {
    let mut rows: Vec<Row> = per_crate
        .iter()
        .map(|(name, lines)| Row {
            name: name.clone(),
            lines: *lines,
            floor: floors.floor_of(name),
        })
        .collect();
    rows.push(Row {
        name: "TOTAL".into(),
        lines: total(per_crate),
        floor: floors.total,
    });
    let ok = rows.iter().all(Row::passes);
    (rows, ok)
}

pub fn render_table(rows: &[Row]) -> String {
    let mut s = format!(
        "{:<16} {:>8} {:>8} {:>8} {:>8}\n",
        "crate", "lines", "hit", "cover", "floor"
    );
    for r in rows {
        let mark = if r.passes() { "" } else { "  <-- below floor" };
        let _ = writeln!(
            s,
            "{:<16} {:>8} {:>8} {:>7.1}% {:>7.1}%{mark}",
            r.name,
            r.lines.found,
            r.lines.hit,
            r.lines.percent(),
            r.floor
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const LCOV: &str = "\
TN:
SF:/home/u/JuanitOS/crates/jos-gfx/src/font.rs
FN:1,f
LF:100
LH:90
end_of_record
SF:/home/u/JuanitOS/crates/jos-gfx/src/png.rs
LF:50
LH:10
end_of_record
SF:C:\\src\\JuanitOS\\crates\\jos\\src\\main.rs
LF:10
LH:5
end_of_record
SF:/home/u/.cargo/registry/src/x/serde/lib.rs
LF:1000
LH:0
end_of_record
SF:/home/u/JuanitOS/xtask/src/main.rs
LF:4
LH:4
end_of_record
";

    #[test]
    fn crate_names_from_paths() {
        assert_eq!(crate_of("/a/crates/jos/src/x.rs").as_deref(), Some("jos"));
        assert_eq!(
            crate_of("C:\\a\\crates\\jos-fb\\src\\x.rs").as_deref(),
            Some("jos-fb")
        );
        assert_eq!(crate_of("/a/xtask/src/main.rs").as_deref(), Some("xtask"));
        assert_eq!(crate_of("/rustc/library/core/src/lib.rs"), None);
    }

    #[test]
    fn lcov_is_summed_per_crate() {
        let m = parse_lcov(LCOV);
        assert_eq!(m.len(), 3);
        assert_eq!(
            m["jos-gfx"],
            Lines {
                found: 150,
                hit: 100
            }
        );
        assert_eq!(m["jos"], Lines { found: 10, hit: 5 });
        assert_eq!(
            total(&m),
            Lines {
                found: 164,
                hit: 109
            }
        );
        assert!((m["jos"].percent() - 50.0).abs() < 1e-9);
        assert_eq!(Lines::default().percent(), 100.0);
    }

    #[test]
    fn floors_roundtrip_and_defaults() {
        let f =
            Floors::parse("[gate]\ntotal = 80\nnew_crate = 55.5\n[crates]\njos = 12.3\n").unwrap();
        assert_eq!(f.total, 80.0);
        assert_eq!(f.new_crate, 55.5);
        assert_eq!(f.floor_of("jos"), 12.3);
        assert_eq!(f.floor_of("other"), 55.5);
        let again = Floors::parse(&f.render()).unwrap();
        assert_eq!(again, f);
        assert_eq!(Floors::parse("").unwrap(), Floors::default());
        assert!(Floors::parse("[crates]\njos = \"high\"\n").is_err());
        assert!(Floors::parse("not toml [").is_err());
        assert_eq!(toml_key("a.b"), "\"a.b\"");
    }

    #[test]
    fn gate_and_ratchet() {
        let m = parse_lcov(LCOV);
        let mut f = Floors {
            total: 60.0,
            new_crate: 40.0,
            slack: 0.0,
            crates: [("jos-gfx".to_string(), 70.0)].into_iter().collect(),
        };
        let (rows, ok) = check(&m, &f);
        // jos-gfx 66.7 < 70: fails; jos 50 >= new_crate 40.
        assert!(!ok);
        let table = render_table(&rows);
        assert!(table.contains("jos-gfx"));
        assert!(table.contains("below floor"));
        assert!(table.contains("TOTAL"));

        // Ratchet raises, rounds down, never lowers.
        assert!(f.ratchet(&m));
        assert_eq!(f.crates["jos-gfx"], 70.0, "not lowered");
        assert_eq!(f.crates["jos"], 50.0);
        assert_eq!(f.crates["xtask"], 100.0);
        assert_eq!(f.total, 66.4); // 109/164 = 66.46
        assert!(!f.ratchet(&m), "idempotent");
        f.crates.insert("jos-gfx".into(), 66.6);
        let (_, ok) = check(&m, &f);
        assert!(ok, "66.67 passes a 66.6 floor");

        // With slack the floors stay below the measurement.
        let mut g = Floors {
            slack: 1.0,
            ..Floors::default()
        };
        g.ratchet(&m);
        assert_eq!(g.crates["jos"], 49.0);
        assert_eq!(g.crates["xtask"], 99.0);
    }
}
