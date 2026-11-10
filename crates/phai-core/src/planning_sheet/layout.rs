//! Lay a [`MonthTab`] out the way the household's own month tab looks.
//!
//! `A:E` is the month table (`Item | Previsto | Realizado | Status | Ve.`):
//! fixed bills and `Subtotal fixas`, one row per card and `Total Cartão`,
//! `Total Despesas`, `RECEITAS` and `Total Receitas`, `SALDO MES`, and the
//! cash gap before the 15th. `G:L` lists the month's card installments,
//! `N:S` holds one block per category budget, three per band. `V:Z` carry
//! the fields phai needs to round-trip a row (`Repete`, `Categoria`,
//! `Conta`, `Obs.`, hidden `id`). Every total is a formula, so the tab stays
//! live while a person edits between syncs.

use rust_decimal::Decimal;

use super::grid::Cell;
use super::{month_name, MonthTab, Section, SheetRow, MONTHLY};

pub(super) const COL_ITEM: usize = 0;
pub(super) const COL_PLANNED: usize = 1;
pub(super) const COL_ACTUAL: usize = 2;
pub(super) const COL_STATUS: usize = 3;
pub(super) const COL_DAY: usize = 4;
const PARCEL_COL: usize = 6;
const ENVELOPE_COL: usize = 13;
/// `T1`: phai's note, which also marks the tab as phai's.
pub(super) const SIGNATURE_COL: usize = 19;
pub(super) const COL_REPEATS: usize = 21;
pub(super) const COL_CATEGORY: usize = 22;
pub(super) const COL_ACCOUNT: usize = 23;
pub(super) const COL_NOTES: usize = 24;
/// `Z`: the forecast id (hidden).
pub const ID_COLUMN: usize = 25;
const COLUMNS: usize = ID_COLUMN + 1;

/// Pixel widths of `A:Z`, matching the household's month tab.
pub const COLUMN_WIDTHS: [u32; COLUMNS] = [
    324, 108, 108, 149, 92, 50, 284, 51, 24, 24, 24, 77, 59, 92, 92, 92, 92, 92, 92, 139, 30, 70,
    190, 120, 220, 60,
];

pub(super) const INCOME_TITLE: &str = "RECEITAS";
pub(super) const EXPENSES_TOTAL: &str = "Total Despesas";
pub(super) const BALANCE_LABEL: &str = "SALDO MES";
/// Values the `Status` column accepts. `Cancelado` removes the row.
pub const STATUSES: [&str; 5] = ["Pago", "Agendado", "Previsto", "Atrasado", "Cancelado"];
pub(super) const CANCELLED: &str = "Cancelado";
/// Day of the month that splits the first and second half of the cash flow.
const MID_MONTH: u32 = 15;
/// Colours cycle through this many category blocks.
pub const BLOCK_PALETTE: u8 = 6;

/// Half-open cell range, 0-based.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub rows: (usize, usize),
    pub cols: (usize, usize),
}

impl Range {
    fn cells(row: usize, cols: (usize, usize)) -> Self {
        Range {
            rows: (row, row + 1),
            cols,
        }
    }
}

/// What a range looks like; [`crate::google_sheets`] turns it into colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Header,
    Fixed,
    /// Bold item names (fixed bills).
    Label,
    FixedTotal,
    Card,
    CardMoney,
    ExpenseTotal,
    IncomeHeader,
    Income,
    IncomeTotal,
    Balance,
    FlowHeader,
    Flow,
    FlowGap,
    Money,
    /// `R$ 1.234,56`, as the category blocks show money.
    Currency,
    Day,
    Status,
    ParcelTitle,
    ParcelHeader,
    ParcelBody,
    ParcelTotal,
    DebitTitle,
    BlockTitle(u8),
    BlockBody(u8),
    BlockTotal(u8),
    BudgetTotal,
    Meta,
    Note,
}

/// Dropdowns: the status vocabulary, `mensal`, and the lists tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    Status,
    Repeats,
    Category,
    Account,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RenderedTab {
    pub cells: Vec<Vec<Cell>>,
    pub styles: Vec<(Style, Range)>,
    pub merges: Vec<Range>,
    pub validations: Vec<(Validation, Range)>,
}

impl RenderedTab {
    fn set(&mut self, row: usize, col: usize, cell: Cell) {
        while self.cells.len() <= row {
            self.cells.push(Vec::new());
        }
        let line = &mut self.cells[row];
        while line.len() <= col {
            line.push(Cell::Empty);
        }
        line[col] = cell;
    }

