// SPDX-License-Identifier: BSD-3-Clause
//! Manual partitioning, after Anaconda's custom partitioning: the disk's
//! partitions and free areas as a list, partitions deleted (and kept
//! again), new ones added into free space with a type and a size. Nothing
//! is written here: the result is a [`Layout`] for the installer, or the
//! reason there is none yet.
//!
//! JuanitOS needs a ZFS pool partition and an EFI system partition (one
//! already on the disk, shared, or a new one); swap is optional. The
//! labels are the ones of the other install methods, so the installed
//! system looks the same whichever way it was partitioned.

use crate::disks::{
    labels_free, partition_index, Disk, Partition, ESP_LABEL, MIN_POOL_GIB, SWAP_LABEL, ZFS_LABEL,
};
use crate::install::{Area, Esp, Layout};

const MIB: u64 = 1 << 20;

/// What a new partition is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Efi,
    Swap,
    Pool,
}

/// The choices for a new partition, in order.
pub const KINDS: [Kind; 3] = [Kind::Efi, Kind::Swap, Kind::Pool];

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Efi => "EFI system",
            Kind::Swap => "swap",
            Kind::Pool => "ZFS pool",
        }
    }

    /// What the size field starts with (empty: all of the free area).
    pub fn default_size(self) -> &'static str {
        match self {
            Kind::Efi => "260M",
            Kind::Swap => "4G",
            Kind::Pool => "",
        }
    }

    pub fn index(self) -> usize {
        KINDS.iter().position(|k| *k == self).unwrap_or(0)
    }
}

/// A partition to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPart {
    pub kind: Kind,
    pub area: Area,
}

/// A line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A partition on the disk: kept, or (at the end of the list) to be
    /// deleted.
    Existing {
        part: Partition,
        deleted: bool,
    },
    Free(Area),
    /// A partition to create, and its index among the additions.
    New {
        index: usize,
        part: NewPart,
    },
}

/// The disk being laid out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    disk: Disk,
    deleted: Vec<String>,
    added: Vec<NewPart>,
}

impl Editor {
    pub fn new(disk: &Disk) -> Self {
        Editor {
            disk: disk.clone(),
            ..Default::default()
        }
    }

    /// Can `disk` be laid out by hand? Its GPT table is edited, or one is
    /// created when it has none; any other table must be erased instead.
    pub fn editable(disk: &Disk) -> Result<(), String> {
        match disk.scheme.as_deref() {
            None | Some("GPT") => Ok(()),
            Some(other) => Err(format!("the disk has an {other} partition table, not GPT")),
        }
    }

    fn kept(&self) -> impl Iterator<Item = &Partition> {
        self.disk
            .partitions
            .iter()
            .filter(|p| !self.deleted.contains(&p.name))
    }

    /// Free areas once the deletions and additions are done, in MiB.
    fn free(&self) -> Vec<Area> {
        let after = Disk {
            partitions: {
                let mut v: Vec<Partition> = self
                    .kept()
                    .cloned()
                    .chain(self.added.iter().map(|n| Partition {
                        offset: n.area.start_mib * MIB,
                        size: n.area.size_mib * MIB,
                        ..Default::default()
                    }))
                    .collect();
                v.sort_by_key(|p| p.offset);
                v
            },
            ..self.disk.clone()
        };
        after
            .free_areas()
            .into_iter()
            .map(|(start, size)| Area {
                start_mib: start / MIB,
                size_mib: size / MIB,
            })
            .collect()
    }

