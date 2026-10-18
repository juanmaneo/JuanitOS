//! Coverage per file from llvm-cov's JSON summary, and the ratcheting gate.
//!
//! The target is 100 % of lines, functions and regions in every file.
//! `coverage.toml` lists, per file, how many lines, functions and regions
//! may still be uncovered; a file it does not list must be fully covered.
//! The gate counts exactly (no percentages, no slack): coverage must be
//! deterministic, so a host or timing difference shows up as a failure to
//! fix in the tests rather than as noise. `--ratchet` lowers the
//! allowances to what was measured and drops files that reach zero, so
//! the list only shrinks.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;

/// Items found / covered for one metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Counts {
    #[serde(rename = "count")]
    pub found: u64,
    #[serde(rename = "covered")]
    pub hit: u64,
}

impl Counts {
    pub fn missed(self) -> u64 {
        self.found.saturating_sub(self.hit)
    }

    pub fn percent(self) -> f64 {
        if self.found == 0 {
            100.0
        } else {
            self.hit as f64 * 100.0 / self.found as f64
        }
    }

    fn add(&mut self, other: Counts) {
        self.found += other.found;
        self.hit += other.hit;
    }
}

/// The three gated metrics of a file, a crate or the whole workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Metrics {
    pub lines: Counts,
    pub functions: Counts,
    pub regions: Counts,
}

impl Metrics {
    fn add(&mut self, other: &Metrics) {
        self.lines.add(other.lines);
        self.functions.add(other.functions);
        self.regions.add(other.regions);
    }

    fn missed(&self) -> Allowed {
        Allowed {
            lines: self.lines.missed(),
            functions: self.functions.missed(),
            regions: self.regions.missed(),
        }
    }
}

/// One crate: its files (paths relative to the repository) and their sum.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Crate {
    pub total: Metrics,
    pub files: BTreeMap<String, Metrics>,
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

#[derive(Deserialize)]
struct Export {
    data: Vec<ExportData>,
}

#[derive(Deserialize)]
struct ExportData {
    files: Vec<ExportFile>,
}

#[derive(Deserialize)]
struct ExportFile {
    filename: String,
    summary: Metrics,
}

/// Group the files of `llvm-cov export -summary-only` output by crate,
/// with paths relative to `root` and `/`-separated.
pub fn parse_summary(json: &str, root: &Path) -> anyhow::Result<BTreeMap<String, Crate>> {
    let export: Export = serde_json::from_str(json)?;
    let root = root.to_string_lossy().replace('\\', "/");
    let root = format!("{}/", root.trim_end_matches('/'));
    let mut per_crate: BTreeMap<String, Crate> = BTreeMap::new();
    for file in export.data.iter().flat_map(|d| &d.files) {
        let Some(name) = crate_of(&file.filename) else {
            continue;
        };
        let path = file.filename.replace('\\', "/");
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_string();
        let c = per_crate.entry(name).or_default();
        c.total.add(&file.summary);
        c.files.insert(rel, file.summary);
    }
    Ok(per_crate)
}

pub fn total(per_crate: &BTreeMap<String, Crate>) -> Metrics {
    let mut t = Metrics::default();
    for c in per_crate.values() {
        t.add(&c.total);
    }
    t
}

/// What a file may still leave uncovered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Allowed {
    pub lines: u64,
    pub functions: u64,
    pub regions: u64,
}

impl Allowed {
    fn is_zero(&self) -> bool {
        *self == Allowed::default()
    }

    fn metrics(&self) -> [(&'static str, u64); 3] {
        [
            ("lines", self.lines),
            ("functions", self.functions),
            ("regions", self.regions),
        ]
    }
}

/// The gate configuration (`coverage.toml`): allowances per file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Floors {
    pub files: BTreeMap<String, Allowed>,
}

