//! Apply planning-sheet edits to the forecast table (ADR-0041).
//!
//! The forecasts of the sheet's window are loaded once into a [`WorkingSet`],
//! edited in memory (so a change can flow forward through a monthly series
//! before later tabs are read), then flushed in one upsert with one audit
//! event per touched forecast. Ids of rows created from the sheet are
//! derived from their content, so a sync that crashed between writing phai
//! and rewriting the sheet does not duplicate them on the next run.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use phai_core::idempotency::ensure_forecast_idempotency;
use phai_core::planning_sheet::{
    CreditCard, RowSnapshot, Section, SheetEdit, CARD_BUDGET_KIND, MONTHLY, SHEET_ORIGIN,
};
use phai_core::storage::FinanceStore;
use phai_core::{AuditEvent, ForecastRecord};
use rust_decimal::Decimal;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// `audit_log.actor_id` for writes that came from the planning sheet.
pub(crate) const SHEET_ACTOR_ID: &str = "planning-sheet";

pub(crate) struct WorkingSet {
    pub(crate) forecasts: BTreeMap<String, ForecastRecord>,
    dirty: BTreeMap<String, &'static str>,
    pub(crate) warnings: Vec<String>,
    actor_id: String,
    now: DateTime<Utc>,
    categories: BTreeSet<String>,
    accounts: BTreeSet<String>,
    cards: Vec<CreditCard>,
    /// First day of the last month the sheet carries.
    horizon_end: NaiveDate,
}

/// The editable fields, so a change can be checked and copied one by one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Item,
    Planned,
    Day,
    Category,
    Account,
    Notes,
}

const FIELDS: [Field; 6] = [
    Field::Item,
    Field::Planned,
    Field::Day,
    Field::Category,
    Field::Account,
    Field::Notes,
];

pub(crate) struct WorkingSetInit {
    pub(crate) forecasts: Vec<ForecastRecord>,
    pub(crate) actor_id: String,
    pub(crate) now: DateTime<Utc>,
    pub(crate) categories: BTreeSet<String>,
    pub(crate) accounts: BTreeSet<String>,
    pub(crate) cards: Vec<CreditCard>,
    pub(crate) horizon_end: NaiveDate,
}

impl WorkingSet {
    pub(crate) fn new(init: WorkingSetInit) -> Self {
        Self {
            forecasts: init
                .forecasts
                .into_iter()
                .map(|f| (f.forecast_id.clone(), f))
                .collect(),
            dirty: BTreeMap::new(),
            warnings: Vec::new(),
            actor_id: init.actor_id,
            now: init.now,
            categories: init.categories,
            accounts: init.accounts,
            cards: init.cards,
            horizon_end: init.horizon_end,
        }
    }