    /// The list: the disk as it will be, by offset, then the partitions
    /// to delete.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows: Vec<(u64, Row)> =
            self.kept()
                .map(|p| {
                    let row = Row::Existing {
                        part: p.clone(),
                        deleted: false,
                    };
                    (p.offset, row)
                })
                .chain(self.added.iter().enumerate().map(|(index, part)| {
                    (part.area.start_mib * MIB, Row::New { index, part: *part })
                }))
                .chain(
                    self.free()
                        .into_iter()
                        .map(|a| (a.start_mib * MIB, Row::Free(a))),
                )
                .collect();
        rows.sort_by_key(|r| r.0);
        rows.into_iter()
            .map(|r| r.1)
            .chain(
                self.disk
                    .partitions
                    .iter()
                    .filter(|p| self.deleted.contains(&p.name))
                    .map(|p| Row::Existing {
                        part: p.clone(),
                        deleted: true,
                    }),
            )
            .collect()
    }

    /// Delete a kept partition, keep a deleted one again, or drop a new
    /// one; free space has nothing to toggle.
    pub fn toggle(&mut self, row: &Row) -> Result<(), String> {
        match row {
            Row::Existing {
                part,
                deleted: false,
            } => self.deleted.push(part.name.clone()),
            Row::Existing {
                part,
                deleted: true,
            } => {
                let (start, end) = (part.offset, part.offset + part.size);
                if self.added.iter().any(|n| {
                    n.area.start_mib * MIB < end
                        && start < (n.area.start_mib + n.area.size_mib) * MIB
                }) {
                    return Err(format!(
                        "Remove the new partitions in the space of {} first.",
                        part.name
                    ));
                }
                self.deleted.retain(|d| *d != part.name);
            }
            Row::New { index, .. } => {
                if *index < self.added.len() {
                    self.added.remove(*index);
                }
            }
            Row::Free(_) => {}
        }
        Ok(())
    }

    /// Add a `kind` partition at the start of the free `area`, of the size
    /// typed (`size_mib`).
    pub fn add(&mut self, kind: Kind, area: Area, size: &str) -> Result<(), String> {
        if self.added.iter().any(|n| n.kind == kind) {
            return Err(format!(
                "There is a new {} partition already.",
                kind.label()
            ));
        }
        let size_mib = parse_size_mib(size, area.size_mib)?;
        self.added.push(NewPart {
            kind,
            area: Area {
                start_mib: area.start_mib,
                size_mib,
            },
        });
        Ok(())
    }

    fn added(&self, kind: Kind) -> Option<Area> {
        self.added.iter().find(|n| n.kind == kind).map(|n| n.area)
    }

    /// The kind a new partition most likely is: the missing ESP first,
    /// then swap (before the pool takes the rest), then the pool.
    pub fn suggest(&self) -> Kind {
        let has_esp = self.added(Kind::Efi).is_some() || self.kept().any(|p| p.kind == "efi");
        if !has_esp {
            Kind::Efi
        } else if self.added(Kind::Swap).is_none() && self.added(Kind::Pool).is_none() {
            Kind::Swap
        } else {
            Kind::Pool
        }
    }

    /// The installer's changes, or what is still missing. `all` are the
    /// machine's disks, whose GPT labels must not clash with the new ones;
    /// on this disk only the partitions kept count, a deleted one takes
    /// its label with it (so an earlier install is replaced by deleting
    /// its partitions, nothing more).
    pub fn layout(&self, all: &[Disk]) -> Result<Layout, String> {
        let Some(pool) = self.added(Kind::Pool) else {
            return Err("Add a ZFS pool partition in free space.".into());
        };
        if pool.size_mib < MIN_POOL_GIB * 1024 {
            return Err(format!(
                "The ZFS pool partition needs {MIN_POOL_GIB} GiB at least."
            ));
        }
        let esp = match self.added(Kind::Efi) {
            Some(area) => Esp::New {
                area,
                index: self.free_index(),
            },
            None => match self.kept().find(|p| p.kind == "efi") {
                Some(p) => Esp::Shared(p.name.clone()),
                None => {
                    return Err("Keep the disk's EFI system partition, or add one.".into());
                }
            },
        };
        let swap = self.added(Kind::Swap);
        let mut labels = vec![ZFS_LABEL];
        if swap.is_some() {
            labels.push(SWAP_LABEL);
        }
        if let Esp::New { .. } = esp {
            labels.push(ESP_LABEL);
        }
        let kept_label = self.kept().find_map(|p| {
            let label = p.label.as_deref().filter(|l| labels.contains(l))?;
            Some((p.name.as_str(), label))
        });
        if let Some((name, label)) = kept_label {
            return Err(format!(
                "Delete {name} first: it is labelled {label} (an earlier install?)."
            ));
        }
        let others = all.iter().filter(|d| d.name != self.disk.name);
        labels_free(others, &labels).map_err(|e| format!("Cannot partition: {e}."))?;
        Ok(Layout {
            create_table: self.disk.scheme.is_none(),
            delete: self
                .disk
                .partitions
                .iter()
                .filter(|p| self.deleted.contains(&p.name))
                .map(|p| p.name.clone())
                .collect(),
            esp,
            swap,
            pool,
        })
    }

    /// The lowest GPT index no kept partition uses.
    fn free_index(&self) -> u32 {
        let used: Vec<u32> = self
            .kept()
            .filter_map(|p| partition_index(&self.disk.name, &p.name))
            .collect();
        (1..).find(|i| !used.contains(i)).unwrap_or(1)
    }
}

