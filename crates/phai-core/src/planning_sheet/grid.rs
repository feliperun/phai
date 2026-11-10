//! Cells, and reading an edited month tab back.
//!
//! The layout itself lives in [`super::layout`]; this side walks a tab the
//! way a person reads it (fixed bills until `Subtotal fixas`, cards until
//! `Total Cartão`, income from `RECEITAS` to `Total Receitas`) and stops at
//! `SALDO MES`, so rows inserted inside a block stay in that block.

use rust_decimal::Decimal;
use serde_json::Value;

use super::diff::{ManifestEntry, RowSnapshot};
use super::layout::{
    BALANCE_LABEL, CANCELLED, COL_ACCOUNT, COL_CATEGORY, COL_DAY, COL_ITEM, COL_NOTES, COL_PLANNED,
    COL_REPEATS, COL_STATUS, EXPENSES_TOTAL, ID_COLUMN, INCOME_TITLE, SIGNATURE_COL,
};
use super::{Section, MONTHLY};
use crate::models::decimal_from_str;

/// Start of the note phai writes in `T1` of every month tab it owns.
pub const SIGNATURE: &str = "Atualizado pelo phai";

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    Number(Decimal),
    Formula(String),
}

impl Cell {
    pub(super) fn text(value: impl Into<String>) -> Cell {
        Cell::Text(value.into())
    }

    pub(super) fn opt_text(value: Option<&str>) -> Cell {
        value.map_or(Cell::Empty, Cell::text)
    }

    pub(super) fn opt_number(value: Option<Decimal>) -> Cell {
        value.map_or(Cell::Empty, |v| Cell::Number(v.round_dp(2)))
    }
}

/// Whether a tab carries phai's note in `T1`, i.e. phai wrote it even if
/// the manifest lost track of it.
pub fn is_phai_tab(values: &[Vec<Value>]) -> bool {
    values
        .first()
        .and_then(|row| text_at(row, SIGNATURE_COL))
        .is_some_and(|note| note.starts_with(SIGNATURE))
}

/// A data line read back from a month tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRow {
    pub section: Section,
    /// The `id` column; `None` for a row a person just typed.
    pub key: Option<String>,
    pub snapshot: RowSnapshot,
    /// `Status` set to `Cancelado`: the person wants the row gone.
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedTab {
    /// False when a block's closing row is missing or renamed: the tab is
    /// not trusted, so nothing on it is read as an edit.
    pub layout_ok: bool,
    pub rows: Vec<ParsedRow>,
}

/// The rows that open and close blocks, in order.
const MARKERS: usize = 4;

pub fn parse_tab(values: &[Vec<Value>]) -> ParsedTab {
    let mut parsed = ParsedTab::default();
    let mut current = Some(Section::Expenses);
    let mut markers = 0;
    for row in values.iter().skip(1) {
        let first = text_at(row, COL_ITEM).unwrap_or_default();
        match line_kind(&first) {
            Line::Boundary(next) => {
                markers += 1;
                current = next;
            }
            Line::Balance => break,
            Line::Other => current = None,
            Line::Data => {
                if let Some(parsed_row) = current.and_then(|s| parse_row(s, first, row)) {
                    parsed.rows.push(parsed_row);
                }
            }
        }
    }
    let header_ok = values
        .first()
        .and_then(|row| text_at(row, COL_PLANNED))
        .is_some_and(|h| h == "Previsto");
    parsed.layout_ok = header_ok && markers == MARKERS;
    parsed
}

enum Line {
    /// A block boundary, and the block that follows it, if any.
    Boundary(Option<Section>),
    Balance,
    /// `Total Despesas`: ends a block without being one of its markers.
    Other,
    Data,
}

fn line_kind(first: &str) -> Line {
    if first == Section::Expenses.total_label() {
        Line::Boundary(Some(Section::Cards))
    } else if first == Section::Cards.total_label() || first == Section::Income.total_label() {
        Line::Boundary(None)
    } else if first == INCOME_TITLE {
        Line::Boundary(Some(Section::Income))
    } else if first == BALANCE_LABEL {
        Line::Balance
    } else if first == EXPENSES_TOTAL {
        Line::Other
    } else {
        Line::Data
    }
}

/// A data line, or `None` for a blank one.
fn parse_row(section: Section, item: String, row: &[Value]) -> Option<ParsedRow> {
    let key = text_at(row, ID_COLUMN);
    let snapshot = RowSnapshot {
        item,
        planned: decimal_at(row, COL_PLANNED),
        day: day_at(row),
        repeats: repeats_at(row),
        category_id: text_at(row, COL_CATEGORY),
        account_id: text_at(row, COL_ACCOUNT),
        notes: text_at(row, COL_NOTES),
    };
    let blank = key.is_none() && snapshot.item.is_empty() && snapshot.planned.is_none();
    (!blank).then(|| ParsedRow {
        section,
        key,
        snapshot,
        cancelled: text_at(row, COL_STATUS).as_deref() == Some(CANCELLED),
    })
}

