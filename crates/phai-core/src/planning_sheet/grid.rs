//! Lay a [`MonthTab`] out as spreadsheet cells and read edited cells back.
//!
//! Columns: `Item | Previsto | Realizado | Status | Dia | Repete | Categoria |
//! Conta | Obs. | id`, then the category actuals in `L:M`. Section totals
//! and the month balance are formulas, so they stay live while a person
//! edits between syncs.

use rust_decimal::Decimal;
use serde_json::Value;

use super::diff::{ManifestEntry, RowSnapshot};
use super::{MonthTab, Section, SheetRow, MONTHLY};
use crate::models::decimal_from_str;

pub const HEADERS: [&str; 10] = [
    "Item",
    "Previsto",
    "Realizado",
    "Status",
    "Dia",
    "Repete",
    "Categoria",
    "Conta",
    "Obs.",
    "id",
];
const BALANCE_LABEL: &str = "SALDO DO MÊS";
const ACTUALS_TITLE: &str = "REALIZADO POR CATEGORIA";
/// Column index of the category actuals block (`L`).
const ACTUALS_COL: usize = 11;
/// Rows above the first section: title and header.
const TOP_ROWS: usize = 2;

const COL_ITEM: usize = 0;
const COL_PLANNED: usize = 1;
const COL_DAY: usize = 4;
const COL_REPEATS: usize = 5;
const COL_CATEGORY: usize = 6;
const COL_ACCOUNT: usize = 7;
const COL_NOTES: usize = 8;
const COL_KEY: usize = 9;

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    Text(String),
    Number(Decimal),
    Formula(String),
}

impl Cell {
    fn text(value: impl Into<String>) -> Cell {
        Cell::Text(value.into())
    }

    fn opt_text(value: Option<&str>) -> Cell {
        value.map_or(Cell::Empty, Cell::text)
    }

    fn opt_number(value: Option<Decimal>) -> Cell {
        value.map_or(Cell::Empty, |v| Cell::Number(v.round_dp(2)))
    }
}

/// A rendered tab plus the 0-based rows that should be bold (headers,
/// section titles, totals and the balance).
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedTab {
    pub cells: Vec<Vec<Cell>>,
    pub bold_rows: Vec<usize>,
}

pub fn render_tab(tab: &MonthTab, updated_label: &str) -> RenderedTab {
    let mut cells: Vec<Vec<Cell>> = vec![
        vec![
            Cell::text(tab.title()),
            Cell::Empty,
            Cell::text(updated_label),
        ],
        HEADERS.iter().map(|h| Cell::text(*h)).collect(),
    ];
    let mut bold_rows = vec![0, 1];
    let mut totals = Vec::new();
    for section in Section::ALL {
        bold_rows.push(cells.len());
        cells.push(vec![Cell::text(section.title())]);
        let first = cells.len() + 1;
        cells.extend(
            tab.rows
                .iter()
                .filter(|r| r.section == section)
                .map(row_cells),
        );
        // One empty line per block, so there is always room to type a new row.
        cells.push(Vec::new());
        let total_row = cells.len() + 1;
        bold_rows.push(cells.len());
        cells.push(vec![
            Cell::text(section.total_label()),
            sum_formula('B', first, total_row - 1),
            sum_formula('C', first, total_row - 1),
        ]);
        cells.push(Vec::new());
        totals.push(total_row);
    }
    bold_rows.push(cells.len());
    let (expenses, cards, income) = (totals[0], totals[1], totals[2]);
    cells.push(vec![
        Cell::text(BALANCE_LABEL),
        Cell::Formula(format!("=B{income}-B{expenses}-B{cards}")),
        Cell::Formula(format!("=C{income}-C{expenses}-C{cards}")),
    ]);
    place_actuals(&mut cells, &tab.category_actuals);
    RenderedTab { cells, bold_rows }
}

fn sum_formula(col: char, first: usize, last: usize) -> Cell {
    Cell::Formula(format!("=SUM({col}{first}:{col}{last})"))
}

fn row_cells(row: &SheetRow) -> Vec<Cell> {
    vec![
        Cell::text(&row.item),
        Cell::opt_number(row.planned),
        Cell::opt_number(row.actual),
        Cell::text(&row.status),
        row.day
            .map_or(Cell::Empty, |d| Cell::Number(Decimal::from(d))),
        if row.repeats {
            Cell::text(MONTHLY)
        } else {
            Cell::Empty
        },
        Cell::opt_text(row.category_id.as_deref()),
        Cell::opt_text(row.account_id.as_deref()),
        Cell::opt_text(row.notes.as_deref()),
        Cell::text(&row.key),
    ]
}

fn place_actuals(cells: &mut Vec<Vec<Cell>>, actuals: &[(String, Decimal)]) {
    let mut block = vec![vec![Cell::text(ACTUALS_TITLE), Cell::text("Valor")]];
    block.extend(
        actuals
            .iter()
            .map(|(label, value)| vec![Cell::text(label), Cell::Number(value.round_dp(2))]),
    );
    let last = TOP_ROWS + actuals.len();
    block.push(vec![
        Cell::text("Total"),
        Cell::Formula(format!("=SUM(M{}:M{last})", TOP_ROWS + 1)),
    ]);
    for (offset, pair) in block.into_iter().enumerate() {
        let row_index = offset + 1;
        while cells.len() <= row_index {
            cells.push(Vec::new());
        }
        let row = &mut cells[row_index];
        while row.len() < ACTUALS_COL {
            row.push(Cell::Empty);
        }
        row.truncate(ACTUALS_COL);
        row.extend(pair);
    }
}