    fn style(&mut self, style: Style, range: Range) {
        self.styles.push((style, range));
    }

    fn validate(&mut self, rows: (usize, usize), checks: &[(Validation, usize)]) {
        for (validation, col) in checks {
            self.validations.push((
                *validation,
                Range {
                    rows,
                    cols: (*col, col + 1),
                },
            ));
        }
    }
}

/// `B5` for row index 4, column index 1.
fn a1(col: usize, row: usize) -> String {
    format!("{}{}", (b'A' + col as u8) as char, row + 1)
}

pub fn render_tab(tab: &MonthTab, note: &str) -> RenderedTab {
    let mut out = RenderedTab::default();
    header(&mut out, tab, note);
    let main = month_table(&mut out, &tab.rows);
    cash_flow(&mut out, &main);
    let below = installments(&mut out, tab);
    debit(&mut out, tab, below + 2);
    envelopes(&mut out, tab);
    out
}

fn header(out: &mut RenderedTab, tab: &MonthTab, note: &str) {
    let titles = [
        month_name(tab.month),
        "Previsto",
        "Realizado",
        "Status",
        "Ve.",
    ];
    for (col, title) in titles.iter().enumerate() {
        out.set(0, col, Cell::text(*title));
    }
    out.style(Style::Header, Range::cells(0, (0, 5)));
    for (col, title) in [
        (COL_REPEATS, "Repete"),
        (COL_CATEGORY, "Categoria"),
        (COL_ACCOUNT, "Conta"),
        (COL_NOTES, "Obs."),
        (ID_COLUMN, "id"),
    ] {
        out.set(0, col, Cell::text(title));
    }
    out.style(Style::Meta, Range::cells(0, (COL_REPEATS, COLUMNS)));
    out.set(0, SIGNATURE_COL, Cell::text(note));
    out.style(
        Style::Note,
        Range::cells(0, (SIGNATURE_COL, SIGNATURE_COL + 1)),
    );
}

/// 0-based rows of the month table's blocks and totals.
struct MainRows {
    fixed: (usize, usize),
    cards: (usize, usize),
    income: (usize, usize),
    income_total: usize,
}

fn month_table(out: &mut RenderedTab, rows: &[SheetRow]) -> MainRows {
    let fixed = block(out, rows, Section::Expenses, 1, Style::Fixed);
    let subtotal = fixed.1;
    total_row(
        out,
        subtotal,
        Section::Expenses.total_label(),
        fixed,
        Style::FixedTotal,
    );

    let cards = block(out, rows, Section::Cards, subtotal + 2, Style::Card);
    let cards_total = cards.1;
    total_row(
        out,
        cards_total,
        Section::Cards.total_label(),
        cards,
        Style::Card,
    );
    out.style(
        Style::CardMoney,
        Range {
            rows: (cards.0, cards_total + 1),
            cols: (1, 3),
        },
    );

    let expenses_total = cards_total + 2;
    out.set(expenses_total, COL_ITEM, Cell::text(EXPENSES_TOTAL));
    for col in [COL_PLANNED, COL_ACTUAL] {
        out.set(
            expenses_total,
            col,
            Cell::Formula(format!("={}+{}", a1(col, subtotal), a1(col, cards_total))),
        );
    }
    out.style(Style::ExpenseTotal, Range::cells(expenses_total, (0, 3)));
    out.style(Style::Money, Range::cells(expenses_total, (1, 3)));

    let income_header = expenses_total + 3;
    out.set(income_header, COL_ITEM, Cell::text(INCOME_TITLE));
    out.set(income_header, COL_STATUS, Cell::text("Status"));
    out.style(Style::IncomeHeader, Range::cells(income_header, (0, 5)));
    let income = block(out, rows, Section::Income, income_header + 1, Style::Income);
    let income_total = income.1;
    total_row(
        out,
        income_total,
        Section::Income.total_label(),
        income,
        Style::IncomeTotal,
    );

    let balance = income_total + 2;
    out.set(balance, COL_ITEM, Cell::text(BALANCE_LABEL));
    for col in [COL_PLANNED, COL_ACTUAL] {
        out.set(
            balance,
            col,
            Cell::Formula(format!(
                "={}-{}",
                a1(col, income_total),
                a1(col, expenses_total)
            )),
        );
    }
    out.style(Style::Balance, Range::cells(balance, (0, 3)));
    out.style(Style::Money, Range::cells(balance, (1, 3)));
    MainRows {
        fixed,
        cards,
        income,
        income_total,
    }
}