    /// Forecast ids changed so far, with the audit action of the last change.
    #[cfg(test)]
    pub(crate) fn changed(&self) -> &BTreeMap<String, &'static str> {
        &self.dirty
    }

    /// Apply one tab's edits. `ordinal` disambiguates identical typed rows.
    pub(crate) fn apply(&mut self, month: NaiveDate, edits: &[SheetEdit]) {
        for (ordinal, edit) in edits.iter().enumerate() {
            match edit {
                SheetEdit::Create { section, row } => self.create(month, *section, row, ordinal),
                SheetEdit::Update {
                    key,
                    section,
                    before,
                    after,
                } => self.update(month, key, *section, before, after),
                SheetEdit::Remove { key } => self.remove(key, "sheet_discard"),
                SheetEdit::CardPlan {
                    account_id,
                    planned,
                } => self.card_plan(month, account_id, *planned),
            }
        }
    }

    fn put(&mut self, mut record: ForecastRecord, action: &'static str) {
        record.updated_at = self.now;
        record.idempotency_key = String::new();
        if ensure_forecast_idempotency(&mut record).is_err() {
            record.idempotency_key = format!("planning-sheet:{}", record.forecast_id);
        }
        self.dirty.insert(record.forecast_id.clone(), action);
        self.forecasts.insert(record.forecast_id.clone(), record);
    }

    fn create(&mut self, month: NaiveDate, section: Section, row: &RowSnapshot, ordinal: usize) {
        let id = content_id(month, section, row, ordinal);
        if self
            .forecasts
            .get(&id)
            .is_some_and(|f| is_active(&f.status))
        {
            return;
        }
        let mut record = self.blank(id.clone(), month, row.day);
        record.amount = section.signed(row.planned.unwrap_or_default());
        for field in [Field::Item, Field::Category, Field::Account, Field::Notes] {
            if let Some(field) = self.validated(field, row) {
                set_field(&mut record, field, row, month, section);
            }
        }
        if row.repeats {
            start_series(&mut record, &id);
        }
        self.put(record.clone(), "sheet_create");
        if row.repeats {
            self.fill_series(&record, month);
        }
    }

    fn blank(&self, forecast_id: String, month: NaiveDate, day: Option<u32>) -> ForecastRecord {
        ForecastRecord {
            forecast_id,
            due_date: Some(due_in(month, day)),
            description: String::new(),
            amount: Decimal::ZERO,
            category_id: None,
            account_id: None,
            status: "ativo".to_string(),
            recurrence: None,
            actor_id: self.actor_id.clone(),
            idempotency_key: String::new(),
            metadata_json: json!({"origin": SHEET_ORIGIN}),
            created_at: self.now,
            updated_at: self.now,
            template_id: None,
            realized_transaction_id: None,
            realized_at: None,
        }
    }

    /// `field` if the typed value is acceptable; unknown categories and
    /// accounts are reported and left as they were.
    fn validated(&mut self, field: Field, row: &RowSnapshot) -> Option<Field> {
        let (value, known, what) = match field {
            Field::Category => (row.category_id.as_deref(), &self.categories, "categoria"),
            Field::Account => (row.account_id.as_deref(), &self.accounts, "conta"),
            _ => return Some(field),
        };
        match value {
            Some(v) if !known.contains(v) => {
                self.warnings.push(format!(
                    "\"{}\": {what} \"{v}\" não existe no phai e foi ignorada",
                    row.item
                ));
                None
            }
            _ => Some(field),
        }
    }

    fn update(
        &mut self,
        month: NaiveDate,
        key: &str,
        section: Section,
        before: &RowSnapshot,
        after: &RowSnapshot,
    ) {
        let Some(mut record) = self.live(key, &after.item) else {
            return;
        };
        let changed: Vec<Field> = FIELDS
            .into_iter()
            .filter(|f| field_differs(*f, before, after))
            .filter_map(|f| self.validated(f, after))
            .collect();
        for field in &changed {
            set_field(&mut record, *field, after, month, section);
        }
        if section.signed(record.amount) != record.amount {
            record.amount = section.signed(record.amount);
        }
        let repeats_changed = before.repeats != after.repeats;
        if repeats_changed && record.template_id.is_some() {
            self.warnings.push(format!(
                "\"{}\" repete por um modelo do phai; mude a recorrência pelo app",
                after.item
            ));
        } else if repeats_changed && after.repeats {
            let series = record.forecast_id.clone();
            start_series(&mut record, &series);
            self.put(record.clone(), "sheet_update");
            self.fill_series(&record, month);
            return;
        } else if repeats_changed {
            end_series(&mut record, month);
            self.end_later_members(&record, month);
        }
        self.put(record.clone(), "sheet_update");
        self.flow_forward(&record, month, section, &changed, before, after);
    }

    /// An active forecast by id, or `None` with a warning.
    fn live(&mut self, key: &str, item: &str) -> Option<ForecastRecord> {
        match self.forecasts.get(key) {
            Some(f) if is_active(&f.status) => Some(f.clone()),
            Some(_) => {
                self.warnings.push(format!(
                    "\"{item}\" já não está ativa no phai; edição ignorada"
                ));
                None
            }
            None => {
                self.warnings.push(format!(
                    "\"{item}\" não existe mais no phai; edição ignorada"
                ));
                None
            }
        }
    }

    /// Later months of the same series that still carried the old value of a
    /// field take the new one; months a person set differently keep theirs.
    fn flow_forward(
        &mut self,
        record: &ForecastRecord,
        month: NaiveDate,
        section: Section,
        changed: &[Field],
        before: &RowSnapshot,
        after: &RowSnapshot,
    ) {
        let Some(series) = series_of(record).map(str::to_string) else {
            return;
        };
        for mut member in self.later_members(&series, month) {
            let member_month = month_of(&member);
            let mut touched = false;
            for field in changed {
                if field_matches(&member, *field, before) {
                    set_field(&mut member, *field, after, member_month, section);
                    touched = true;
                }
            }
            if touched {
                self.put(member, "sheet_update");
            }
        }
    }

    fn later_members(&self, series: &str, month: NaiveDate) -> Vec<ForecastRecord> {
        self.forecasts
            .values()
            .filter(|f| series_of(f) == Some(series))
            .filter(|f| month_of(f) > month && is_active(&f.status))
            .cloned()
            .collect()
    }

    fn end_later_members(&mut self, record: &ForecastRecord, month: NaiveDate) {
        let Some(series) = series_of(record).map(str::to_string) else {
            return;
        };
        for member in self.later_members(&series, month) {
            self.remove(&member.forecast_id, "sheet_series_end");
        }
    }

    fn remove(&mut self, key: &str, action: &'static str) {
        let Some(mut record) = self.forecasts.get(key).cloned() else {
            return;
        };
        if !is_active(&record.status) {
            return;
        }
        record.status = "descartado".to_string();
        let stamp = self.now.to_rfc3339();
        metadata(&mut record).insert("discarded_at".to_string(), Value::String(stamp));
        self.put(record, action);
    }

    fn card_plan(&mut self, month: NaiveDate, account_id: &str, planned: Option<Decimal>) {
        let id = format!("sheet-card-{account_id}-{}", month.format("%Y%m"));
        let Some(amount) = planned else {
            self.remove(&id, "sheet_discard");
            return;
        };
        let card = self
            .cards
            .iter()
            .find(|c| c.account_id == account_id)
            .cloned();
        let label = card.as_ref().map_or(account_id, |c| c.label.as_str());
        let mut record = self
            .forecasts
            .get(&id)
            .cloned()
            .unwrap_or_else(|| self.blank(id.clone(), month, None));
        record.due_date = Some(due_in(month, card.as_ref().and_then(|c| c.due_day)));
        record.description = format!("Fatura {label} (planejada)");
        record.amount = -amount.abs();
        record.account_id = Some(account_id.to_string());
        record.status = "ativo".to_string();
        record.metadata_json = json!({"origin": SHEET_ORIGIN, "kind": CARD_BUDGET_KIND});
        self.put(record, "sheet_card_plan");
    }

    /// Members of `record`'s series for every month after `month` up to the
    /// horizon, copied from `record`.
    fn fill_series(&mut self, record: &ForecastRecord, month: NaiveDate) {
        let Some(series) = series_of(record).map(str::to_string) else {
            return;
        };
        let mut next = next_month(month);
        while next <= self.horizon_end {
            let id = format!("sheet-{series}-{}", next.format("%Y%m"));
            let mut member = record.clone();
            member.forecast_id = id;
            member.due_date = Some(due_in(next, record.due_date.map(|d| d.day())));
            member.status = "ativo".to_string();
            member.created_at = self.now;
            member.realized_transaction_id = None;
            member.realized_at = None;
            member.amount = predicted(record);
            let meta = metadata(&mut member);
            meta.remove("series_end");
            meta.remove("predicted_amount");
            meta.remove("discarded_at");
            self.put(member, "sheet_series_extend");
            next = next_month(next);
        }
    }

    /// Carry every open series through the horizon (new months appear as the
    /// calendar moves on).
    pub(crate) fn extend_series(&mut self) {
        let closed = self.closed_series();
        let mut latest: BTreeMap<String, ForecastRecord> = BTreeMap::new();
        for f in self.forecasts.values() {
            let Some(series) = series_of(f) else { continue };
            if closed.contains(series) {
                continue;
            }
            let newer = latest
                .get(series)
                .is_none_or(|current| f.due_date > current.due_date);
            if newer {
                latest.insert(series.to_string(), f.clone());
            }
        }
        for record in latest.into_values() {
            let month = month_of(&record);
            if month < self.horizon_end {
                self.fill_series(&record, month);
            }
        }
    }

    pub(crate) fn closed_series(&self) -> BTreeSet<String> {
        self.forecasts
            .values()
            .filter(|f| f.metadata_json.get("series_end").is_some())
            .filter_map(|f| series_of(f).map(str::to_string))
            .collect()
    }
}