impl Floors {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let table: toml::Table = text.parse()?;
        let mut f = Floors::default();
        if let Some(files) = table.get("files").and_then(|c| c.as_table()) {
            for (path, v) in files {
                let Some(t) = v.as_table() else {
                    anyhow::bail!("coverage.toml: files.{path:?} must be a table");
                };
                let mut a = Allowed::default();
                for (key, value) in t {
                    let Some(n) = value.as_integer().and_then(|n| u64::try_from(n).ok()) else {
                        anyhow::bail!(
                            "coverage.toml: files.{path:?}.{key} must be a non-negative integer"
                        );
                    };
                    match key.as_str() {
                        "lines" => a.lines = n,
                        "functions" => a.functions = n,
                        "regions" => a.regions = n,
                        other => anyhow::bail!("coverage.toml: unknown metric {other:?}"),
                    }
                }
                f.files.insert(path.clone(), a);
            }
        }
        Ok(f)
    }

    pub fn render(&self) -> String {
        let mut s = String::from(
            "# Coverage allowances checked by `cargo xtask coverage`.\n\
             #\n\
             # The target is 100 % of lines, functions and regions in every file.\n\
             # Each entry below says how many of them a file may still leave\n\
             # uncovered; a file that is not listed must be fully covered.\n\
             # Counts are exact: coverage has to be deterministic.\n\
             # `cargo xtask coverage --ratchet` lowers the allowances to the\n\
             # measured values and drops files that reach zero (never raises).\n\
             # Commit the updated file together with the tests that earned it.\n",
        );
        for (path, a) in &self.files {
            let _ = write!(s, "\n[files.{path:?}]\n");
            for (key, n) in a.metrics() {
                if n > 0 {
                    let _ = writeln!(s, "{key} = {n}");
                }
            }
        }
        s
    }

    pub fn allowed(&self, path: &str) -> Allowed {
        self.files.get(path).copied().unwrap_or_default()
    }

    /// Lower every allowance to what was measured; never raise one. Files
    /// that are fully covered, or gone, leave the list.
    pub fn ratchet(&mut self, per_crate: &BTreeMap<String, Crate>) -> bool {
        let mut next = BTreeMap::new();
        for (path, m) in per_crate.values().flat_map(|c| &c.files) {
            let old = self.allowed(path);
            let got = m.missed();
            let a = Allowed {
                lines: old.lines.min(got.lines),
                functions: old.functions.min(got.functions),
                regions: old.regions.min(got.regions),
            };
            if !a.is_zero() {
                next.insert(path.clone(), a);
            }
        }
        let changed = next != self.files;
        self.files = next;
        changed
    }
}

/// One line of the report: a crate (or the total) and whether all of its
/// files are within their allowances.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub name: String,
    pub metrics: Metrics,
    /// `path: metric N uncovered, M allowed` for each file over the limit.
    pub over: Vec<String>,
}

impl Row {
    pub fn passes(&self) -> bool {
        self.over.is_empty()
    }
}

/// Compare a measurement with the allowances: the rows (crates then
/// total) and whether everything passes.
pub fn check(per_crate: &BTreeMap<String, Crate>, floors: &Floors) -> (Vec<Row>, bool) {
    let mut rows: Vec<Row> = per_crate
        .iter()
        .map(|(name, c)| {
            let mut over = Vec::new();
            for (path, m) in &c.files {
                let allowed = floors.allowed(path);
                for ((key, got), (_, max)) in
                    m.missed().metrics().into_iter().zip(allowed.metrics())
                {
                    if got > max {
                        over.push(format!("{path}: {got} {key} uncovered, {max} allowed"));
                    }
                }
            }
            Row {
                name: name.clone(),
                metrics: c.total,
                over,
            }
        })
        .collect();
    let all_over: Vec<String> = rows.iter().flat_map(|r| r.over.clone()).collect();
    rows.push(Row {
        name: "TOTAL".into(),
        metrics: total(per_crate),
        over: all_over,
    });
    let ok = rows.iter().all(Row::passes);
    (rows, ok)
}

