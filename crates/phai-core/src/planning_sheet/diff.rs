//! Tell a person's edits apart from phai's own changes.
//!
//! Every sync records what it rendered in the hidden manifest tab. On the
//! next sync, a row that still matches its manifest snapshot was not touched
//! by hand, so phai's current value wins; a row that differs was edited and
//! becomes a forecast change. Rows a person typed have no id; rows a person
//! deleted are in the manifest but missing from the tab.

use std::collections::{BTreeSet, HashMap};

use rust_decimal::Decimal;

use super::grid::ParsedTab;
use super::{MonthTab, Section, CARD_KEY_PREFIX};

/// The editable fields of a row, as shown on the sheet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RowSnapshot {
    pub item: String,
    pub planned: Option<Decimal>,
    pub day: Option<u32>,
    pub repeats: bool,
    pub category_id: Option<String>,
    pub account_id: Option<String>,
    pub notes: Option<String>,
}

/// What the last sync rendered for one row of one tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    pub tab: String,
    pub key: String,
    pub section: Section,
    pub snapshot: RowSnapshot,
    pub editable: bool,
}

/// A change a person made on a month tab.
#[derive(Debug, Clone, PartialEq)]
pub enum SheetEdit {
    /// A new line typed into a block.
    Create { section: Section, row: RowSnapshot },
    /// An existing forecast's line was changed.
    Update {
        key: String,
        section: Section,
        before: RowSnapshot,
        after: RowSnapshot,
    },
    /// A forecast's line was deleted.
    Remove { key: String },
    /// A card's planned bill was typed (or cleared, with `None`).
    CardPlan {
        account_id: String,
        planned: Option<Decimal>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TabDiff {
    pub edits: Vec<SheetEdit>,
    pub warnings: Vec<String>,
}

/// Manifest key that marks a tab as written by phai, even when it is empty.
pub const TAB_MARKER: &str = "#aba";

/// The month tabs a previous sync wrote. A tab with a month title that is
/// not listed here belongs to the person and is never overwritten.
pub fn managed_tabs(manifest: &[ManifestEntry]) -> BTreeSet<String> {
    manifest
        .iter()
        .filter(|e| e.key == TAB_MARKER)
        .map(|e| e.tab.clone())
        .collect()
}

pub fn manifest_entries(tab: &MonthTab) -> Vec<ManifestEntry> {
    let title = tab.title();
    let marker = ManifestEntry {
        tab: title.clone(),
        key: TAB_MARKER.to_string(),
        section: Section::Expenses,
        snapshot: RowSnapshot::default(),
        editable: false,
    };
    std::iter::once(marker)
        .chain(tab.rows.iter().map(|row| ManifestEntry {
            tab: title.clone(),
            key: row.key.clone(),
            section: row.section,
            snapshot: RowSnapshot {
                item: row.item.clone(),
                planned: row.planned,
                day: row.day,
                repeats: row.repeats,
                category_id: row.category_id.clone(),
                account_id: row.account_id.clone(),
                notes: row.notes.clone(),
            },
            editable: row.editable,
        }))
        .collect()
}

pub fn diff_tab(title: &str, parsed: &ParsedTab, manifest: &[ManifestEntry]) -> TabDiff {
    let mut diff = TabDiff::default();
    if parsed.sections_found < Section::ALL.len() {
        diff.warnings.push(format!(
            "{title}: os blocos DESPESAS, CARTÕES e RECEITAS não estão todos na aba; \
             as edições dela foram ignoradas"
        ));
        return diff;
    }
    let known: HashMap<&str, &ManifestEntry> = manifest
        .iter()
        .filter(|e| e.tab == title)
        .map(|e| (e.key.as_str(), e))
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for row in &parsed.rows {
        let entry = row
            .key
            .as_deref()
            .filter(|k| !seen.contains(k))
            .and_then(|k| known.get(k).copied());
        match entry {
            Some(entry) => {
                seen.insert(entry.key.as_str());
                diff_known(title, entry, row.section, &row.snapshot, &mut diff);
            }
            None => diff_new(title, row.section, &row.snapshot, &mut diff),
        }
    }
    for entry in known.values() {
        if entry.editable && entry.section != Section::Cards && !seen.contains(entry.key.as_str()) {
            diff.edits.push(SheetEdit::Remove {
                key: entry.key.clone(),
            });
        }
    }
    diff
}

fn diff_known(
    title: &str,
    entry: &ManifestEntry,
    section: Section,
    row: &RowSnapshot,
    diff: &mut TabDiff,
) {
    if !entry.editable {
        if *row != entry.snapshot {
            diff.warnings.push(format!(
                "{title}: \"{}\" já foi pago e não muda pela planilha",
                entry.snapshot.item
            ));
        }
        return;
    }
    if entry.section == Section::Cards {
        if row.planned != entry.snapshot.planned {
            diff.edits.push(SheetEdit::CardPlan {
                account_id: entry
                    .key
                    .strip_prefix(CARD_KEY_PREFIX)
                    .unwrap_or(&entry.key)
                    .to_string(),
                planned: row.planned,
            });
        }
        return;
    }
    if *row != entry.snapshot || section != entry.section {
        diff.edits.push(SheetEdit::Update {
            key: entry.key.clone(),
            section,
            before: entry.snapshot.clone(),
            after: row.clone(),
        });
    }
}

fn diff_new(title: &str, section: Section, row: &RowSnapshot, diff: &mut TabDiff) {
    if section == Section::Cards {
        diff.warnings.push(format!(
            "{title}: CARTÕES tem uma linha por cartão; \"{}\" foi ignorada",
            row.item
        ));
        return;
    }
    if row.item.is_empty() || row.planned.is_none() {
        diff.warnings.push(format!(
            "{title}: linha sem item ou sem valor previsto foi ignorada ({})",
            if row.item.is_empty() {
                "sem item"
            } else {
                &row.item
            }
        ));
        return;
    }
    diff.edits.push(SheetEdit::Create {
        section,
        row: row.clone(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning_sheet::grid::ParsedRow;

    const TAB: &str = "MAIO 2030";

    fn snap(item: &str, planned: i64) -> RowSnapshot {
        RowSnapshot {
            item: item.to_string(),
            planned: Some(Decimal::new(planned, 0)),
            day: Some(5),
            ..RowSnapshot::default()
        }
    }

    fn entry(key: &str, section: Section, snapshot: RowSnapshot) -> ManifestEntry {
        ManifestEntry {
            tab: TAB.to_string(),
            key: key.to_string(),
            section,
            snapshot,
            editable: true,
        }
    }

    fn parsed(rows: Vec<(Section, Option<&str>, RowSnapshot)>) -> ParsedTab {
        ParsedTab {
            sections_found: 3,
            rows: rows
                .into_iter()
                .map(|(section, key, snapshot)| ParsedRow {
                    section,
                    key: key.map(str::to_string),
                    snapshot,
                })
                .collect(),
        }
    }

    #[test]
    fn manifest_marks_every_written_tab_even_when_empty() {
        let tab = MonthTab {
            month: chrono::NaiveDate::from_ymd_opt(2030, 5, 1).unwrap(),
            rows: Vec::new(),
            category_actuals: Vec::new(),
        };
        let entries = manifest_entries(&tab);
        assert_eq!(managed_tabs(&entries), [TAB.to_string()].into());
        let edits = diff_tab(TAB, &parsed(Vec::new()), &entries).edits;
        assert!(edits.is_empty(), "the marker is never a removal");
    }

    #[test]
    fn untouched_rows_produce_no_edits() {
        let manifest = vec![entry("rent", Section::Expenses, snap("Aluguel", 5000))];
        let tab = parsed(vec![(
            Section::Expenses,
            Some("rent"),
            snap("Aluguel", 5000),
        )]);
        assert_eq!(diff_tab(TAB, &tab, &manifest), TabDiff::default());
    }

    #[test]
    fn changed_value_is_an_update_with_before_and_after() {
        let manifest = vec![entry("rent", Section::Expenses, snap("Aluguel", 5000))];
        let tab = parsed(vec![(
            Section::Expenses,
            Some("rent"),
            snap("Aluguel", 5150),
        )]);
        assert_eq!(
            diff_tab(TAB, &tab, &manifest).edits,
            vec![SheetEdit::Update {
                key: "rent".to_string(),
                section: Section::Expenses,
                before: snap("Aluguel", 5000),
                after: snap("Aluguel", 5150),
            }]
        );
    }

    #[test]
    fn typed_row_is_a_create_and_missing_row_is_a_remove() {
        let manifest = vec![entry("old", Section::Expenses, snap("Envelope", 9300))];
        let tab = parsed(vec![(Section::Expenses, None, snap("Natação", 440))]);
        assert_eq!(
            diff_tab(TAB, &tab, &manifest).edits,
            vec![
                SheetEdit::Create {
                    section: Section::Expenses,
                    row: snap("Natação", 440),
                },
                SheetEdit::Remove {
                    key: "old".to_string()
                },
            ]
        );
    }

    #[test]
    fn row_copied_from_another_month_is_new_here() {
        let mut other_month = entry("rent", Section::Expenses, snap("Aluguel", 5000));
        other_month.tab = "ABRIL 2030".to_string();
        let tab = parsed(vec![(
            Section::Expenses,
            Some("rent"),
            snap("Aluguel", 5000),
        )]);
        assert!(matches!(
            diff_tab(TAB, &tab, &[other_month]).edits.as_slice(),
            [SheetEdit::Create { .. }]
        ));
    }

    #[test]
    fn duplicated_line_in_the_same_tab_creates_a_copy() {
        let manifest = vec![entry("rent", Section::Expenses, snap("Aluguel", 5000))];
        let tab = parsed(vec![
            (Section::Expenses, Some("rent"), snap("Aluguel", 5000)),
            (Section::Expenses, Some("rent"), snap("Aluguel extra", 100)),
        ]);
        assert_eq!(
            diff_tab(TAB, &tab, &manifest).edits,
            vec![SheetEdit::Create {
                section: Section::Expenses,
                row: snap("Aluguel extra", 100),
            }]
        );
    }

    #[test]
    fn paid_rows_ignore_edits_with_a_warning_and_are_never_removed() {
        let mut paid = entry("luz", Section::Expenses, snap("Luz", 400));
        paid.editable = false;
        let edited = parsed(vec![(Section::Expenses, Some("luz"), snap("Luz", 1))]);
        let diff = diff_tab(TAB, &edited, std::slice::from_ref(&paid));
        assert!(diff.edits.is_empty());
        assert_eq!(diff.warnings.len(), 1);
        assert!(diff_tab(TAB, &parsed(Vec::new()), &[paid]).edits.is_empty());
    }

    #[test]
    fn card_row_edit_is_a_card_plan_and_deleting_it_is_not_a_remove() {
        let manifest = vec![entry("card:c1", Section::Cards, snap("Cartão A", 110))];
        let tab = parsed(vec![(
            Section::Cards,
            Some("card:c1"),
            snap("Cartão A", 4000),
        )]);
        assert_eq!(
            diff_tab(TAB, &tab, &manifest).edits,
            vec![SheetEdit::CardPlan {
                account_id: "c1".to_string(),
                planned: Some(Decimal::new(4000, 0)),
            }]
        );
        assert!(diff_tab(TAB, &parsed(Vec::new()), &manifest)
            .edits
            .is_empty());
    }

    #[test]
    fn new_card_lines_and_rows_without_value_are_warnings() {
        let mut no_value = snap("Sem valor", 0);
        no_value.planned = None;
        let tab = parsed(vec![
            (Section::Cards, None, snap("Cartão novo", 100)),
            (Section::Expenses, None, no_value),
        ]);
        let diff = diff_tab(TAB, &tab, &[]);
        assert!(diff.edits.is_empty());
        assert_eq!(diff.warnings.len(), 2);
    }

    #[test]
    fn broken_layout_ignores_the_whole_tab() {
        let manifest = vec![entry("rent", Section::Expenses, snap("Aluguel", 5000))];
        let mut tab = parsed(Vec::new());
        tab.sections_found = 2;
        let diff = diff_tab(TAB, &tab, &manifest);
        assert!(
            diff.edits.is_empty(),
            "no Remove for rows hidden by a broken layout"
        );
        assert_eq!(diff.warnings.len(), 1);
    }

    #[test]
    fn moving_a_row_to_income_is_an_update_with_the_new_section() {
        let manifest = vec![entry("x", Section::Expenses, snap("Reembolso", 200))];
        let tab = parsed(vec![(Section::Income, Some("x"), snap("Reembolso", 200))]);
        assert!(matches!(
            diff_tab(TAB, &tab, &manifest).edits.as_slice(),
            [SheetEdit::Update {
                section: Section::Income,
                ..
            }]
        ));
    }
}