/// Persist every changed forecast with one audit event each.
pub(crate) async fn flush(store: &dyn FinanceStore, set: &WorkingSet) -> Result<usize> {
    let records: Vec<ForecastRecord> = set
        .dirty
        .keys()
        .filter_map(|id| set.forecasts.get(id).cloned())
        .collect();
    if records.is_empty() {
        return Ok(0);
    }
    store.upsert_forecasts(&records).await?;
    let events: Vec<AuditEvent> = records
        .iter()
        .map(|r| {
            let action = set
                .dirty
                .get(&r.forecast_id)
                .copied()
                .unwrap_or("sheet_update");
            AuditEvent::from_entity(
                "forecast",
                &r.forecast_id,
                action,
                SHEET_ACTOR_ID,
                &format!(
                    "{SHEET_ACTOR_ID}:{}:{}",
                    r.forecast_id,
                    set.now.timestamp_millis()
                ),
                serde_json::to_value(r).unwrap_or(Value::Null),
            )
        })
        .collect();
    store.insert_audit_events(&events).await?;
    Ok(records.len())
}

fn is_active(status: &str) -> bool {
    status.eq_ignore_ascii_case("ativo") || status.eq_ignore_ascii_case("active")
}

fn metadata(record: &mut ForecastRecord) -> &mut Map<String, Value> {
    if !record.metadata_json.is_object() {
        record.metadata_json = json!({});
    }
    record
        .metadata_json
        .as_object_mut()
        .expect("forecast metadata_json must be an object")
}

