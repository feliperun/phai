//! Turn a month's forecasts and actuals into what its tab shows.

use std::collections::BTreeSet;

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use serde_json::Value;

use super::{
    EnvelopeBlock, InstallmentLine, MonthTab, Section, SheetRow, CARD_BUDGET_KIND, CARD_KEY_PREFIX,
    SHEET_ORIGIN,
};
use crate::installments::{parse_installment_description, InstallmentMarker};
use crate::models::{CardSummaryRow, ForecastRecord, TransactionRecord};

/// Purchases listed under a category budget; the rest are summed in one line.
const ENVELOPE_LINES: usize = 10;
/// Width of a purchase's description inside a category block.
const LINE_WIDTH: usize = 22;

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
    /// The month's transactions by purchase date: each category budget lists
    /// and sums its purchases.
    pub transactions: &'a [TransactionRecord],
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
    let on_card = |f: &&ForecastRecord| {
        is_card_budget(f)
            || !is_sheet_row(f)
                && f.account_id
                    .as_deref()
                    .is_some_and(|account| card_ids.contains(account))
    };
    let card_forecasts: Vec<&ForecastRecord> = live.iter().copied().filter(on_card).collect();
    let (envelopes, own): (Vec<&ForecastRecord>, Vec<&ForecastRecord>) = live
        .iter()
        .copied()
        .filter(|f| !on_card(f))
        .partition(|f| is_envelope(f) && !is_realized(f));

    let mut rows: Vec<SheetRow> = own
        .iter()
        .map(|f| forecast_row(f, input.today, input.closed_series))
        .collect();
    rows.sort_by(|a, b| (a.section as u8, a.day, &a.item).cmp(&(b.section as u8, b.day, &b.item)));
    let split = rows
        .iter()
        .position(|r| r.section == Section::Income)
        .unwrap_or(rows.len());
    let income = rows.split_off(split);
    rows.extend(
        input
            .cards
            .iter()
            .filter_map(|card| card_row(card, &card_forecasts, input)),
    );
    rows.extend(income);

    MonthTab {
        month: input.month,
        rows,
        installments: installment_lines(&card_forecasts, input.cards),
        debit: debit_block(&live, input, &card_ids),
        envelopes: envelopes.iter().map(|f| envelope(f, input)).collect(),
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

/// A category-wide budget: no account to reconcile against, so its actual
/// is the month's spend in the category.
fn is_envelope(f: &ForecastRecord) -> bool {
    f.account_id.is_none() && f.category_id.is_some()
}

/// The amount a forecast planned before reconciliation overwrote it with
/// the matching transaction's amount.
fn predicted_amount(f: &ForecastRecord) -> Decimal {
    meta_str(f, "predicted_amount")
        .and_then(|v| v.parse::<Decimal>().ok())
        .unwrap_or(f.amount)
}

fn forecast_row(
    f: &ForecastRecord,
    today: NaiveDate,
    closed_series: &BTreeSet<String>,
) -> SheetRow {
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
        actual: realized.then(|| f.amount.abs()),
        status: row_status(f, today).to_string(),
        day: f.due_date.map(|d| d.day()),
        repeats: row_repeats(f, closed_series),
        category_id: f.category_id.clone(),
        account_id: f.account_id.clone(),
        notes: meta_str(f, "notes").map(str::to_string),
        editable: !realized,
    }
}

/// The household's own vocabulary: `Pago`, `Atrasado`, `Previsto`.
fn row_status(f: &ForecastRecord, today: NaiveDate) -> &'static str {
    if is_realized(f) {
        "Pago"
    } else if f.due_date.is_some_and(|d| d < today) {
        "Atrasado"
    } else {
        "Previsto"
    }
}

fn row_repeats(f: &ForecastRecord, closed_series: &BTreeSet<String>) -> bool {
    let series_open = meta_str(f, "series_id").is_some_and(|s| !closed_series.contains(s));
    series_open || (f.template_id.is_some() && f.recurrence.as_deref() == Some("mensal"))
}

/// Spend in `category_id` and its subcategories (`moradia` covers `moradia:*`).
fn in_category(candidate: Option<&str>, category_id: &str) -> bool {
    candidate.is_some_and(|c| {
        c == category_id
            || c.strip_prefix(category_id)
                .is_some_and(|rest| rest.starts_with(':'))
    })
}