fn text_at(row: &[Value], col: usize) -> Option<String> {
    let text = match row.get(col)? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!text.is_empty()).then_some(text)
}

fn decimal_at(row: &[Value], col: usize) -> Option<Decimal> {
    let value = match row.get(col)? {
        Value::Number(n) => n.to_string().parse::<Decimal>().ok()?,
        Value::String(s) => decimal_from_str(s.trim_start_matches("R$").trim()).ok()?,
        _ => return None,
    };
    Some(value.abs().round_dp(2))
}

fn whole_at(row: &[Value], col: usize) -> Option<u32> {
    decimal_at(row, col)?
        .trunc()
        .to_string()
        .parse::<u32>()
        .ok()
}

fn day_at(row: &[Value]) -> Option<u32> {
    whole_at(row, COL_DAY).filter(|d| (1..=31).contains(d))
}

fn repeats_at(row: &[Value]) -> bool {
    match row.get(COL_REPEATS) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(
            s.trim().to_lowercase().as_str(),
            MONTHLY | "sim" | "s" | "x" | "true"
        ),
        _ => false,
    }
}

const MANIFEST_HEADERS: [&str; 11] = [
    "aba",
    "chave",
    "bloco",
    "item",
    "previsto",
    "dia",
    "repete",
    "categoria",
    "conta",
    "obs",
    "editavel",
];

pub fn render_manifest(entries: &[ManifestEntry]) -> Vec<Vec<Cell>> {
    let mut cells = vec![MANIFEST_HEADERS.iter().map(|h| Cell::text(*h)).collect()];
    cells.extend(entries.iter().map(|e| {
        let s = &e.snapshot;
        vec![
            Cell::text(&e.tab),
            Cell::text(&e.key),
            Cell::text(e.section.title()),
            Cell::text(&s.item),
            Cell::opt_number(s.planned),
            s.day
                .map_or(Cell::Empty, |d| Cell::Number(Decimal::from(d))),
            Cell::text(if s.repeats { MONTHLY } else { "" }),
            Cell::opt_text(s.category_id.as_deref()),
            Cell::opt_text(s.account_id.as_deref()),
            Cell::opt_text(s.notes.as_deref()),
            Cell::text(if e.editable { "sim" } else { "nao" }),
        ]
    }));
    cells
}

pub fn parse_manifest(values: &[Vec<Value>]) -> Vec<ManifestEntry> {
    values
        .iter()
        .skip(1)
        .filter_map(|row| {
            let section = Section::from_title(&text_at(row, 2)?)?;
            Some(ManifestEntry {
                tab: text_at(row, 0)?,
                key: text_at(row, 1)?,
                section,
                snapshot: RowSnapshot {
                    item: text_at(row, 3).unwrap_or_default(),
                    planned: decimal_at(row, 4),
                    day: whole_at(row, 5),
                    repeats: text_at(row, 6).as_deref() == Some(MONTHLY),
                    category_id: text_at(row, 7),
                    account_id: text_at(row, 8),
                    notes: text_at(row, 9),
                },
                editable: text_at(row, 10).as_deref() == Some("sim"),
            })
        })
        .collect()
}