/// Write a block's rows from `start`, plus one empty line to type into.
/// Returns the half-open row range, whose end is the total row.
fn block(
    out: &mut RenderedTab,
    rows: &[SheetRow],
    section: Section,
    start: usize,
    style: Style,
) -> (usize, usize) {
    let mut row = start;
    for line in rows.iter().filter(|r| r.section == section) {
        row_cells(out, row, line);
        row += 1;
    }
    row += 1;
    out.style(
        style,
        Range {
            rows: (start, row),
            cols: (0, 5),
        },
    );
    if section == Section::Expenses {
        out.style(
            Style::Label,
            Range {
                rows: (start, row),
                cols: (0, 1),
            },
        );
    }
    if section != Section::Cards {
        out.style(
            Style::Money,
            Range {
                rows: (start, row),
                cols: (1, 3),
            },
        );
        out.validate(
            (start, row),
            &[
                (Validation::Status, COL_STATUS),
                (Validation::Repeats, COL_REPEATS),
                (Validation::Category, COL_CATEGORY),
                (Validation::Account, COL_ACCOUNT),
            ],
        );
    }
    out.style(
        Style::Status,
        Range {
            rows: (start, row),
            cols: (3, 4),
        },
    );
    out.style(
        Style::Day,
        Range {
            rows: (start, row),
            cols: (4, 5),
        },
    );
    (start, row)
}

fn total_row(out: &mut RenderedTab, row: usize, label: &str, span: (usize, usize), style: Style) {
    out.set(row, COL_ITEM, Cell::text(label));
    for col in [COL_PLANNED, COL_ACTUAL] {
        out.set(
            row,
            col,
            Cell::Formula(format!("=SUM({}:{})", a1(col, span.0), a1(col, span.1 - 1))),
        );
    }
    out.style(style, Range::cells(row, (0, 3)));
    out.style(Style::Money, Range::cells(row, (1, 3)));
}

fn row_cells(out: &mut RenderedTab, row: usize, line: &SheetRow) {
    out.set(row, COL_ITEM, Cell::text(&line.item));
    out.set(row, COL_PLANNED, Cell::opt_number(line.planned));
    out.set(row, COL_ACTUAL, Cell::opt_number(line.actual));
    out.set(row, COL_STATUS, Cell::text(&line.status));
    out.set(
        row,
        COL_DAY,
        line.day
            .map_or(Cell::Empty, |d| Cell::Number(Decimal::from(d))),
    );
    if line.repeats {
        out.set(row, COL_REPEATS, Cell::text(MONTHLY));
    }
    out.set(
        row,
        COL_CATEGORY,
        Cell::opt_text(line.category_id.as_deref()),
    );
    out.set(row, COL_ACCOUNT, Cell::opt_text(line.account_id.as_deref()));
    out.set(row, COL_NOTES, Cell::opt_text(line.notes.as_deref()));
    out.set(row, ID_COLUMN, Cell::text(&line.key));
}

/// `Fluxo Caixa`: what comes in by the 15th against what goes out before it.
fn cash_flow(out: &mut RenderedTab, main: &MainRows) {
    let top = main.income_total + 4;
    out.set(top, COL_ITEM, Cell::text("Fluxo Caixa"));
    out.style(Style::FlowHeader, Range::cells(top, (0, 3)));
    let early = |col: usize, span: (usize, usize), op: &str| {
        format!(
            "SUMPRODUCT(({}:{}{op}{MID_MONTH})*{}:{})",
            a1(COL_DAY, span.0),
            a1(COL_DAY, span.1 - 1),
            a1(col, span.0),
            a1(col, span.1 - 1),
        )
    };
    let (income, outgo, gap) = (top + 1, top + 2, top + 3);
    out.set(income, COL_ITEM, Cell::text("Receitas inicio mes"));
    out.set(
        outgo,
        COL_ITEM,
        Cell::text(format!("Despesas antes dia {MID_MONTH}")),
    );
    out.set(gap, COL_ITEM, Cell::text("Gap inicio mes"));
    for col in [COL_PLANNED, COL_ACTUAL] {
        out.set(
            income,
            col,
            Cell::Formula(format!("={}", early(col, main.income, "<="))),
        );
        out.set(
            outgo,
            col,
            Cell::Formula(format!(
                "={}+{}",
                early(col, main.fixed, "<"),
                early(col, main.cards, "<")
            )),
        );
        out.set(
            gap,
            col,
            Cell::Formula(format!("={}-{}", a1(col, income), a1(col, outgo))),
        );
    }
    out.style(
        Style::Flow,
        Range {
            rows: (income, gap + 1),
            cols: (0, 3),
        },
    );
    out.style(
        Style::Money,
        Range {
            rows: (income, gap + 1),
            cols: (1, 3),
        },
    );
    out.style(Style::FlowGap, Range::cells(gap, (1, 3)));
}