fn envelope(f: &ForecastRecord, input: &MonthInputs<'_>) -> EnvelopeBlock {
    let category = f.category_id.as_deref().unwrap_or_default();
    let purchases = purchases(input.transactions, |t| {
        in_category(t.category_id.as_deref(), category)
    });
    summarize(f.description.clone(), predicted_amount(f).abs(), &purchases)
}

/// Spending from non-card accounts (debit, Pix, cash) not already shown as
/// a paid row of the table.
fn debit_block(
    live: &[&ForecastRecord],
    input: &MonthInputs<'_>,
    card_ids: &BTreeSet<&str>,
) -> EnvelopeBlock {
    let covered: BTreeSet<&str> = live
        .iter()
        .filter_map(|f| f.realized_transaction_id.as_deref())
        .collect();
    let spent = purchases(input.transactions, |t| {
        !t.account_id
            .as_deref()
            .is_some_and(|account| card_ids.contains(account))
            && !covered.contains(t.transaction_id.as_str())
    });
    summarize("Débito / Dinheiro".to_string(), Decimal::ZERO, &spent)
}

/// Outgoing transactions matching `keep`, largest first, each purchase once
/// even when it was imported twice (statement and aggregator).
fn purchases(
    transactions: &[TransactionRecord],
    keep: impl Fn(&TransactionRecord) -> bool,
) -> Vec<&TransactionRecord> {
    let mut seen = BTreeSet::new();
    let mut out: Vec<&TransactionRecord> = transactions
        .iter()
        .filter(|t| t.amount < Decimal::ZERO && t.payment_status != "installment")
        .filter(|t| keep(t))
        .filter(|t| {
            seen.insert((
                t.account_id.clone(),
                t.transaction_date,
                t.amount,
                t.raw_description.trim().to_lowercase(),
            ))
        })
        .collect();
    out.sort_by(|a, b| {
        a.amount
            .cmp(&b.amount)
            .then(a.transaction_date.cmp(&b.transaction_date))
    });
    out
}

/// The largest purchases one per line, the rest in `outras (n)`.
fn summarize(title: String, planned: Decimal, purchases: &[&TransactionRecord]) -> EnvelopeBlock {
    let actual: Decimal = purchases.iter().map(|t| t.amount.abs()).sum();
    let mut lines: Vec<(String, Decimal)> = purchases
        .iter()
        .take(ENVELOPE_LINES)
        .map(|t| (short(t.display_description()), t.amount.abs()))
        .collect();
    if let Some(rest) = purchases.get(ENVELOPE_LINES..).filter(|r| !r.is_empty()) {
        let total = rest.iter().map(|t| t.amount.abs()).sum();
        lines.push((format!("outras ({})", rest.len()), total));
    }
    EnvelopeBlock {
        title,
        planned,
        actual,
        lines,
    }
}

