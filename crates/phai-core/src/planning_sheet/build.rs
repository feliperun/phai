//! Turn a month's forecasts and actuals into the rows of its tab.

use std::collections::{BTreeSet, HashMap};

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use serde_json::Value;

use super::{MonthTab, Section, SheetRow, CARD_BUDGET_KIND, CARD_KEY_PREFIX, SHEET_ORIGIN};
use crate::models::{CardSummaryRow, ForecastRecord};

/// A credit card account and its billing days (from `accounts.metadata_json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditCard {
    pub account_id: String,
    pub label: String,
    pub closing_day: Option<u32>,
    pub due_day: Option<u32>,
}

/// What phai knows about one month.
pub struct MonthInputs<'a> {
    /// First day of the month.
    pub month: NaiveDate,
    pub today: NaiveDate,
    /// Forecasts due in the month, any status.
    pub forecasts: &'a [ForecastRecord],
    pub cards: &'a [CreditCard],
    /// `card_summary` rows of the billing cycle that closes in the month.
    pub card_totals: &'a [CardSummaryRow],
    /// Actual spend per category id in the month, as positive magnitudes.
    pub category_spend: &'a [(String, Decimal)],
    pub category_labels: &'a HashMap<String, String>,
    /// Series a person ended from the sheet (`Repete` cleared).
    pub closed_series: &'a BTreeSet<String>,
}

pub fn build_month_tab(input: &MonthInputs<'_>) -> MonthTab {
    let card_ids: BTreeSet<&str> = input.cards.iter().map(|c| c.account_id.as_str()).collect();
    let live: Vec<&ForecastRecord> = input
        .forecasts
        .iter()
        .filter(|f| is_live(&f.status))
        .collect();
    let (on_card, own): (Vec<&ForecastRecord>, Vec<&ForecastRecord>) =
        live.into_iter().partition(|f| {
            !is_sheet_row(f)
                && f.account_id
                    .as_deref()
                    .is_some_and(|account| card_ids.contains(account))
                || is_card_budget(f)
        });

    let mut rows: Vec<SheetRow> = own.iter().map(|f| forecast_row(f, input)).collect();
    rows.sort_by(|a, b| (a.section as u8, a.day, &a.item).cmp(&(b.section as u8, b.day, &b.item)));
    let mut cards: Vec<SheetRow> = input
        .cards
        .iter()
        .filter_map(|card| card_row(card, &on_card, input))
        .collect();
    let split = rows
        .iter()
        .position(|r| r.section == Section::Income)
        .unwrap_or(rows.len());
    let income = rows.split_off(split);
    rows.append(&mut cards);
    rows.extend(income);

    MonthTab {
        month: input.month,
        rows,
        category_actuals: category_actuals(input),
    }
}

fn is_live(status: &str) -> bool {
    ["ativo", "active", "realizado"]
        .iter()
        .any(|s| status.eq_ignore_ascii_case(s))
}

fn is_realized(f: &ForecastRecord) -> bool {
    f.status.eq_ignore_ascii_case("realizado") || f.realized_transaction_id.is_some()
}

fn meta_str<'a>(f: &'a ForecastRecord, key: &str) -> Option<&'a str> {
    f.metadata_json.get(key).and_then(Value::as_str)
}

fn is_sheet_row(f: &ForecastRecord) -> bool {
    meta_str(f, "origin") == Some(SHEET_ORIGIN) && !is_card_budget(f)
}

fn is_card_budget(f: &ForecastRecord) -> bool {
    meta_str(f, "kind") == Some(CARD_BUDGET_KIND)
}

fn is_open_bill(f: &ForecastRecord) -> bool {
    f.forecast_id.starts_with("cob-")
}

/// The amount a forecast planned before reconciliation overwrote it with
/// the matching transaction's amount.
fn predicted_amount(f: &ForecastRecord) -> Decimal {
    meta_str(f, "predicted_amount")
        .and_then(|v| v.parse::<Decimal>().ok())
        .unwrap_or(f.amount)
}

fn forecast_row(f: &ForecastRecord, input: &MonthInputs<'_>) -> SheetRow {
    let section = if f.amount > Decimal::ZERO {
        Section::Income
    } else {
        Section::Expenses
    };
    let realized = is_realized(f);
    SheetRow {
        section,
        key: f.forecast_id.clone(),
        item: f.description.clone(),
        planned: Some(predicted_amount(f).abs()),
        actual: row_actual(f, input),
        status: row_status(f, section, input.today).to_string(),
        day: f.due_date.map(|d| d.day()),
        repeats: row_repeats(f, input.closed_series),
        category_id: f.category_id.clone(),
        account_id: f.account_id.clone(),
        notes: meta_str(f, "notes").map(str::to_string),
        editable: !realized,
    }
}