/// The dropdown sources: category ids in `A`, account ids in `B`.
pub fn render_lists(categories: &[String], accounts: &[String]) -> Vec<Vec<Cell>> {
    let rows = categories.len().max(accounts.len());
    let mut cells = vec![vec![Cell::text("categoria"), Cell::text("conta")]];
    for i in 0..rows {
        cells.push(vec![
            Cell::opt_text(categories.get(i).map(String::as_str)),
            Cell::opt_text(accounts.get(i).map(String::as_str)),
        ]);
    }
    cells
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::planning_sheet::layout::render_tab;
    use crate::planning_sheet::{MonthTab, SheetRow};
    use chrono::NaiveDate;
    use serde_json::json;

    fn row(section: Section, key: &str, planned: i64) -> SheetRow {
        SheetRow {
            section,
            key: key.to_string(),
            item: format!("item {key}"),
            planned: Some(Decimal::new(planned, 0)),
            actual: None,
            status: "Previsto".to_string(),
            day: Some(5),
            repeats: key == "rent",
            category_id: Some("moradia:aluguel".to_string()),
            account_id: None,
            notes: None,
            editable: true,
        }
    }

    fn tab() -> MonthTab {
        MonthTab {
            month: NaiveDate::from_ymd_opt(2030, 5, 1).unwrap(),
            rows: vec![
                row(Section::Expenses, "rent", 5000),
                row(Section::Cards, "card:c1", 4000),
                row(Section::Income, "pay", 9000),
            ],
            installments: Vec::new(),
            debit: crate::planning_sheet::EnvelopeBlock {
                title: "Débito / Dinheiro".to_string(),
                planned: Decimal::ZERO,
                actual: Decimal::ZERO,
                lines: Vec::new(),
            },
            envelopes: Vec::new(),
        }
    }

    /// What the Sheets API hands back for a rendered grid.
    pub(crate) fn as_values(cells: &[Vec<Cell>]) -> Vec<Vec<Value>> {
        cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| match c {
                        Cell::Empty => json!(""),
                        Cell::Text(t) | Cell::Formula(t) => json!(t),
                        Cell::Number(n) => serde_json::from_str(&n.to_string()).unwrap(),
                    })
                    .collect()
            })
            .collect()
    }

    fn rendered() -> Vec<Vec<Value>> {
        as_values(&render_tab(&tab(), &format!("{SIGNATURE} em 01/05/2030")).cells)
    }

    fn row_of(values: &[Vec<Value>], label: &str) -> usize {
        values
            .iter()
            .position(|r| r.first() == Some(&json!(label)))
            .unwrap()
    }

    #[test]
    fn parse_reads_back_what_render_wrote() {
        let parsed = parse_tab(&rendered());
        assert!(parsed.layout_ok);
        let keys: Vec<_> = parsed.rows.iter().map(|r| r.key.clone().unwrap()).collect();
        assert_eq!(keys, vec!["rent", "card:c1", "pay"]);
        let sections: Vec<_> = parsed.rows.iter().map(|r| r.section).collect();
        assert_eq!(sections, Section::ALL.to_vec());
        let rent = &parsed.rows[0].snapshot;
        assert_eq!(rent.planned, Some(Decimal::new(5000, 0)));
        assert_eq!(rent.day, Some(5));
        assert!(rent.repeats);
        assert_eq!(rent.category_id.as_deref(), Some("moradia:aluguel"));
    }

    #[test]
    fn parse_picks_up_a_typed_row_and_ignores_blank_lines() {
        let mut values = rendered();
        let typed = row_of(&values, "Subtotal fixas") - 1;
        values[typed] = vec![
            json!("Natação"),
            json!("440,00"),
            json!(""),
            json!(""),
            json!(12),
        ];
        let parsed = parse_tab(&values);
        let typed = &parsed.rows[1];
        assert_eq!(typed.section, Section::Expenses);
        assert_eq!(typed.key, None);
        assert_eq!(typed.snapshot.item, "Natação");
        assert_eq!(typed.snapshot.planned, Some(Decimal::new(440, 0)));
        assert_eq!(typed.snapshot.day, Some(12));
    }

    #[test]
    fn cancelado_in_status_marks_the_row() {
        let mut values = rendered();
        values[1][COL_STATUS] = json!("Cancelado");
        assert!(parse_tab(&values).rows[0].cancelled);
    }

    #[test]
    fn a_renamed_block_end_breaks_the_layout() {
        let mut values = rendered();
        let cards_total = row_of(&values, "Total Cartão");
        values[cards_total][0] = json!("Cartões");
        assert!(!parse_tab(&values).layout_ok);
    }

    #[test]
    fn phai_tabs_are_recognised_by_the_note_in_t1() {
        assert!(is_phai_tab(&rendered()));
        assert!(!is_phai_tab(&[vec![json!("meu rascunho")]]));
        assert!(!is_phai_tab(&[]));
    }

    #[test]
    fn manifest_round_trips() {
        let entries = vec![ManifestEntry {
            tab: "MAIO 2030".to_string(),
            key: "rent".to_string(),
            section: Section::Expenses,
            snapshot: RowSnapshot {
                item: "Aluguel".to_string(),
                planned: Some(Decimal::new(500025, 2)),
                day: Some(3),
                repeats: true,
                category_id: Some("moradia:aluguel".to_string()),
                account_id: Some("conta_corrente".to_string()),
                notes: Some("reajuste em março".to_string()),
            },
            editable: true,
        }];
        let values = as_values(&render_manifest(&entries));
        assert_eq!(parse_manifest(&values), entries);
    }

    #[test]
    fn lists_pad_the_shorter_column() {
        let cells = render_lists(&["a".to_string(), "b".to_string()], &["x".to_string()]);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[2], vec![Cell::text("b"), Cell::Empty]);
    }
}