/// Returns the row of the block's total.
fn installments(out: &mut RenderedTab, tab: &MonthTab) -> usize {
    let span = (PARCEL_COL, PARCEL_COL + 6);
    out.set(0, PARCEL_COL, Cell::text("Parcelamentos Cartão"));
    out.merges.push(Range::cells(0, span));
    out.style(Style::ParcelTitle, Range::cells(0, span));
    for (offset, title) in ["Conta", "C", "P", "T", "R", "Valor"].iter().enumerate() {
        out.set(1, PARCEL_COL + offset, Cell::text(*title));
    }
    out.style(Style::ParcelHeader, Range::cells(1, span));
    let mut row = 2;
    for line in &tab.installments {
        out.set(row, PARCEL_COL, Cell::text(&line.item));
        out.set(row, PARCEL_COL + 1, Cell::text(&line.card));
        out.set(
            row,
            PARCEL_COL + 2,
            Cell::Number(Decimal::from(line.number)),
        );
        out.set(row, PARCEL_COL + 3, Cell::Number(Decimal::from(line.total)));
        out.set(
            row,
            PARCEL_COL + 4,
            Cell::Number(Decimal::from(line.total.saturating_sub(line.number))),
        );
        out.set(row, PARCEL_COL + 5, Cell::Number(line.amount.round_dp(2)));
        row += 1;
    }
    let last = row.max(3);
    out.style(
        Style::ParcelBody,
        Range {
            rows: (2, last),
            cols: span,
        },
    );
    let value = PARCEL_COL + 5;
    out.style(
        Style::Money,
        Range {
            rows: (2, last + 1),
            cols: (value, value + 1),
        },
    );
    out.set(last, PARCEL_COL, Cell::text("Total"));
    out.set(
        last,
        value,
        Cell::Formula(format!("=SUM({}:{})", a1(value, 2), a1(value, last - 1))),
    );
    out.style(Style::ParcelTotal, Range::cells(last, span));
    last
}

/// `Débito / Dinheiro` under the installments: one purchase per line and
/// what the month spent from the accounts.
fn debit(out: &mut RenderedTab, tab: &MonthTab, top: usize) {
    let span = (PARCEL_COL, PARCEL_COL + 6);
    let value = PARCEL_COL + 5;
    out.set(top, PARCEL_COL, Cell::text(&tab.debit.title));
    out.merges.push(Range::cells(top, span));
    out.style(Style::DebitTitle, Range::cells(top, span));
    let mut row = top + 1;
    for (label, amount) in &tab.debit.lines {
        out.set(row, PARCEL_COL, Cell::text(label));
        out.set(row, value, Cell::Number(amount.round_dp(2)));
        row += 1;
    }
    let last = row.max(top + 2);
    out.style(
        Style::Fixed,
        Range {
            rows: (top + 1, last),
            cols: span,
        },
    );
    out.set(last, PARCEL_COL + 1, Cell::text("realizado:"));
    out.set(last, value, Cell::Number(tab.debit.actual.round_dp(2)));
    out.style(Style::BudgetTotal, Range::cells(last, (value, value + 1)));
    out.style(
        Style::Money,
        Range {
            rows: (top + 1, last + 1),
            cols: (value, value + 1),
        },
    );
}