/// A category-wide budget: no account to reconcile against, so its actual
/// is the month's spend in the category.
fn is_envelope(f: &ForecastRecord) -> bool {
    f.account_id.is_none() && f.category_id.is_some()
}

fn row_actual(f: &ForecastRecord, input: &MonthInputs<'_>) -> Option<Decimal> {
    if is_realized(f) {
        return Some(f.amount.abs());
    }
    if !is_envelope(f) {
        return None;
    }
    f.category_id
        .as_deref()
        .map(|c| category_total(input.category_spend, c))
}

fn row_status(f: &ForecastRecord, section: Section, today: NaiveDate) -> &'static str {
    match (is_realized(f), section) {
        (true, Section::Income) => "Recebido",
        (true, _) => "Pago",
        _ if is_envelope(f) => "Orçamento",
        _ if f.due_date.is_some_and(|d| d < today) => "Pendente",
        _ => "Previsto",
    }
}

fn row_repeats(f: &ForecastRecord, closed_series: &BTreeSet<String>) -> bool {
    let series_open = meta_str(f, "series_id").is_some_and(|s| !closed_series.contains(s));
    series_open || (f.template_id.is_some() && f.recurrence.as_deref() == Some("mensal"))
}

/// Spend in `category_id` and its subcategories (`moradia` covers `moradia:*`).
fn category_total(spend: &[(String, Decimal)], category_id: &str) -> Decimal {
    let prefix = format!("{category_id}:");
    spend
        .iter()
        .filter(|(c, _)| c == category_id || c.starts_with(&prefix))
        .map(|(_, v)| *v)
        .sum()
}

fn card_row(
    card: &CreditCard,
    on_card: &[&ForecastRecord],
    input: &MonthInputs<'_>,
) -> Option<SheetRow> {
    let mine: Vec<&ForecastRecord> = on_card
        .iter()
        .copied()
        .filter(|f| f.account_id.as_deref() == Some(card.account_id.as_str()))
        .collect();
    let budget = mine.iter().find(|f| is_card_budget(f));
    let open_bill = mine.iter().find(|f| is_open_bill(f));
    let committed: Decimal = mine
        .iter()
        .filter(|f| !is_card_budget(f) && !is_open_bill(f))
        .map(|f| f.amount.abs())
        .sum();
    let actual = input
        .card_totals
        .iter()
        .find(|t| t.account_id == card.account_id)
        .map(|t| t.total_charges)
        .filter(|v| !v.is_zero());
    let planned = budget
        .or(open_bill)
        .map(|f| f.amount.abs())
        .or((!committed.is_zero()).then_some(committed));
    if planned.is_none() && actual.is_none() && card.due_day.is_none() {
        return None;
    }
    let mut notes = Vec::new();
    if let (Some(_), Some(bill)) = (budget, open_bill) {
        notes.push(format!("fatura aberta {}", brl(bill.amount.abs())));
    }
    if !committed.is_zero() {
        notes.push(format!("parcelas e assinaturas {}", brl(committed)));
    }
    Some(SheetRow {
        section: Section::Cards,
        key: format!("{CARD_KEY_PREFIX}{}", card.account_id),
        item: format!("Cartão {}", card.label),
        planned,
        actual,
        status: card_status(card, input).to_string(),
        day: card.due_day,
        repeats: false,
        category_id: None,
        account_id: Some(card.account_id.clone()),
        notes: (!notes.is_empty()).then(|| notes.join(" · ")),
        editable: true,
    })
}

