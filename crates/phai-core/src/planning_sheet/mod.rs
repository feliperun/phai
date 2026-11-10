//! Monthly planning tabs in a user-owned Google Sheet (ADR-0041).
//!
//! One tab per month mirrors the household's paper-style budget: planned
//! expenses, card bills and income, each with `Previsto` (planned, editable)
//! next to `Realizado` (actual, owned by phai). Edits typed into the sheet
//! come back as forecast changes; everything else is re-rendered on every
//! sync. This module is pure: it builds, renders, parses and diffs grids.
//! The network client lives in [`crate::google_sheets`].

mod build;
mod diff;
mod grid;

pub use build::{build_month_tab, CreditCard, MonthInputs};
pub use diff::{
    diff_tab, managed_tabs, manifest_entries, ManifestEntry, RowSnapshot, SheetEdit, TabDiff,
    TAB_MARKER,
};
pub use grid::{
    parse_manifest, parse_tab, render_lists, render_manifest, render_tab, Cell, ParsedRow,
    ParsedTab, RenderedTab, HEADERS,
};

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;

/// Hidden tab holding the snapshot of what the last sync rendered, so the
/// next sync can tell a person's edit apart from phai's own changes.
pub const MANIFEST_TAB: &str = "_phai";
/// Hidden tab with the category and account ids offered as dropdowns.
pub const LISTS_TAB: &str = "_phai_listas";
/// `forecast.metadata_json.origin` for rows created from the sheet.
pub const SHEET_ORIGIN: &str = "planning_sheet";
/// `forecast.metadata_json.kind` for a card's planned bill typed in the sheet.
pub const CARD_BUDGET_KIND: &str = "card_budget";
/// Prefix of the synthetic key that identifies a card row (one per card).
pub const CARD_KEY_PREFIX: &str = "card:";
/// Value of the `Repete` column for a monthly series.
pub const MONTHLY: &str = "mensal";

const MONTH_NAMES: [&str; 12] = [
    "JANEIRO",
    "FEVEREIRO",
    "MARÇO",
    "ABRIL",
    "MAIO",
    "JUNHO",
    "JULHO",
    "AGOSTO",
    "SETEMBRO",
    "OUTUBRO",
    "NOVEMBRO",
    "DEZEMBRO",
];

/// Tab title for a month, e.g. `NOVEMBRO 2026`.
pub fn tab_title(month: NaiveDate) -> String {
    format!("{} {}", MONTH_NAMES[month.month0() as usize], month.year())
}

/// Inverse of [`tab_title`]: the first day of the month a title names.
pub fn month_from_title(title: &str) -> Option<NaiveDate> {
    let (name, year) = title.trim().rsplit_once(' ')?;
    let month = MONTH_NAMES.iter().position(|m| *m == name)? as u32 + 1;
    NaiveDate::from_ymd_opt(year.parse().ok()?, month, 1)
}

/// The block a row belongs to. Totals and the month balance are formulas
/// over these blocks, so the order here is the order on the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Expenses,
    Cards,
    Income,
}

impl Section {
    pub const ALL: [Section; 3] = [Section::Expenses, Section::Cards, Section::Income];

    pub fn title(self) -> &'static str {
        match self {
            Section::Expenses => "DESPESAS",
            Section::Cards => "CARTÕES",
            Section::Income => "RECEITAS",
        }
    }

    pub fn total_label(self) -> &'static str {
        match self {
            Section::Expenses => "Total despesas",
            Section::Cards => "Total cartões",
            Section::Income => "Total receitas",
        }
    }

    fn from_title(text: &str) -> Option<Section> {
        Section::ALL.into_iter().find(|s| s.title() == text.trim())
    }

    /// Signed forecast amount for a planned magnitude typed in this block.
    pub fn signed(self, planned: Decimal) -> Decimal {
        match self {
            Section::Income => planned.abs(),
            Section::Expenses | Section::Cards => -planned.abs(),
        }
    }
}

/// One rendered line of a month tab.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetRow {
    pub section: Section,
    /// `forecast_id`, or `card:<account_id>` for a card row.
    pub key: String,
    pub item: String,
    /// Planned magnitude (always positive on the sheet).
    pub planned: Option<Decimal>,
    pub actual: Option<Decimal>,
    pub status: String,
    pub day: Option<u32>,
    pub repeats: bool,
    pub category_id: Option<String>,
    pub account_id: Option<String>,
    pub notes: Option<String>,
    /// False for rows phai owns entirely (paid forecasts): edits are ignored.
    pub editable: bool,
}

/// Everything rendered on one month tab.
#[derive(Debug, Clone, PartialEq)]
pub struct MonthTab {
    pub month: NaiveDate,
    pub rows: Vec<SheetRow>,
    /// Actual spend per category label for the month, largest first.
    pub category_actuals: Vec<(String, Decimal)>,
}

impl MonthTab {
    pub fn title(&self) -> String {
        tab_title(self.month)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_title_round_trips() {
        let month = NaiveDate::from_ymd_opt(2027, 3, 1).unwrap();
        assert_eq!(tab_title(month), "MARÇO 2027");
        assert_eq!(month_from_title("MARÇO 2027"), Some(month));
    }

    #[test]
    fn month_from_title_rejects_tabs_without_a_year() {
        assert_eq!(month_from_title("FEVEREIRO"), None);
        assert_eq!(month_from_title("Resumo 2026"), None);
    }

    #[test]
    fn income_is_positive_and_expenses_negative() {
        let v = Decimal::new(5000, 0);
        assert_eq!(Section::Income.signed(-v), v);
        assert_eq!(Section::Expenses.signed(v), -v);
        assert_eq!(Section::Cards.signed(v), -v);
    }
}