/// A size as typed: `4G`, `260M`, `16` (GiB), or nothing for all of the
/// `max_mib` available.
pub fn parse_size_mib(text: &str, max_mib: u64) -> Result<u64, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(max_mib);
    }
    let (digits, unit) = match t.strip_suffix(['M', 'm']) {
        Some(d) => (d, 1),
        None => (t.strip_suffix(['G', 'g']).unwrap_or(t), 1024),
    };
    let size = match digits.parse::<u64>() {
        Ok(n) if n > 0 => n.saturating_mul(unit),
        _ => return Err("Sizes are a whole number of GiB, or MiB with an M: 4G, 260M.".into()),
    };
    if size > max_mib {
        return Err(format!(
            "Only {:.1} GiB are free there.",
            max_mib as f64 / 1024.0
        ));
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disks::tests::{labelled, linux_disk};

    fn area(start_mib: u64, size_mib: u64) -> Area {
        Area {
            start_mib,
            size_mib,
        }
    }

    fn free_rows(e: &Editor) -> Vec<Area> {
        e.rows()
            .into_iter()
            .filter_map(|r| match r {
                Row::Free(a) => Some(a),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size_mib("", 500), Ok(500));
        assert_eq!(parse_size_mib(" 260M ", 500), Ok(260));
        assert_eq!(parse_size_mib("260m", 500), Ok(260));
        assert_eq!(parse_size_mib("4G", 8192), Ok(4096));
        assert_eq!(parse_size_mib("4g", 8192), Ok(4096));
        assert_eq!(parse_size_mib("8", 8192), Ok(8192));
        assert!(parse_size_mib("0", 500)
            .unwrap_err()
            .contains("whole number"));
        assert!(parse_size_mib("G", 500).is_err());
        assert!(parse_size_mib("1.5G", 5000).is_err());
        assert_eq!(
            parse_size_mib("1G", 512).unwrap_err(),
            "Only 0.5 GiB are free there."
        );
    }

    #[test]
    fn kinds() {
        for (i, k) in KINDS.iter().enumerate() {
            assert_eq!(k.index(), i);
            assert!(!k.label().is_empty());
        }
        assert_eq!(Kind::Pool.default_size(), "");
        assert_eq!(Kind::Efi.default_size(), "260M");
        assert_eq!(Kind::Swap.default_size(), "4G");
    }

    #[test]
    fn which_disks_can_be_edited() {
        assert!(Editor::editable(&linux_disk()).is_ok());
        assert!(Editor::editable(&Disk::default()).is_ok());
        let mbr = Disk {
            scheme: Some("MBR".into()),
            ..Default::default()
        };
        assert_eq!(
            Editor::editable(&mbr).unwrap_err(),
            "the disk has an MBR partition table, not GPT"
        );
    }

    #[test]
    fn rows_are_the_disk_in_order() {
        let d = linux_disk();
        let mut e = Editor::new(&d);
        let names = |e: &Editor| -> Vec<String> {
            e.rows()
                .iter()
                .map(|r| match r {
                    Row::Existing { part, .. } => part.name.clone(),
                    Row::Free(a) => format!("free {}", a.size_mib),
                    Row::New { part, .. } => part.kind.label().into(),
                })
                .collect()
        };
        assert_eq!(
            names(&e),
            ["nda0p1", "nda0p2", "free 306899", "nda0p3", "free 4095"]
        );
        assert_eq!(e.suggest(), Kind::Swap);
        assert_eq!(e.free_index(), 4);
        e.add(Kind::Swap, free_rows(&e)[1], "1G").unwrap();
        assert_eq!(
            names(&e),
            [
                "nda0p1",
                "nda0p2",
                "free 306899",
                "nda0p3",
                "swap",
                "free 3071"
            ]
        );
    }

    #[test]
    fn next_to_linux_by_hand() {
        let d = linux_disk();
        let all = std::slice::from_ref(&d);
        let mut e = Editor::new(&d);
        assert_eq!(
            e.layout(all).unwrap_err(),
            "Add a ZFS pool partition in free space."
        );
        let gap = free_rows(&e)[0];
        e.add(Kind::Swap, gap, "8G").unwrap();
        assert_eq!(e.suggest(), Kind::Pool);
        assert!(e
            .add(Kind::Swap, gap, "1G")
            .unwrap_err()
            .contains("already"));
        // The swap took the start of the gap; the pool goes after it.
        let rest = free_rows(&e)[0];
        assert_eq!(rest, area(205_101 + 8192, 306_899 - 8192));
        e.add(Kind::Pool, rest, "10G").unwrap();
        assert!(e.layout(all).unwrap_err().contains("16 GiB"));
        let pool_row = e
            .rows()
            .into_iter()
            .find(|r| matches!(r, Row::New { part, .. } if part.kind == Kind::Pool))
            .unwrap();
        e.toggle(&pool_row).unwrap();
        e.add(Kind::Pool, rest, "").unwrap();
        assert_eq!(
            e.layout(all).unwrap(),
            Layout {
                create_table: false,
                delete: vec![],
                esp: Esp::Shared("nda0p1".into()),
                swap: Some(area(205_101, 8192)),
                pool: rest,
            }
        );
        // Toggling free space, or a row that is gone, changes nothing.
        let before = e.clone();
        e.toggle(&Row::Free(rest)).unwrap();
        e.toggle(&Row::New {
            index: 9,
            part: NewPart {
                kind: Kind::Pool,
                area: rest,
            },
        })
        .unwrap();
        assert_eq!(e, before);
    }

    #[test]
    fn replacing_linux() {
        let d = linux_disk();
        let all = std::slice::from_ref(&d);
        let mut e = Editor::new(&d);
        // Delete the Linux root and swap: one big area after the ESP.
        for name in ["nda0p2", "nda0p3"] {
            let row = e
                .rows()
                .into_iter()
                .find(|r| matches!(r, Row::Existing { part, .. } if part.name == name))
                .unwrap();
            e.toggle(&row).unwrap();
        }
        let rows = e.rows();
        assert!(matches!(
            rows.last(),
            Some(Row::Existing { part, deleted: true }) if part.name == "nda0p3"
        ));
        let gap = free_rows(&e)[0];
        assert_eq!(gap, area(301, 512 * 1024 - 301 - 1));
        e.add(Kind::Pool, gap, "").unwrap();
        let l = e.layout(all).unwrap();
        assert_eq!(l.delete, ["nda0p2", "nda0p3"]);
        assert_eq!((l.swap, l.pool), (None, gap));
        // A deleted partition whose space is now used cannot come back.
        let p2 = rows
            .iter()
            .find(|r| matches!(r, Row::Existing { part, .. } if part.name == "nda0p2"))
            .unwrap()
            .clone();
        assert!(e.toggle(&p2).unwrap_err().contains("nda0p2"));
        // Deleting the ESP needs a new one, at the first free index.
        let esp = e.rows().into_iter().next().unwrap();
        e.toggle(&esp).unwrap();
        assert_eq!(e.suggest(), Kind::Efi);
        assert_eq!(
            e.layout(all).unwrap_err(),
            "Keep the disk's EFI system partition, or add one."
        );
        let pool_row = e
            .rows()
            .into_iter()
            .find(|r| matches!(r, Row::New { .. }))
            .unwrap();
        e.toggle(&pool_row).unwrap();
        // With the pool gone, nda0p3's space is free again: it comes back.
        let p3 = e.rows().into_iter().last().unwrap();
        e.toggle(&p3).unwrap();
        let gap = free_rows(&e)[0];
        e.add(Kind::Efi, gap, "260M").unwrap();
        let gap = free_rows(&e)[0];
        e.add(Kind::Pool, gap, "100G").unwrap();
        let l = e.layout(all).unwrap();
        assert_eq!(l.delete, ["nda0p1", "nda0p2"]);
        assert_eq!(
            l.esp,
            Esp::New {
                area: area(1, 260),
                index: 1
            }
        );
        assert_eq!(l.pool, area(261, 100 * 1024));
    }

    #[test]
    fn a_blank_disk_gets_a_table() {
        let d = Disk {
            name: "ada0".into(),
            size_bytes: 64 << 30,
            ..Default::default()
        };
        let mut e = Editor::new(&d);
        let gap = free_rows(&e)[0];
        e.add(Kind::Efi, gap, "260M").unwrap();
        e.add(Kind::Pool, free_rows(&e)[0], "").unwrap();
        let l = e.layout(std::slice::from_ref(&d)).unwrap();
        assert!(l.create_table);
        assert_eq!(
            l.esp,
            Esp::New {
                area: area(1, 260),
                index: 1
            }
        );
        // The labels of an earlier install on another disk get in the way.
        let old = Disk {
            name: "ada1".into(),
            partitions: vec![labelled("ada1p1", ESP_LABEL)],
            ..Default::default()
        };
        assert_eq!(
            e.layout(&[d.clone(), old]).unwrap_err(),
            "Cannot partition: ada1 already has a partition labelled efiboot0 (an earlier install?)."
        );
    }

    #[test]
    fn replacing_an_earlier_install() {
        // A disk JuanitOS filled: ESP, swap and a pool to the end, with
        // the install's labels.
        let part = |name: &str, kind: &str, start_mib: u64, size_mib: u64, label: &str| Partition {
            name: name.into(),
            kind: kind.into(),
            offset: start_mib * MIB,
            size: size_mib * MIB,
            label: Some(label.into()),
        };
        let end = 1800 * 1024 - 1;
        let d = Disk {
            name: "da0".into(),
            size_bytes: 1800 << 30,
            scheme: Some("GPT".into()),
            partitions: vec![
                part("da0p1", "efi", 1, 260, ESP_LABEL),
                part("da0p2", "freebsd-swap", 261, 4096, SWAP_LABEL),
                part("da0p3", "freebsd-zfs", 4357, end - 4357, ZFS_LABEL),
            ],
            ..Default::default()
        };
        let all = std::slice::from_ref(&d);
        let row = |e: &Editor, name: &str| {
            e.rows()
                .into_iter()
                .find(|r| matches!(r, Row::Existing { part, .. } if part.name == name))
                .unwrap()
        };
        let mut e = Editor::new(&d);
        // The pool deleted and a smaller one added in its place: the
        // label goes with the deleted partition, so it is free.
        e.toggle(&row(&e, "da0p3")).unwrap();
        let gap = free_rows(&e)[0];
        assert_eq!(gap, area(4357, end - 4357));
        e.add(Kind::Pool, gap, "500G").unwrap();
        let l = e.layout(all).unwrap();
        assert_eq!(l.delete, ["da0p3"]);
        assert_eq!(l.esp, Esp::Shared("da0p1".into()));
        assert_eq!(l.swap, None);
        assert_eq!(l.pool, area(4357, 500 * 1024));
        // The old swap is kept and a new one added: same label twice.
        e.add(Kind::Swap, free_rows(&e)[0], "8G").unwrap();
        assert_eq!(
            e.layout(all).unwrap_err(),
            "Delete da0p2 first: it is labelled swap0 (an earlier install?)."
        );
        e.toggle(&row(&e, "da0p2")).unwrap();
        assert_eq!(e.layout(all).unwrap().delete, ["da0p2", "da0p3"]);
        // The same disk among the machine's disks is not "another disk"
        // with the labels; a real other disk with them still is.
        let mut other = d.clone();
        other.name = "da1".into();
        assert!(e
            .layout(&[d.clone(), other])
            .unwrap_err()
            .starts_with("Cannot partition: da1 already has a partition labelled"));
    }
}