fn series_of(record: &ForecastRecord) -> Option<&str> {
    record
        .metadata_json
        .get("series_id")
        .and_then(Value::as_str)
}

fn start_series(record: &mut ForecastRecord, series_id: &str) {
    record.recurrence = Some(MONTHLY.to_string());
    let meta = metadata(record);
    meta.insert(
        "series_id".to_string(),
        Value::String(series_id.to_string()),
    );
    meta.remove("series_end");
}

fn end_series(record: &mut ForecastRecord, month: NaiveDate) {
    record.recurrence = None;
    metadata(record).insert(
        "series_end".to_string(),
        Value::String(month.format("%Y-%m").to_string()),
    );
}

fn predicted(record: &ForecastRecord) -> Decimal {
    record
        .metadata_json
        .get("predicted_amount")
        .and_then(Value::as_str)
        .and_then(|v| v.parse().ok())
        .unwrap_or(record.amount)
}

fn month_of(record: &ForecastRecord) -> NaiveDate {
    record
        .due_date
        .and_then(|d| d.with_day(1))
        .unwrap_or(NaiveDate::MIN)
}

fn next_month(month: NaiveDate) -> NaiveDate {
    month
        .checked_add_months(chrono::Months::new(1))
        .unwrap_or(month)
}

/// `day` of `month`, clamped to its length; the last day when unset.
fn due_in(month: NaiveDate, day: Option<u32>) -> NaiveDate {
    let last = next_month(month).pred_opt().unwrap_or(month);
    let day = day.unwrap_or(last.day()).clamp(1, last.day());
    month.with_day(day).unwrap_or(last)
}

fn field_differs(field: Field, a: &RowSnapshot, b: &RowSnapshot) -> bool {
    match field {
        Field::Item => a.item != b.item && !b.item.is_empty(),
        Field::Planned => a.planned != b.planned && b.planned.is_some(),
        Field::Day => a.day != b.day,
        Field::Category => a.category_id != b.category_id,
        Field::Account => a.account_id != b.account_id,
        Field::Notes => a.notes != b.notes,
    }
}

/// Whether `record` still holds the value `snapshot` shows for `field`.
fn field_matches(record: &ForecastRecord, field: Field, snapshot: &RowSnapshot) -> bool {
    match field {
        Field::Item => record.description == snapshot.item,
        Field::Planned => Some(predicted(record).abs()) == snapshot.planned,
        Field::Day => record.due_date.map(|d| d.day()) == snapshot.day,
        Field::Category => record.category_id == snapshot.category_id,
        Field::Account => record.account_id == snapshot.account_id,
        Field::Notes => {
            record.metadata_json.get("notes").and_then(Value::as_str) == snapshot.notes.as_deref()
        }
    }
}