/// A purchase's name as it fits in a block. Raw bank lines read
/// `Transferência enviada|Fulano`: the part after the bar names who was paid.
fn short(text: &str) -> String {
    let main = text
        .split_once('|')
        .map(|(_, who)| who)
        .filter(|who| !who.trim().is_empty())
        .unwrap_or(text)
        .trim()
        .trim_end_matches('|');
    if main.chars().count() <= LINE_WIDTH {
        main.to_string()
    } else {
        let cut: String = main.chars().take(LINE_WIDTH - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

fn installment_lines(
    card_forecasts: &[&ForecastRecord],
    cards: &[CreditCard],
) -> Vec<InstallmentLine> {
    let mut lines: Vec<InstallmentLine> = card_forecasts
        .iter()
        .filter(|f| !is_card_budget(f) && !is_open_bill(f))
        .filter_map(|f| {
            let marker = installment_marker(&f.description)?;
            let card = cards
                .iter()
                .find(|c| Some(c.account_id.as_str()) == f.account_id.as_deref())
                .map_or_else(String::new, |c| card_short(&c.label));
            Some(InstallmentLine {
                item: marker.base_description,
                card,
                number: marker.current,
                total: marker.total,
                amount: f.amount.abs(),
            })
        })
        .collect();
    lines.sort_by(|a, b| (&a.card, &a.item).cmp(&(&b.card, &b.item)));
    lines
}

/// The `k/n` of an installment forecast, written `Loja (2/5)` or `Loja 2/5`.
fn installment_marker(description: &str) -> Option<InstallmentMarker> {
    let trimmed = description.trim();
    let unwrapped = trimmed
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once('('))
        .map(|(base, marker)| format!("{} {marker}", base.trim_end()));
    parse_installment_description(unwrapped.as_deref().unwrap_or(trimmed))
}

/// `Roxo` for `Cartão Roxo`: the column is narrow.
fn card_short(label: &str) -> String {
    label
        .split_whitespace()
        .filter(|w| !w.eq_ignore_ascii_case("cartão") && !w.eq_ignore_ascii_case("cartao"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn card_row(
    card: &CreditCard,
    card_forecasts: &[&ForecastRecord],
    input: &MonthInputs<'_>,
) -> Option<SheetRow> {
    let mine: Vec<&ForecastRecord> = card_forecasts
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
        item: card_item(&card.label),
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

/// `Cartão <label>`, unless the label already says it is a card.
fn card_item(label: &str) -> String {
    let lower = label.to_lowercase();
    if lower.contains("cartão") || lower.contains("cartao") {
        label.to_string()
    } else {
        format!("Cartão {label}")
    }
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
    month
        .checked_add_months(chrono::Months::new(1))
        .and_then(|d| d.pred_opt())
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

    fn purchase(id: &str, category: &str, cents: i64) -> TransactionRecord {
        TransactionRecord {
            transaction_id: id.to_string(),
            account_id: Some("cartao_a".to_string()),
            transaction_date: d(2030, 5, 3),
            raw_description: format!("compra {id}"),
            description: None,
            merchant_name: None,
            purpose: None,
            amount: Decimal::new(-cents, 2),
            amount_cents: Some(-cents),
            tx_type: "debit".to_string(),
            category_id: Some(category.to_string()),
            category_source: "rule".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: "pluggy".to_string(),
            actor_id: "tester".to_string(),
            idempotency_key: format!("tx-{id}"),
            metadata_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            enrichment_attempted_at: None,
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
        transactions: Vec<TransactionRecord>,
        closed: BTreeSet<String>,
    }

    impl Fixture {
        fn new(forecasts: Vec<ForecastRecord>) -> Self {
            Self {
                forecasts,
                cards: vec![card()],
                totals: Vec::new(),
                transactions: Vec::new(),
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
                transactions: &self.transactions,
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

    fn envelope_forecast(category: &str, amount: i64) -> ForecastRecord {
        let mut f = forecast("mercado", d(2030, 5, 15), amount, "ativo");
        f.account_id = None;
        f.category_id = Some(category.to_string());
        f.description = "Mercado".to_string();
        f
    }

    #[test]
    fn rows_are_fixed_bills_then_cards_then_income_and_skip_discarded() {
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
    fn realized_forecast_is_pago_with_predicted_and_actual_and_read_only() {
        let mut paid = forecast("luz", d(2030, 5, 4), -431, "realizado");
        paid.metadata_json = json!({"predicted_amount": "-400"});
        let mut salary = forecast("salario", d(2030, 5, 5), 9000, "realizado");
        salary.realized_transaction_id = Some("tx".to_string());
        let tab = Fixture::new(vec![paid, salary]).build(d(2030, 5, 20));
        let row = &tab.rows[0];
        assert_eq!(row.planned, Some(Decimal::new(400, 0)));
        assert_eq!(row.actual, Some(Decimal::new(431, 0)));
        assert_eq!(row.status, "Pago");
        assert!(!row.editable);
        assert_eq!(tab.rows[2].status, "Pago", "income received is Pago too");
    }

    #[test]
    fn unpaid_past_due_is_atrasado() {
        let tab = Fixture::new(vec![forecast("escola", d(2030, 5, 4), -2000, "ativo")])
            .build(d(2030, 5, 20));
        assert_eq!(tab.rows[0].status, "Atrasado");
        assert_eq!(tab.rows[0].actual, None);
    }

    #[test]
    fn category_budget_becomes_a_block_with_its_largest_purchases() {
        let mut fx = Fixture::new(vec![envelope_forecast("alimentacao", -3000)]);
        fx.transactions = (1..=12)
            .map(|i| purchase(&format!("p{i:02}"), "alimentacao:mercado", i * 1000))
            .chain([
                purchase("fora", "lazer", 99_999),
                purchase("prefixo", "alimentacaox", 99_999),
                purchase("p12", "alimentacao:mercado", 12_000),
            ])
            .collect();
        let tab = fx.build(d(2030, 5, 20));
        assert!(
            tab.rows.iter().all(|r| r.key != "mercado"),
            "not a table row"
        );
        let block = &tab.envelopes[0];
        assert_eq!(block.title, "Mercado");
        assert_eq!(block.planned, Decimal::new(3000, 0));
        assert_eq!(
            block.actual,
            Decimal::new(780, 0),
            "1+2+…+12 tens, twin counted once"
        );
        assert_eq!(block.lines.len(), 11, "ten purchases and the rest");
        assert_eq!(
            block.lines[0],
            ("compra p12".to_string(), Decimal::new(120, 0))
        );
        assert_eq!(
            block.lines[10],
            ("outras (2)".to_string(), Decimal::new(30, 0))
        );
    }

    #[test]
    fn debit_block_lists_account_spending_not_shown_as_a_paid_row() {
        let mut rent = forecast("aluguel", d(2030, 5, 3), -5000, "realizado");
        rent.realized_transaction_id = Some("t-aluguel".to_string());
        let mut fx = Fixture::new(vec![rent]);
        let on_account = |id: &str, cents| {
            let mut t = purchase(id, "casa", cents);
            t.account_id = Some("conta_corrente".to_string());
            t
        };
        fx.transactions = vec![
            on_account("t-aluguel", 500_000),
            on_account("t-diarista", 35_000),
            purchase("t-cartao", "casa", 9_000),
        ];
        let debit = fx.build(d(2030, 5, 20)).debit;
        assert_eq!(debit.title, "Débito / Dinheiro");
        assert_eq!(
            debit.lines,
            vec![("compra t-diarista".to_string(), Decimal::new(350, 0))]
        );
        assert_eq!(debit.actual, Decimal::new(350, 0));
    }

    #[test]
    fn card_installments_are_listed_with_card_number_and_total() {
        let mut parcela = forecast("installment-x", d(2030, 5, 2), -110, "ativo");
        parcela.account_id = Some("cartao_a".to_string());
        parcela.description = "Loja Tal (2/3)".to_string();
        let mut subscription = forecast("sub", d(2030, 5, 2), -40, "ativo");
        subscription.account_id = Some("cartao_a".to_string());
        let tab = Fixture::new(vec![parcela, subscription]).build(d(2030, 5, 1));
        assert_eq!(
            tab.installments,
            vec![InstallmentLine {
                item: "Loja Tal".to_string(),
                card: "A".to_string(),
                number: 2,
                total: 3,
                amount: Decimal::new(110, 0),
            }]
        );
    }

    #[test]
    fn installment_marker_reads_both_spellings() {
        let paren = installment_marker("Loja Tal (2/3)").unwrap();
        assert_eq!(
            (paren.base_description.as_str(), paren.current, paren.total),
            ("Loja Tal", 2, 3)
        );
        assert_eq!(installment_marker("Loja 4/10").map(|m| m.current), Some(4));
        assert!(installment_marker("Assinatura (mensal)").is_none());
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
    fn card_names_are_short_and_never_repeat_cartao() {
        assert_eq!(card_item("Cartão Roxo"), "Cartão Roxo");
        assert_eq!(card_item("Nubank"), "Cartão Nubank");
        assert_eq!(card_short("Cartão Roxo"), "Roxo");
    }

    #[test]
    fn long_descriptions_are_cut_to_fit_the_block() {
        assert_eq!(
            short("Pagamento efetuado|Companhia de Luz"),
            "Companhia de Luz"
        );
        assert_eq!(short("Padaria|"), "Padaria");
        assert_eq!(
            short("Supermercado Muito Grande Ltda"),
            "Supermercado Muito Gr…"
        );
    }

    #[test]
    fn brl_groups_thousands() {
        assert_eq!(brl(Decimal::new(123456789, 2)), "R$ 1.234.567,89");
        assert_eq!(brl(Decimal::new(5, 1)), "R$ 0,50");
        assert_eq!(brl(Decimal::new(-1000, 0)), "-R$ 1.000,00");
    }
}