/// A data line read back from a month tab.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRow {
    pub section: Section,
    /// The `id` column; `None` for a row a person just typed.
    pub key: Option<String>,
    pub snapshot: RowSnapshot,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParsedTab {
    /// How many of the three section titles were found. Anything short of
    /// three means the layout was broken by hand and the tab is not trusted.
    pub sections_found: usize,
    pub rows: Vec<ParsedRow>,
}

pub fn parse_tab(values: &[Vec<Value>]) -> ParsedTab {
    let mut parsed = ParsedTab::default();
    let mut current: Option<Section> = None;
    for row in values.iter().skip(TOP_ROWS) {
        let first = text_at(row, COL_ITEM).unwrap_or_default();
        match line_kind(&first) {
            Line::Title(section) => {
                current = Some(section);
                parsed.sections_found += 1;
            }
            Line::Total => current = None,
            Line::Balance => break,
            Line::Data => {
                if let Some(parsed_row) = current.and_then(|s| parse_row(s, first, row)) {
                    parsed.rows.push(parsed_row);
                }
            }
        }
    }
    parsed
}

enum Line {
    Title(Section),
    Total,
    Balance,
    Data,
}

fn line_kind(first: &str) -> Line {
    if let Some(section) = Section::from_title(first) {
        Line::Title(section)
    } else if first.starts_with("Total ") {
        Line::Total
    } else if first == BALANCE_LABEL {
        Line::Balance
    } else {
        Line::Data
    }
}

/// A data line, or `None` for a blank one.
fn parse_row(section: Section, item: String, row: &[Value]) -> Option<ParsedRow> {
    let key = text_at(row, COL_KEY);
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
    (!blank).then_some(ParsedRow {
        section,
        key,
        snapshot,
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

fn day_at(row: &[Value]) -> Option<u32> {
    let day = decimal_at(row, COL_DAY)?
        .trunc()
        .to_string()
        .parse::<u32>()
        .ok()?;
    (1..=31).contains(&day).then_some(day)
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
                    day: decimal_at(row, 5).and_then(|d| d.trunc().to_string().parse::<u32>().ok()),
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
mod tests {
    use super::*;
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
            category_actuals: vec![("Moradia".to_string(), Decimal::new(4800, 0))],
        }
    }

    /// What the Sheets API hands back for a rendered grid (formulas as
    /// computed numbers are irrelevant to parsing, so they become strings).
    fn as_values(cells: &[Vec<Cell>]) -> Vec<Vec<Value>> {
        cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| match c {
                        Cell::Empty => json!(""),
                        Cell::Text(t) => json!(t),
                        Cell::Number(n) => serde_json::from_str(&n.to_string()).unwrap(),
                        Cell::Formula(f) => json!(f),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn render_lays_out_sections_totals_and_balance() {
        let rendered = render_tab(&tab(), "atualizado");
        let col_a: Vec<String> = rendered
            .cells
            .iter()
            .map(|r| match r.first() {
                Some(Cell::Text(t)) => t.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(
            col_a,
            vec![
                "MAIO 2030",
                "Item",
                "DESPESAS",
                "item rent",
                "",
                "Total despesas",
                "",
                "CARTÕES",
                "item card:c1",
                "",
                "Total cartões",
                "",
                "RECEITAS",
                "item pay",
                "",
                "Total receitas",
                "",
                "SALDO DO MÊS",
            ]
        );
        assert_eq!(
            rendered.cells[5][1],
            Cell::Formula("=SUM(B4:B5)".to_string())
        );
        assert_eq!(
            rendered.cells[17][1],
            Cell::Formula("=B16-B6-B11".to_string())
        );
        assert_eq!(rendered.bold_rows, vec![0, 1, 2, 5, 7, 10, 12, 15, 17]);
    }

    #[test]
    fn render_places_category_actuals_beside_the_table() {
        let rendered = render_tab(&tab(), "atualizado");
        assert_eq!(rendered.cells[1][11], Cell::text("REALIZADO POR CATEGORIA"));
        assert_eq!(rendered.cells[2][11], Cell::text("Moradia"));
        assert_eq!(rendered.cells[2][12], Cell::Number(Decimal::new(4800, 0)));
        assert_eq!(
            rendered.cells[3][12],
            Cell::Formula("=SUM(M3:M3)".to_string())
        );
        assert_eq!(
            rendered.cells[3][0],
            Cell::text("item rent"),
            "left side intact"
        );
    }

    #[test]
    fn parse_reads_back_what_render_wrote() {
        let values = as_values(&render_tab(&tab(), "atualizado").cells);
        let parsed = parse_tab(&values);
        assert_eq!(parsed.sections_found, 3);
        let keys: Vec<_> = parsed.rows.iter().map(|r| r.key.clone().unwrap()).collect();
        assert_eq!(keys, vec!["rent", "card:c1", "pay"]);
        let rent = &parsed.rows[0].snapshot;
        assert_eq!(rent.planned, Some(Decimal::new(5000, 0)));
        assert_eq!(rent.day, Some(5));
        assert!(rent.repeats);
        assert_eq!(rent.category_id.as_deref(), Some("moradia:aluguel"));
    }

    #[test]
    fn parse_picks_up_a_typed_row_and_ignores_blank_lines() {
        let mut values = as_values(&render_tab(&tab(), "atualizado").cells);
        // Type into the empty line under the expense row.
        values[4] = vec![
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
    fn parse_counts_missing_sections() {
        let mut values = as_values(&render_tab(&tab(), "atualizado").cells);
        values[7] = vec![json!("cartoes renomeado")];
        assert_eq!(parse_tab(&values).sections_found, 2);
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