fn set_field(
    record: &mut ForecastRecord,
    field: Field,
    row: &RowSnapshot,
    month: NaiveDate,
    section: Section,
) {
    match field {
        Field::Item => record.description = row.item.clone(),
        Field::Planned => {
            if let Some(planned) = row.planned {
                record.amount = section.signed(planned);
            }
        }
        Field::Day => record.due_date = Some(due_in(month, row.day)),
        Field::Category => record.category_id = row.category_id.clone(),
        Field::Account => record.account_id = row.account_id.clone(),
        Field::Notes => {
            let meta = metadata(record);
            match &row.notes {
                Some(notes) => meta.insert("notes".to_string(), Value::String(notes.clone())),
                None => meta.remove("notes"),
            };
        }
    }
}

/// Stable id for a row typed into the sheet.
fn content_id(month: NaiveDate, section: Section, row: &RowSnapshot, ordinal: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "{}|{}|{}|{:?}|{:?}|{:?}|{:?}|{ordinal}",
        month.format("%Y-%m"),
        section.title(),
        row.item,
        row.planned,
        row.day,
        row.category_id,
        row.account_id,
    ));
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("sheet-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn row(item: &str, planned: i64, repeats: bool) -> RowSnapshot {
        RowSnapshot {
            item: item.to_string(),
            planned: Some(Decimal::new(planned, 0)),
            day: Some(5),
            repeats,
            category_id: Some("moradia:aluguel".to_string()),
            account_id: None,
            notes: None,
        }
    }

    fn set() -> WorkingSet {
        WorkingSet::new(WorkingSetInit {
            forecasts: Vec::new(),
            actor_id: "tester".to_string(),
            now: Utc::now(),
            categories: ["moradia:aluguel".to_string()].into(),
            accounts: ["conta_corrente".to_string(), "cartao_a".to_string()].into(),
            cards: vec![CreditCard {
                account_id: "cartao_a".to_string(),
                label: "A".to_string(),
                closing_day: Some(10),
                due_day: Some(17),
            }],
            horizon_end: d(2030, 8, 1),
        })
    }

    fn by_month(set: &WorkingSet) -> Vec<(NaiveDate, Decimal, String)> {
        let mut out: Vec<_> = set
            .forecasts
            .values()
            .filter(|f| is_active(&f.status))
            .map(|f| (f.due_date.unwrap(), f.amount, f.description.clone()))
            .collect();
        out.sort();
        out
    }

    fn create(set: &mut WorkingSet, month: NaiveDate, r: RowSnapshot) -> String {
        set.apply(
            month,
            &[SheetEdit::Create {
                section: Section::Expenses,
                row: r,
            }],
        );
        set.forecasts
            .values()
            .filter(|f| month_of(f) == month)
            .map(|f| f.forecast_id.clone())
            .next_back()
            .unwrap()
    }

    #[test]
    fn typed_expense_becomes_a_negative_forecast_on_its_day() {
        let mut ws = set();
        create(&mut ws, d(2030, 5, 1), row("Aluguel", 5000, false));
        assert_eq!(
            by_month(&ws),
            vec![(d(2030, 5, 5), Decimal::new(-5000, 0), "Aluguel".to_string())]
        );
        assert_eq!(
            ws.changed().values().copied().collect::<Vec<_>>(),
            vec!["sheet_create"]
        );
    }

    #[test]
    fn same_typed_row_twice_does_not_duplicate() {
        let mut ws = set();
        create(&mut ws, d(2030, 5, 1), row("Aluguel", 5000, false));
        create(&mut ws, d(2030, 5, 1), row("Aluguel", 5000, false));
        assert_eq!(by_month(&ws).len(), 1);
    }

    #[test]
    fn monthly_row_fills_every_month_to_the_horizon() {
        let mut ws = set();
        create(&mut ws, d(2030, 6, 1), row("Natação", 440, true));
        let months: Vec<_> = by_month(&ws).iter().map(|(d, ..)| d.month()).collect();
        assert_eq!(months, vec![6, 7, 8]);
    }

    #[test]
    fn value_change_flows_forward_only_to_months_still_on_the_old_value() {
        let mut ws = set();
        let june = create(&mut ws, d(2030, 6, 1), row("Natação", 440, true));
        // August was set by hand to 500 before June changes.
        let august = format!("sheet-{june}-203008");
        ws.forecasts.get_mut(&august).unwrap().amount = Decimal::new(-500, 0);
        ws.apply(
            d(2030, 6, 1),
            &[SheetEdit::Update {
                key: june,
                section: Section::Expenses,
                before: row("Natação", 440, true),
                after: row("Natação", 470, true),
            }],
        );
        let amounts: Vec<_> = by_month(&ws).iter().map(|(_, a, _)| *a).collect();
        assert_eq!(
            amounts,
            vec![
                Decimal::new(-470, 0),
                Decimal::new(-470, 0),
                Decimal::new(-500, 0)
            ]
        );
    }

    #[test]
    fn clearing_repete_ends_the_series_after_that_month() {
        let mut ws = set();
        let june = create(&mut ws, d(2030, 6, 1), row("Natação", 440, true));
        ws.apply(
            d(2030, 6, 1),
            &[SheetEdit::Update {
                key: june,
                section: Section::Expenses,
                before: row("Natação", 440, true),
                after: row("Natação", 440, false),
            }],
        );
        assert_eq!(by_month(&ws).len(), 1, "July and August discarded");
        ws.horizon_end = d(2030, 10, 1);
        ws.extend_series();
        assert_eq!(by_month(&ws).len(), 1, "a closed series does not grow back");
    }

    #[test]
    fn open_series_grows_when_the_horizon_moves() {
        let mut ws = set();
        create(&mut ws, d(2030, 7, 1), row("Natação", 440, true));
        ws.horizon_end = d(2030, 10, 1);
        ws.extend_series();
        let months: Vec<_> = by_month(&ws).iter().map(|(d, ..)| d.month()).collect();
        assert_eq!(months, vec![7, 8, 9, 10]);
    }

    #[test]
    fn deleted_row_is_discarded_not_dropped() {
        let mut ws = set();
        let id = create(&mut ws, d(2030, 5, 1), row("Aluguel", 5000, false));
        ws.apply(d(2030, 5, 1), &[SheetEdit::Remove { key: id.clone() }]);
        assert_eq!(ws.forecasts[&id].status, "descartado");
        assert!(ws.forecasts[&id]
            .metadata_json
            .get("discarded_at")
            .is_some());
    }

    #[test]
    fn unknown_category_is_reported_and_left_out() {
        let mut ws = set();
        let mut typed = row("Pilates", 350, false);
        typed.category_id = Some("inventada".to_string());
        let id = create(&mut ws, d(2030, 5, 1), typed);
        assert_eq!(ws.forecasts[&id].category_id, None);
        assert_eq!(ws.warnings.len(), 1);
    }

    #[test]
    fn card_plan_upserts_one_budget_forecast_per_card_and_month() {
        let mut ws = set();
        let plan = |planned| SheetEdit::CardPlan {
            account_id: "cartao_a".to_string(),
            planned,
        };
        ws.apply(d(2030, 6, 1), &[plan(Some(Decimal::new(4000, 0)))]);
        ws.apply(d(2030, 6, 1), &[plan(Some(Decimal::new(3500, 0)))]);
        let card = &ws.forecasts["sheet-card-cartao_a-203006"];
        assert_eq!(card.amount, Decimal::new(-3500, 0));
        assert_eq!(card.due_date, Some(d(2030, 6, 17)));
        assert_eq!(card.metadata_json["kind"], CARD_BUDGET_KIND);
        ws.apply(d(2030, 6, 1), &[plan(None)]);
        assert_eq!(
            ws.forecasts["sheet-card-cartao_a-203006"].status,
            "descartado"
        );
    }

    #[test]
    fn edits_to_inactive_forecasts_are_warnings() {
        let mut ws = set();
        let id = create(&mut ws, d(2030, 5, 1), row("Aluguel", 5000, false));
        ws.apply(d(2030, 5, 1), &[SheetEdit::Remove { key: id.clone() }]);
        ws.apply(
            d(2030, 5, 1),
            &[SheetEdit::Update {
                key: id,
                section: Section::Expenses,
                before: row("Aluguel", 5000, false),
                after: row("Aluguel", 1, false),
            }],
        );
        assert_eq!(ws.warnings.len(), 1);
    }

    #[test]
    fn missing_day_falls_on_the_last_day_of_the_month() {
        assert_eq!(due_in(d(2030, 2, 1), None), d(2030, 2, 28));
        assert_eq!(due_in(d(2030, 2, 1), Some(31)), d(2030, 2, 28));
        assert_eq!(due_in(d(2030, 2, 1), Some(3)), d(2030, 2, 3));
    }
}