pub fn render_table(rows: &[Row]) -> String {
    let mut s = format!(
        "{:<16} {:>16} {:>16} {:>16}\n",
        "crate", "lines", "functions", "regions"
    );
    let cell = |c: Counts| format!("{:.1}% ({})", c.percent(), c.missed());
    for r in rows {
        let mark = if r.passes() { "" } else { "  <-- over" };
        let m = r.metrics;
        let _ = writeln!(
            s,
            "{:<16} {:>16} {:>16} {:>16}{mark}",
            r.name,
            cell(m.lines),
            cell(m.functions),
            cell(m.regions)
        );
    }
    s.push_str("(uncovered in parentheses; the goal is 0 everywhere)\n");
    if let Some(total) = rows.last() {
        for line in &total.over {
            let _ = writeln!(s, "  over: {line}");
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(found: u64, hit: u64) -> String {
        format!(r#"{{"count":{found},"covered":{hit},"percent":0}}"#)
    }

    fn file(name: &str, l: (u64, u64), f: (u64, u64), r: (u64, u64)) -> String {
        format!(
            r#"{{"filename":"{name}","summary":{{"lines":{},"functions":{},"regions":{},"branches":{}}}}}"#,
            counts(l.0, l.1),
            counts(f.0, f.1),
            counts(r.0, r.1),
            counts(0, 0)
        )
    }

    fn summary() -> String {
        format!(
            r#"{{"data":[{{"files":[{},{},{},{}],"totals":{{}}}}],"type":"llvm.coverage.json.export"}}"#,
            file(
                "/home/u/JuanitOS/crates/jos-gfx/src/font.rs",
                (100, 90),
                (10, 9),
                (200, 180)
            ),
            file(
                "/home/u/JuanitOS/crates/jos-gfx/src/png.rs",
                (50, 50),
                (5, 5),
                (80, 80)
            ),
            file(
                "C:\\\\src\\\\JuanitOS\\\\crates\\\\jos\\\\src\\\\main.rs",
                (10, 5),
                (2, 1),
                (20, 10)
            ),
            file(
                "/home/u/.cargo/registry/src/x/serde/lib.rs",
                (1000, 0),
                (1, 0),
                (1, 0)
            ),
        )
    }

    fn measured() -> BTreeMap<String, Crate> {
        parse_summary(&summary(), Path::new("/home/u/JuanitOS/")).unwrap()
    }

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
    fn summary_is_grouped_per_crate_with_relative_paths() {
        let m = measured();
        assert_eq!(m.len(), 2, "dependencies are left out");
        let gfx = &m["jos-gfx"];
        assert_eq!(
            gfx.files.keys().collect::<Vec<_>>(),
            ["crates/jos-gfx/src/font.rs", "crates/jos-gfx/src/png.rs"]
        );
        assert_eq!(
            gfx.total.lines,
            Counts {
                found: 150,
                hit: 140
            }
        );
        assert_eq!(gfx.total.regions.missed(), 20);
        // Outside the root (another checkout): the path is kept whole.
        assert!(m["jos"]
            .files
            .contains_key("C:/src/JuanitOS/crates/jos/src/main.rs"));
        let t = total(&m);
        assert_eq!(t.functions, Counts { found: 17, hit: 15 });
        assert!((m["jos"].total.lines.percent() - 50.0).abs() < 1e-9);
        assert_eq!(Counts::default().percent(), 100.0);
        assert!(parse_summary("{}", Path::new("/")).is_err());
    }

    #[test]
    fn allowances_roundtrip_and_reject_nonsense() {
        let f = Floors::parse(
            "[files.\"crates/jos/src/main.rs\"]\nlines = 3\nregions = 7\n\
             [files.\"crates/a.rs\"]\nfunctions = 1\n",
        )
        .unwrap();
        assert_eq!(
            f.allowed("crates/jos/src/main.rs"),
            Allowed {
                lines: 3,
                functions: 0,
                regions: 7
            }
        );
        assert_eq!(f.allowed("crates/new.rs"), Allowed::default());
        let text = f.render();
        assert!(
            text.contains("[files.\"crates/a.rs\"]\nfunctions = 1\n"),
            "{text}"
        );
        assert_eq!(Floors::parse(&text).unwrap(), f);
        assert_eq!(Floors::parse("").unwrap(), Floors::default());
        for bad in [
            "[files]\nx = 1\n",
            "[files.x]\nlines = -1\n",
            "[files.x]\nlines = \"many\"\n",
            "[files.x]\nbranches = 1\n",
            "not toml [",
        ] {
            assert!(Floors::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn gate_and_ratchet() {
        let m = measured();
        let font = "crates/jos-gfx/src/font.rs";
        let mut f = Floors::default();
        f.files.insert(
            font.into(),
            Allowed {
                lines: 10,
                functions: 0,
                regions: 50,
            },
        );
        let (rows, ok) = check(&m, &f);
        assert!(!ok);
        let gfx = rows.iter().find(|r| r.name == "jos-gfx").unwrap();
        assert_eq!(
            gfx.over,
            [format!("{font}: 1 functions uncovered, 0 allowed")]
        );
        let table = render_table(&rows);
        assert!(table.contains("jos-gfx"), "{table}");
        assert!(table.contains("93.3% (10)"), "{table}");
        assert!(table.contains("<-- over"), "{table}");
        assert!(table.contains("  over: crates/jos-gfx/src/font.rs: 1 functions"));
        // A file nobody listed must be complete.
        assert!(
            table.contains("main.rs: 5 lines uncovered, 0 allowed"),
            "{table}"
        );

        // Ratchet lowers to the measurement, never raises, drops zeros.
        f.files.insert(
            "crates/jos-gfx/src/png.rs".into(),
            Allowed {
                lines: 2,
                ..Allowed::default()
            },
        );
        f.files.insert(
            "crates/gone.rs".into(),
            Allowed {
                lines: 1,
                ..Allowed::default()
            },
        );
        assert!(f.ratchet(&m));
        assert_eq!(
            f.allowed(font),
            Allowed {
                lines: 10,
                functions: 0,
                regions: 20
            }
        );
        assert!(
            !f.files.contains_key("crates/jos-gfx/src/png.rs"),
            "complete"
        );
        assert!(!f.files.contains_key("crates/gone.rs"), "deleted file");
        assert!(!f.ratchet(&m), "idempotent");

        // Within every allowance: passes.
        let mut all = Floors::default();
        for (path, metrics) in m.values().flat_map(|c| &c.files) {
            all.files.insert(path.clone(), metrics.missed());
        }
        let (rows, ok) = check(&m, &all);
        assert!(ok);
        let table = render_table(&rows);
        assert!(
            !table.contains("<-- over") && !table.contains("  over:"),
            "{table}"
        );
    }
}