/// One block per category budget, three per band: purchases, then
/// `orçado` (with what is left beside it) and `realizado`.
fn envelopes(out: &mut RenderedTab, tab: &MonthTab) {
    let mut top = 0;
    let mut planned_cells = Vec::new();
    let mut actual_cells = Vec::new();
    for (band, blocks) in tab.envelopes.chunks(3).enumerate() {
        let height = blocks
            .iter()
            .map(|b| b.lines.len())
            .max()
            .unwrap_or(0)
            .max(1);
        for (slot, block) in blocks.iter().enumerate() {
            let col = ENVELOPE_COL + 2 * slot;
            let color = ((band * 3 + slot) % BLOCK_PALETTE as usize) as u8;
            out.set(top, col, Cell::text(&block.title));
            out.merges.push(Range::cells(top, (col, col + 2)));
            out.style(Style::BlockTitle(color), Range::cells(top, (col, col + 2)));
            for (i, (label, value)) in block.lines.iter().enumerate() {
                out.set(top + 1 + i, col, Cell::text(label));
                out.set(top + 1 + i, col + 1, Cell::Number(value.round_dp(2)));
            }
            let body = Range {
                rows: (top + 1, top + 1 + height),
                cols: (col, col + 2),
            };
            out.style(Style::BlockBody(color), body);
            out.style(
                Style::Money,
                Range {
                    rows: body.rows,
                    cols: (col + 1, col + 2),
                },
            );
            let (planned, actual) = (top + 1 + height, top + 2 + height);
            out.set(planned, col, Cell::Number(block.planned.round_dp(2)));
            out.set(actual, col, Cell::Number(block.actual.round_dp(2)));
            let (p, a) = (a1(col, planned), a1(col, actual));
            out.set(
                planned,
                col + 1,
                Cell::Formula(format!("=({p}-{a})*({p}>{a})")),
            );
            out.merges.push(Range {
                rows: (planned, actual + 1),
                cols: (col + 1, col + 2),
            });
            let totals = Range {
                rows: (planned, actual + 1),
                cols: (col, col + 2),
            };
            out.style(Style::BlockTotal(color), totals);
            out.style(Style::Currency, totals);
            planned_cells.push(p);
            actual_cells.push(a);
        }
        top += height + 4;
    }
    if planned_cells.is_empty() {
        return;
    }
    let label_col = ENVELOPE_COL + 3;
    for (row, label, cells) in [
        (top, "orçado:", &planned_cells),
        (top + 1, "realizado:", &actual_cells),
    ] {
        out.set(row, label_col, Cell::text(label));
        out.set(
            row,
            label_col + 1,
            Cell::Formula(format!("={}", cells.join("+"))),
        );
        out.style(
            Style::BudgetTotal,
            Range::cells(row, (label_col, label_col + 2)),
        );
        out.style(
            Style::Money,
            Range::cells(row, (label_col + 1, label_col + 2)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planning_sheet::{EnvelopeBlock, InstallmentLine};
    use chrono::NaiveDate;

    fn row(section: Section, key: &str, planned: i64, day: u32) -> SheetRow {
        SheetRow {
            section,
            key: key.to_string(),
            item: format!("item {key}"),
            planned: Some(Decimal::new(planned, 0)),
            actual: None,
            status: "Previsto".to_string(),
            day: Some(day),
            repeats: key == "rent",
            category_id: Some("moradia:aluguel".to_string()),
            account_id: None,
            notes: None,
            editable: true,
        }
    }

    pub(crate) fn tab() -> MonthTab {
        MonthTab {
            month: NaiveDate::from_ymd_opt(2030, 5, 1).unwrap(),
            rows: vec![
                row(Section::Expenses, "rent", 5000, 3),
                row(Section::Cards, "card:c1", 4000, 17),
                row(Section::Income, "pay", 9000, 5),
            ],
            installments: vec![InstallmentLine {
                item: "Loja".to_string(),
                card: "A".to_string(),
                number: 2,
                total: 5,
                amount: Decimal::new(110, 0),
            }],
            debit: EnvelopeBlock {
                title: "Débito / Dinheiro".to_string(),
                planned: Decimal::ZERO,
                actual: Decimal::new(350, 0),
                lines: vec![("Diarista".to_string(), Decimal::new(350, 0))],
            },
            envelopes: vec![EnvelopeBlock {
                title: "Mercado".to_string(),
                planned: Decimal::new(3000, 0),
                actual: Decimal::new(1200, 0),
                lines: vec![("Feira".to_string(), Decimal::new(80, 0))],
            }],
        }
    }

    fn col(rendered: &RenderedTab, c: usize) -> Vec<String> {
        rendered
            .cells
            .iter()
            .map(|r| match r.get(c) {
                Some(Cell::Text(t)) => t.clone(),
                Some(Cell::Formula(f)) => f.clone(),
                _ => String::new(),
            })
            .collect()
    }

    #[test]
    fn month_table_follows_the_household_layout() {
        let rendered = render_tab(&tab(), "nota");
        let a = col(&rendered, COL_ITEM);
        assert_eq!(
            &a[..23],
            &[
                "MAIO",
                "item rent",
                "",
                "Subtotal fixas",
                "",
                "item card:c1",
                "",
                "Total Cartão",
                "",
                "Total Despesas",
                "",
                "",
                "RECEITAS",
                "item pay",
                "",
                "Total Receitas",
                "",
                "SALDO MES",
                "",
                "Fluxo Caixa",
                "Receitas inicio mes",
                "Despesas antes dia 15",
                "Gap inicio mes",
            ]
        );
        let b = col(&rendered, COL_PLANNED);
        assert_eq!(b[3], "=SUM(B2:B3)");
        assert_eq!(b[7], "=SUM(B6:B7)");
        assert_eq!(b[9], "=B4+B8");
        assert_eq!(b[15], "=SUM(B14:B15)");
        assert_eq!(b[17], "=B16-B10");
        assert_eq!(b[20], "=SUMPRODUCT((E14:E15<=15)*B14:B15)");
        assert_eq!(
            b[21],
            "=SUMPRODUCT((E2:E3<15)*B2:B3)+SUMPRODUCT((E6:E7<15)*B6:B7)"
        );
        assert_eq!(b[22], "=B21-B22");
    }

    #[test]
    fn round_trip_fields_sit_in_v_to_z() {
        let rendered = render_tab(&tab(), "nota");
        let rent = &rendered.cells[1];
        assert_eq!(rent[COL_REPEATS], Cell::text(MONTHLY));
        assert_eq!(rent[COL_CATEGORY], Cell::text("moradia:aluguel"));
        assert_eq!(rent[ID_COLUMN], Cell::text("rent"));
        assert_eq!(rendered.cells[0][SIGNATURE_COL], Cell::text("nota"));
    }

    #[test]
    fn installments_fill_g_to_l_with_a_total() {
        let rendered = render_tab(&tab(), "nota");
        let g = col(&rendered, PARCEL_COL);
        assert_eq!(&g[..4], &["Parcelamentos Cartão", "Conta", "Loja", "Total"]);
        assert_eq!(
            rendered.cells[2][PARCEL_COL + 4],
            Cell::Number(Decimal::from(3)),
            "R = remaining"
        );
        assert_eq!(col(&rendered, PARCEL_COL + 5)[3], "=SUM(L3:L3)");
    }

    #[test]
    fn debit_block_sits_two_rows_under_the_installments() {
        let rendered = render_tab(&tab(), "nota");
        let g = col(&rendered, PARCEL_COL);
        assert_eq!(
            &g[3..8],
            &["Total", "", "Débito / Dinheiro", "Diarista", ""]
        );
        assert_eq!(rendered.cells[7][PARCEL_COL + 1], Cell::text("realizado:"));
        assert_eq!(
            rendered.cells[7][PARCEL_COL + 5],
            Cell::Number(Decimal::new(350, 0))
        );
    }

    #[test]
    fn category_budgets_are_blocks_with_planned_actual_and_what_is_left() {
        let rendered = render_tab(&tab(), "nota");
        let n = col(&rendered, ENVELOPE_COL);
        assert_eq!(&n[..2], &["Mercado", "Feira"]);
        assert_eq!(
            rendered.cells[2][ENVELOPE_COL],
            Cell::Number(Decimal::new(3000, 0))
        );
        assert_eq!(
            rendered.cells[3][ENVELOPE_COL],
            Cell::Number(Decimal::new(1200, 0))
        );
        assert_eq!(col(&rendered, ENVELOPE_COL + 1)[2], "=(N3-N4)*(N3>N4)");
        assert_eq!(col(&rendered, ENVELOPE_COL + 3)[5], "orçado:");
        assert_eq!(col(&rendered, ENVELOPE_COL + 4)[5], "=N3");
        assert!(rendered.merges.contains(&Range {
            rows: (0, 1),
            cols: (13, 15)
        }));
    }

    #[test]
    fn editable_blocks_get_dropdowns_and_cards_do_not() {
        let rendered = render_tab(&tab(), "nota");
        let status_rows: Vec<_> = rendered
            .validations
            .iter()
            .filter(|(v, _)| *v == Validation::Status)
            .map(|(_, r)| r.rows)
            .collect();
        assert_eq!(status_rows, vec![(1, 3), (13, 15)]);
    }
}