fn card_status(card: &CreditCard, input: &MonthInputs<'_>) -> &'static str {
    let current = NaiveDate::from_ymd_opt(input.today.year(), input.today.month(), 1);
    if current.is_some_and(|c| input.month > c) {
        return "Projeção";
    }
    let closing = card
        .closing_day
        .and_then(|day| input.month.with_day(day))
        .or_else(|| last_day(input.month));
    if closing.is_some_and(|c| input.today < c) {
        "Aberta"
    } else {
        "Fechada"
    }
}

fn last_day(month: NaiveDate) -> Option<NaiveDate> {
    let next = if month.month() == 12 {
        NaiveDate::from_ymd_opt(month.year() + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(month.year(), month.month() + 1, 1)
    };
    next.and_then(|d| d.pred_opt())
}

fn category_actuals(input: &MonthInputs<'_>) -> Vec<(String, Decimal)> {
    let mut totals: HashMap<String, Decimal> = HashMap::new();
    for (category_id, value) in input.category_spend {
        let label = input
            .category_labels
            .get(category_id)
            .cloned()
            .unwrap_or_else(|| category_id.clone());
        *totals.entry(label).or_default() += *value;
    }
    let mut out: Vec<(String, Decimal)> =
        totals.into_iter().filter(|(_, v)| !v.is_zero()).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// `R$ 1.234,56` — for notes only; money cells are written as numbers.
pub(crate) fn brl(value: Decimal) -> String {
    let fixed = format!("{:.2}", value.abs().round_dp(2));
    let (int, frac) = fixed.split_once('.').unwrap_or((fixed.as_str(), "00"));
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push('.');
        }
        grouped.push(ch);
    }
    let sign = if value.is_sign_negative() && !value.is_zero() {
        "-"
    } else {
        ""
    };
    format!("{sign}R$ {grouped},{frac}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn forecast(id: &str, due: NaiveDate, amount: i64, status: &str) -> ForecastRecord {
        ForecastRecord {
            forecast_id: id.to_string(),
            due_date: Some(due),
            description: format!("item {id}"),
            amount: Decimal::new(amount, 0),
            category_id: None,
            account_id: Some("conta_corrente".to_string()),
            status: status.to_string(),
            recurrence: None,
            actor_id: "tester".to_string(),
            idempotency_key: format!("k-{id}"),
            metadata_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            template_id: None,
            realized_transaction_id: None,
            realized_at: None,
        }
    }

    fn card() -> CreditCard {
        CreditCard {
            account_id: "cartao_a".to_string(),
            label: "A".to_string(),
            closing_day: Some(10),
            due_day: Some(17),
        }
    }

    struct Fixture {
        forecasts: Vec<ForecastRecord>,
        cards: Vec<CreditCard>,
        totals: Vec<CardSummaryRow>,
        spend: Vec<(String, Decimal)>,
        labels: HashMap<String, String>,
        closed: BTreeSet<String>,
    }

    impl Fixture {
        fn new(forecasts: Vec<ForecastRecord>) -> Self {
            Self {
                forecasts,
                cards: vec![card()],
                totals: Vec::new(),
                spend: Vec::new(),
                labels: HashMap::new(),
                closed: BTreeSet::new(),
            }
        }

        fn build(&self, today: NaiveDate) -> MonthTab {
            build_month_tab(&MonthInputs {
                month: d(2030, 5, 1),
                today,
                forecasts: &self.forecasts,
                cards: &self.cards,
                card_totals: &self.totals,
                category_spend: &self.spend,
                category_labels: &self.labels,
                closed_series: &self.closed,
            })
        }
    }

    fn sections(tab: &MonthTab) -> Vec<(Section, String)> {
        tab.rows
            .iter()
            .map(|r| (r.section, r.key.clone()))
            .collect()
    }

    #[test]
    fn rows_are_grouped_expenses_cards_income_and_skip_discarded() {
        let fx = Fixture::new(vec![
            forecast("salario", d(2030, 5, 5), 9000, "ativo"),
            forecast("aluguel", d(2030, 5, 3), -5000, "ativo"),
            forecast("velho", d(2030, 5, 3), -100, "descartado"),
        ]);
        let tab = fx.build(d(2030, 5, 1));
        assert_eq!(
            sections(&tab),
            vec![
                (Section::Expenses, "aluguel".to_string()),
                (Section::Cards, "card:cartao_a".to_string()),
                (Section::Income, "salario".to_string()),
            ]
        );
        assert_eq!(tab.rows[0].planned, Some(Decimal::new(5000, 0)));
        assert_eq!(tab.rows[0].status, "Previsto");
    }

    #[test]
    fn realized_forecast_shows_predicted_and_actual_and_is_read_only() {
        let mut paid = forecast("luz", d(2030, 5, 4), -431, "realizado");
        paid.metadata_json = json!({"predicted_amount": "-400"});
        let tab = Fixture::new(vec![paid]).build(d(2030, 5, 20));
        let row = &tab.rows[0];
        assert_eq!(row.planned, Some(Decimal::new(400, 0)));
        assert_eq!(row.actual, Some(Decimal::new(431, 0)));
        assert_eq!(row.status, "Pago");
        assert!(!row.editable);
    }

    #[test]
    fn unpaid_past_due_is_pending() {
        let tab = Fixture::new(vec![forecast("escola", d(2030, 5, 4), -2000, "ativo")])
            .build(d(2030, 5, 20));
        assert_eq!(tab.rows[0].status, "Pendente");
        assert_eq!(tab.rows[0].actual, None);
    }

    #[test]
    fn category_envelope_actual_sums_subcategories() {
        let mut envelope = forecast("mercado", d(2030, 5, 15), -3000, "ativo");
        envelope.account_id = None;
        envelope.category_id = Some("alimentacao".to_string());
        let mut fx = Fixture::new(vec![envelope]);
        fx.spend = vec![
            ("alimentacao:mercado".to_string(), Decimal::new(1200, 0)),
            ("alimentacao:padaria".to_string(), Decimal::new(300, 0)),
            ("alimentacaox".to_string(), Decimal::new(999, 0)),
        ];
        let tab = fx.build(d(2030, 5, 20));
        assert_eq!(tab.rows[0].actual, Some(Decimal::new(1500, 0)));
        assert_eq!(tab.rows[0].status, "Orçamento");
    }

    #[test]
    fn card_row_prefers_budget_then_open_bill_then_committed() {
        let mut parcela = forecast("installment-x", d(2030, 5, 2), -110, "ativo");
        parcela.account_id = Some("cartao_a".to_string());
        let committed_only = Fixture::new(vec![parcela.clone()]).build(d(2030, 4, 20));
        let row = &committed_only.rows[0];
        assert_eq!(row.planned, Some(Decimal::new(110, 0)));
        assert_eq!(row.status, "Projeção");

        let mut bill = forecast("cob-x", d(2030, 5, 17), -2500, "ativo");
        bill.account_id = Some("cartao_a".to_string());
        let with_bill = Fixture::new(vec![parcela.clone(), bill.clone()]).build(d(2030, 5, 5));
        assert_eq!(with_bill.rows[0].planned, Some(Decimal::new(2500, 0)));
        assert_eq!(with_bill.rows[0].status, "Aberta");

        let mut budget = forecast("sheet-card", d(2030, 5, 17), -4000, "ativo");
        budget.account_id = Some("cartao_a".to_string());
        budget.metadata_json = json!({"origin": SHEET_ORIGIN, "kind": CARD_BUDGET_KIND});
        let with_budget = Fixture::new(vec![parcela, bill, budget]).build(d(2030, 5, 12));
        let row = &with_budget.rows[0];
        assert_eq!(row.planned, Some(Decimal::new(4000, 0)));
        assert_eq!(row.status, "Fechada");
        assert_eq!(
            row.notes.as_deref(),
            Some("fatura aberta R$ 2.500,00 · parcelas e assinaturas R$ 110,00")
        );
    }

    #[test]
    fn card_actual_comes_from_the_cycle_total() {
        let mut fx = Fixture::new(Vec::new());
        fx.totals = vec![CardSummaryRow {
            month_ref: "2030-05".to_string(),
            account_id: "cartao_a".to_string(),
            total_charges: Decimal::new(321099, 2),
            open_amount: Decimal::ZERO,
            installments_future: Decimal::ZERO,
            transaction_count: 3,
        }];
        let tab = fx.build(d(2030, 5, 20));
        assert_eq!(tab.rows[0].actual, Some(Decimal::new(321099, 2)));
        assert_eq!(tab.rows[0].planned, None);
    }

    #[test]
    fn sheet_rows_on_a_card_account_stay_in_their_own_section() {
        let mut streaming = forecast("sheet-1", d(2030, 5, 8), -50, "ativo");
        streaming.account_id = Some("cartao_a".to_string());
        streaming.metadata_json = json!({"origin": SHEET_ORIGIN});
        let tab = Fixture::new(vec![streaming]).build(d(2030, 5, 1));
        assert_eq!(tab.rows[0].key, "sheet-1");
        assert_eq!(tab.rows[1].planned, None, "card row does not absorb it");
    }

    #[test]
    fn open_series_repeats_until_closed() {
        let mut rent = forecast("sheet-2", d(2030, 5, 3), -5000, "ativo");
        rent.metadata_json = json!({"origin": SHEET_ORIGIN, "series_id": "s1"});
        let mut fx = Fixture::new(vec![rent]);
        assert!(fx.build(d(2030, 5, 1)).rows[0].repeats);
        fx.closed.insert("s1".to_string());
        assert!(!fx.build(d(2030, 5, 1)).rows[0].repeats);
    }

    #[test]
    fn category_actuals_use_labels_and_sort_by_value() {
        let mut fx = Fixture::new(Vec::new());
        fx.spend = vec![
            ("moradia:aluguel".to_string(), Decimal::new(5000, 0)),
            ("lazer".to_string(), Decimal::new(300, 0)),
        ];
        fx.labels.insert(
            "moradia:aluguel".to_string(),
            "Moradia › Aluguel".to_string(),
        );
        let tab = fx.build(d(2030, 5, 20));
        assert_eq!(
            tab.category_actuals,
            vec![
                ("Moradia › Aluguel".to_string(), Decimal::new(5000, 0)),
                ("lazer".to_string(), Decimal::new(300, 0)),
            ]
        );
    }

    #[test]
    fn brl_groups_thousands() {
        assert_eq!(brl(Decimal::new(123456789, 2)), "R$ 1.234.567,89");
        assert_eq!(brl(Decimal::new(5, 1)), "R$ 0,50");
        assert_eq!(brl(Decimal::new(-1000, 0)), "-R$ 1.000,00");
    }
}
