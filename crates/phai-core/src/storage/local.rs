use super::{FinanceStore, TransactionAnatomyPatch};
use crate::config::AppConfig;
use crate::models::{
    parse_datetime_or_now, AccountRecord, AccountSnapshotRecord, AuditEvent, BudgetStatusRow,
    CardClosedTransactionRow, CardSummaryRow, CashflowRow, CategoryBudgetRecord, CategoryRecord,
    CheckingBalance, DailyPulseItem, DuplicateTransactionGroup, ForecastRecord,
    ForecastTemplateRecord, ForecastVsActualRow, MonthlySpendRow, PlanChangeRecord,
    PlanScenarioRecord, RuleRecord, TransactionContextRow, TransactionRecord, UncategorizedRow,
};
use crate::splits::{
    ItemPriceRow, ReceiptItemRecord, SplitCandidateRow, TransactionSplitDetail,
    TransactionSplitLineRecord, TransactionSplitRecord,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, Days, NaiveDate, Utc};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;

pub struct LocalStore {
    db_path: PathBuf,
}

impl LocalStore {
    pub fn new(config: AppConfig) -> Result<Self> {
        let db_path = config
            .local_db_path
            .clone()
            .context("local_db_path não configurado")?;
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Falha ao criar {}", parent.display()))?;
        }
        Ok(Self { db_path })
    }

    fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("Falha ao abrir {}", self.db_path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .context("Falha ao configurar PRAGMAs do SQLite")?;
        Ok(conn)
    }

    fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
        let exists = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1 LIMIT 1",
                [table],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        Ok(exists)
    }
}

fn decimal_to_sql(value: &Decimal) -> String {
    value.round_dp(2).to_string()
}

fn parse_decimal(value: String) -> std::result::Result<Decimal, rust_decimal::Error> {
    Decimal::from_str(&value)
}

/// Convert a `Decimal` BRL amount to integer cents, mirroring how
/// `amount_cents` is stored in `v_transactions_reportable`.
fn decimal_to_cents(value: Decimal) -> Result<i64> {
    let scaled = (value * Decimal::from(100u32)).round();
    scaled
        .to_i64()
        .with_context(|| format!("Valor monetário fora do range i64: {value}"))
}

/// Returns the closing-anchor date for `month_start`: the last day of that
/// calendar month, capped at `today` so the current (in-progress) month
/// uses "now" as its closing edge rather than a future date.
fn last_day_of_target_month(month_start: NaiveDate, today: NaiveDate) -> Result<NaiveDate> {
    let (year, month) = (month_start.year(), month_start.month());
    let next_month_first = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    }
    .context("Falha ao calcular início do mês seguinte")?;
    let last_day = next_month_first
        .checked_sub_days(Days::new(1))
        .context("Falha ao calcular último dia do mês")?;
    Ok(if last_day > today { today } else { last_day })
}

fn parse_sql_date(
    value: String,
    column_index: usize,
) -> std::result::Result<NaiveDate, rusqlite::Error> {
    NaiveDate::parse_from_str(&value, "%Y-%m-%d").map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            column_index,
            rusqlite::types::Type::Text,
            Box::new(err),
        )
    })
}

fn parse_optional_sql_date(
    value: Option<String>,
    column_index: usize,
) -> std::result::Result<Option<NaiveDate>, rusqlite::Error> {
    value
        .map(|raw| parse_sql_date(raw, column_index))
        .transpose()
}

fn parse_sql_json(
    value: String,
    column_index: usize,
) -> std::result::Result<Value, rusqlite::Error> {
    serde_json::from_str(&value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            column_index,
            rusqlite::types::Type::Text,
            Box::new(err),
        )
    })
}

fn parse_sql_decimal(
    value: String,
    column_index: usize,
) -> std::result::Result<Decimal, rusqlite::Error> {
    parse_decimal(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            column_index,
            rusqlite::types::Type::Text,
            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
        )
    })
}

fn forecast_template_row_from_local(row: &Row<'_>) -> rusqlite::Result<ForecastTemplateRecord> {
    let amount_str: String = row.get(6)?;
    let amount = parse_decimal(amount_str).map_err(parse_decimal_err)?;
    let amount_lower = row
        .get::<_, Option<String>>(7)?
        .map(parse_decimal)
        .transpose()
        .map_err(parse_decimal_err)?;
    let amount_upper = row
        .get::<_, Option<String>>(8)?
        .map(parse_decimal)
        .transpose()
        .map_err(parse_decimal_err)?;
    let start_date: String = row.get(11)?;
    let start_date = NaiveDate::parse_from_str(&start_date, "%Y-%m-%d").map_err(parse_date_err)?;
    let end_date = row
        .get::<_, Option<String>>(12)?
        .map(|s| NaiveDate::parse_from_str(&s, "%Y-%m-%d"))
        .transpose()
        .map_err(parse_date_err)?;
    let metadata_str: String = row.get(17)?;
    let metadata_json: serde_json::Value = serde_json::from_str(&metadata_str)
        .unwrap_or_else(|_| serde_json::Value::Object(Default::default()));
    let created_str: String = row.get(20)?;
    let updated_str: String = row.get(21)?;
    Ok(ForecastTemplateRecord {
        template_id: row.get(0)?,
        kind: row.get(1)?,
        description: row.get(2)?,
        merchant_pattern: row.get(3)?,
        category_id: row.get(4)?,
        account_id: row.get(5)?,
        amount,
        amount_lower,
        amount_upper,
        cadence: row.get(9)?,
        next_due_day: row.get(10)?,
        start_date,
        end_date,
        remaining_count: row.get(13)?,
        source: row.get(14)?,
        confidence: row.get(15)?,
        status: row.get(16)?,
        metadata_json,
        actor_id: row.get(18)?,
        idempotency_key: row.get(19)?,
        created_at: parse_datetime_or_now(Some(&created_str)),
        updated_at: parse_datetime_or_now(Some(&updated_str)),
    })
}

fn plan_scenario_row_from_local(row: &Row<'_>) -> rusqlite::Result<PlanScenarioRecord> {
    let promoted_at = row
        .get::<_, Option<String>>(4)?
        .map(|raw| parse_datetime_or_now(Some(&raw)));
    let metadata_str: String = row.get(5)?;
    let metadata_json: Value =
        serde_json::from_str(&metadata_str).unwrap_or_else(|_| Value::Object(Default::default()));
    let created_str: String = row.get(8)?;
    let updated_str: String = row.get(9)?;
    Ok(PlanScenarioRecord {
        scenario_id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        status: row.get(3)?,
        promoted_at,
        metadata_json,
        actor_id: row.get(6)?,
        idempotency_key: row.get(7)?,
        created_at: parse_datetime_or_now(Some(&created_str)),
        updated_at: parse_datetime_or_now(Some(&updated_str)),
    })
}

fn plan_change_row_from_local(row: &Row<'_>) -> rusqlite::Result<PlanChangeRecord> {
    let amount = row
        .get::<_, Option<String>>(7)?
        .map(parse_decimal)
        .transpose()
        .map_err(parse_decimal_err)?;
    let payload_str: String = row.get(13)?;
    let payload_json: Value =
        serde_json::from_str(&payload_str).unwrap_or_else(|_| Value::Object(Default::default()));
    let created_str: String = row.get(16)?;
    let updated_str: String = row.get(17)?;
    Ok(PlanChangeRecord {
        change_id: row.get(0)?,
        scenario_id: row.get(1)?,
        kind: row.get(2)?,
        target_forecast_id: row.get(3)?,
        target_template_id: row.get(4)?,
        month: row.get(5)?,
        effective_from: row.get(6)?,
        amount,
        months_count: row.get(8)?,
        description: row.get(9)?,
        category_id: row.get(10)?,
        account_id: row.get(11)?,
        status: row.get(12)?,
        payload_json,
        actor_id: row.get(14)?,
        idempotency_key: row.get(15)?,
        created_at: parse_datetime_or_now(Some(&created_str)),
        updated_at: parse_datetime_or_now(Some(&updated_str)),
    })
}

fn parse_decimal_err(err: rust_decimal::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
    )
}

fn parse_date_err(err: chrono::ParseError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
    )
}

fn transaction_record_from_row(row: &Row<'_>) -> rusqlite::Result<TransactionRecord> {
    let transaction_date = row.get::<_, String>(2)?;
    let raw_description = row.get::<_, Option<String>>(3)?.unwrap_or_default();
    let description = row.get::<_, Option<String>>(4)?;
    let amount = row.get::<_, String>(7)?;
    let metadata_json = row.get::<_, String>(17)?;
    let created_at = row.get::<_, String>(18)?;
    let updated_at = row.get::<_, String>(19)?;
    let enrichment_attempted_at = row
        .get::<_, Option<String>>(20)?
        .map(|raw| parse_datetime_or_now(Some(&raw)));
    Ok(TransactionRecord {
        transaction_id: row.get(0)?,
        account_id: row.get(1)?,
        transaction_date: parse_sql_date(transaction_date, 2)?,
        raw_description: if raw_description.trim().is_empty() {
            description.clone().unwrap_or_default()
        } else {
            raw_description
        },
        description,
        merchant_name: row.get(5)?,
        purpose: row.get(6)?,
        amount: parse_sql_decimal(amount, 7)?,
        tx_type: row.get(8)?,
        category_id: row.get(9)?,
        category_source: row.get(10)?,
        context: row.get(11)?,
        classifier_trace: row.get(12)?,
        payment_status: row.get(13)?,
        source: row.get(14)?,
        actor_id: row.get(15)?,
        idempotency_key: row.get(16)?,
        metadata_json: parse_sql_json(metadata_json, 17)?,
        created_at: parse_datetime_or_now(Some(&created_at)),
        updated_at: parse_datetime_or_now(Some(&updated_at)),
        enrichment_attempted_at,
        amount_cents: None,
    })
}

fn split_bigquery_only_error() -> anyhow::Error {
    anyhow::anyhow!("transaction split/detailing is supported only on the BigQuery backend")
}

#[async_trait(?Send)]
impl FinanceStore for LocalStore {
    async fn applied_migrations(&self) -> Result<BTreeSet<String>> {
        let conn = self.connection()?;
        if !Self::table_exists(&conn, "schema_versions")? {
            return Ok(BTreeSet::new());
        }
        let mut stmt = conn.prepare("SELECT version FROM schema_versions ORDER BY version")?;
        let versions = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(versions.into_iter().collect())
    }

    async fn apply_sql(&self, sql: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute_batch(sql).context("Falha ao aplicar SQL")?;
        Ok(())
    }

    async fn record_migration(&self, version: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "INSERT OR REPLACE INTO schema_versions (version, applied_at) VALUES (?1, ?2)",
            params![version, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    async fn upsert_accounts(&self, rows: &[AccountRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO accounts (
              account_id, owner, account_type, bank, label, pluggy_account_id, pluggy_item_id,
              status, actor_id, idempotency_key, metadata_json, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(account_id) DO UPDATE SET
              owner = excluded.owner,
              account_type = excluded.account_type,
              bank = excluded.bank,
              label = excluded.label,
              pluggy_account_id = excluded.pluggy_account_id,
              pluggy_item_id = excluded.pluggy_item_id,
              status = excluded.status,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              metadata_json = excluded.metadata_json,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.account_id,
                row.owner,
                row.account_type,
                row.bank,
                row.label,
                row.pluggy_account_id,
                row.pluggy_item_id,
                row.status,
                row.actor_id,
                row.idempotency_key,
                row.metadata_json.to_string(),
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn get_accounts(&self) -> Result<Vec<AccountRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              account_id, owner, account_type, bank, label,
              pluggy_account_id, pluggy_item_id, status, actor_id, idempotency_key,
              metadata_json, created_at, updated_at
            FROM accounts
            ORDER BY account_id
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let metadata_str: String = row.get(10)?;
            let created_at_str: String = row.get(11)?;
            let updated_at_str: String = row.get(12)?;
            Ok(AccountRecord {
                account_id: row.get(0)?,
                owner: row.get(1)?,
                account_type: row.get(2)?,
                bank: row.get(3)?,
                label: row.get(4)?,
                pluggy_account_id: row.get(5)?,
                pluggy_item_id: row.get(6)?,
                status: row.get(7)?,
                actor_id: row.get(8)?,
                idempotency_key: row.get(9)?,
                metadata_json: serde_json::from_str(&metadata_str).unwrap_or_default(),
                created_at: parse_datetime_or_now(Some(&created_at_str)),
                updated_at: parse_datetime_or_now(Some(&updated_at_str)),
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    async fn insert_account_snapshots(&self, rows: &[AccountSnapshotRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT OR IGNORE INTO account_snapshots (
              snapshot_id, account_id, snapshot_date, balance, credit_limit, currency_code,
              source, actor_id, idempotency_key, metadata_json, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.snapshot_id,
                row.account_id,
                row.snapshot_date.format("%Y-%m-%d").to_string(),
                row.balance.as_ref().map(decimal_to_sql),
                row.credit_limit.as_ref().map(decimal_to_sql),
                row.currency_code,
                row.source,
                row.actor_id,
                row.idempotency_key,
                row.metadata_json.to_string(),
                row.created_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn latest_account_snapshots(&self) -> Result<Vec<AccountSnapshotRecord>> {
        let conn = self.connection()?;
        // For each account, pick the row with the greatest (snapshot_date,
        // created_at) — guarantees at most one row per account and ties go
        // to the most recently inserted snapshot.
        let mut stmt = conn.prepare(
            "
            SELECT
              snapshot_id, account_id, snapshot_date, balance, credit_limit,
              currency_code, source, actor_id, idempotency_key, metadata_json,
              created_at
            FROM account_snapshots AS s
            WHERE (s.snapshot_date, s.created_at) = (
              SELECT s2.snapshot_date, MAX(s2.created_at)
              FROM account_snapshots s2
              WHERE s2.account_id = s.account_id
              AND s2.snapshot_date = (
                SELECT MAX(s3.snapshot_date)
                FROM account_snapshots s3
                WHERE s3.account_id = s.account_id
              )
              GROUP BY s2.snapshot_date
            )
            ORDER BY account_id
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let balance: Option<String> = row.get(3)?;
                let credit_limit: Option<String> = row.get(4)?;
                let metadata_str: String = row.get(9)?;
                let created_str: String = row.get(10)?;
                Ok(AccountSnapshotRecord {
                    snapshot_id: row.get(0)?,
                    account_id: row.get(1)?,
                    snapshot_date: parse_sql_date(row.get(2)?, 2)?,
                    balance: balance.and_then(|s| parse_decimal(s).ok()),
                    credit_limit: credit_limit.and_then(|s| parse_decimal(s).ok()),
                    currency_code: row.get(5)?,
                    source: row.get(6)?,
                    actor_id: row.get(7)?,
                    idempotency_key: row.get(8)?,
                    metadata_json: parse_sql_json(metadata_str, 9)?,
                    created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                        .map(|d| d.with_timezone(&chrono::Utc))
                        .unwrap_or_else(|_| chrono::Utc::now()),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn upsert_transactions(&self, rows: &[TransactionRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO transactions (
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, amount, tx_type, category_id, category_source, context,
              classifier_trace, payment_status, source, actor_id, idempotency_key, metadata_json,
              created_at, updated_at, enrichment_attempted_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
            ON CONFLICT(transaction_id) DO UPDATE SET
              account_id = excluded.account_id,
              transaction_date = excluded.transaction_date,
              raw_description = COALESCE(NULLIF(transactions.raw_description, ''), excluded.raw_description),
              description = CASE
                WHEN excluded.source = 'pluggy' THEN COALESCE(transactions.description, excluded.description)
                ELSE excluded.description
              END,
              merchant_name = COALESCE(transactions.merchant_name, excluded.merchant_name),
              purpose = COALESCE(transactions.purpose, excluded.purpose),
              amount = excluded.amount,
              tx_type = excluded.tx_type,
              category_id = CASE WHEN transactions.category_source = 'manual' THEN transactions.category_id ELSE excluded.category_id END,
              category_source = CASE WHEN transactions.category_source = 'manual' THEN transactions.category_source ELSE excluded.category_source END,
              classifier_trace = CASE WHEN transactions.category_source = 'manual' THEN transactions.classifier_trace ELSE excluded.classifier_trace END,
              payment_status = excluded.payment_status,
              source = excluded.source,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              metadata_json = excluded.metadata_json,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.transaction_id,
                row.account_id,
                row.transaction_date.format("%Y-%m-%d").to_string(),
                row.raw_description,
                row.description,
                row.merchant_name,
                row.purpose,
                decimal_to_sql(&row.amount),
                row.tx_type,
                row.category_id,
                row.category_source,
                row.context,
                row.classifier_trace,
                row.payment_status,
                row.source,
                row.actor_id,
                row.idempotency_key,
                row.metadata_json.to_string(),
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
                row.enrichment_attempted_at.map(|value| value.to_rfc3339()),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn upsert_rules(&self, rows: &[RuleRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO rules (
              rule_id, body, status, actor_id, idempotency_key, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(rule_id) DO UPDATE SET
              body = excluded.body,
              status = excluded.status,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.rule_id,
                row.body,
                row.status,
                row.actor_id,
                row.idempotency_key,
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn upsert_categories(&self, rows: &[CategoryRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO categories (
              category_id, name, parent_category_id, metadata_json, actor_id, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(category_id) DO UPDATE SET
              name = excluded.name,
              parent_category_id = excluded.parent_category_id,
              metadata_json = excluded.metadata_json,
              actor_id = excluded.actor_id,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.category_id,
                row.name,
                row.parent_category_id,
                row.metadata_json.to_string(),
                row.actor_id,
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn upsert_forecasts(&self, rows: &[ForecastRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO forecast (
              forecast_id, due_date, description, amount, category_id, account_id,
              status, recurrence, actor_id, idempotency_key, metadata_json, created_at, updated_at,
              template_id, realized_transaction_id, realized_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
            ON CONFLICT(forecast_id) DO UPDATE SET
              due_date = excluded.due_date,
              description = excluded.description,
              amount = excluded.amount,
              category_id = excluded.category_id,
              account_id = excluded.account_id,
              status = excluded.status,
              recurrence = excluded.recurrence,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              metadata_json = excluded.metadata_json,
              updated_at = excluded.updated_at,
              template_id = excluded.template_id,
              realized_transaction_id = excluded.realized_transaction_id,
              realized_at = excluded.realized_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.forecast_id,
                row.due_date
                    .map(|value| value.format("%Y-%m-%d").to_string()),
                row.description,
                decimal_to_sql(&row.amount),
                row.category_id,
                row.account_id,
                row.status,
                row.recurrence,
                row.actor_id,
                row.idempotency_key,
                row.metadata_json.to_string(),
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
                row.template_id,
                row.realized_transaction_id,
                row.realized_at.map(|d| d.to_rfc3339()),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn upsert_forecast_templates(&self, rows: &[ForecastTemplateRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO forecast_template (
              template_id, kind, description, merchant_pattern, category_id, account_id,
              amount, amount_lower, amount_upper, cadence, next_due_day,
              start_date, end_date, remaining_count, source, confidence,
              status, metadata_json, actor_id, idempotency_key, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)
            ON CONFLICT(template_id) DO UPDATE SET
              kind = excluded.kind,
              description = excluded.description,
              merchant_pattern = excluded.merchant_pattern,
              category_id = excluded.category_id,
              account_id = excluded.account_id,
              amount = excluded.amount,
              amount_lower = excluded.amount_lower,
              amount_upper = excluded.amount_upper,
              cadence = excluded.cadence,
              next_due_day = excluded.next_due_day,
              start_date = excluded.start_date,
              end_date = excluded.end_date,
              remaining_count = excluded.remaining_count,
              source = excluded.source,
              confidence = excluded.confidence,
              status = excluded.status,
              metadata_json = excluded.metadata_json,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.template_id,
                row.kind,
                row.description,
                row.merchant_pattern,
                row.category_id,
                row.account_id,
                decimal_to_sql(&row.amount),
                row.amount_lower.as_ref().map(decimal_to_sql),
                row.amount_upper.as_ref().map(decimal_to_sql),
                row.cadence,
                row.next_due_day,
                row.start_date.format("%Y-%m-%d").to_string(),
                row.end_date.map(|d| d.format("%Y-%m-%d").to_string()),
                row.remaining_count,
                row.source,
                row.confidence,
                row.status,
                row.metadata_json.to_string(),
                row.actor_id,
                row.idempotency_key,
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn list_forecast_templates(
        &self,
        kind: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<ForecastTemplateRecord>> {
        let conn = self.connection()?;
        // Build the params as owned Strings so they live as long as the
        // query call. The filter SQL fragments are also built once here.
        let mut filters: Vec<&'static str> = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(k) = kind {
            filters.push("kind = ?");
            params.push(k.to_string());
        }
        if let Some(s) = status {
            filters.push("status = ?");
            params.push(s.to_string());
        }
        let where_sql = if filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filters.join(" AND "))
        };
        let sql = format!(
            "
            SELECT template_id, kind, description, merchant_pattern, category_id, account_id,
                   amount, amount_lower, amount_upper, cadence, next_due_day,
                   start_date, end_date, remaining_count, source, confidence,
                   status, metadata_json, actor_id, idempotency_key, created_at, updated_at
            FROM forecast_template
            {where_sql}
            ORDER BY datetime(created_at) DESC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), forecast_template_row_from_local)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn get_forecast_template(
        &self,
        template_id: &str,
    ) -> Result<Option<ForecastTemplateRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT template_id, kind, description, merchant_pattern, category_id, account_id,
                   amount, amount_lower, amount_upper, cadence, next_due_day,
                   start_date, end_date, remaining_count, source, confidence,
                   status, metadata_json, actor_id, idempotency_key, created_at, updated_at
            FROM forecast_template
            WHERE template_id = ?1
            ",
        )?;
        let mut rows = stmt.query_map([template_id], forecast_template_row_from_local)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    async fn upsert_plan_scenarios(&self, rows: &[PlanScenarioRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO plan_scenario (
              scenario_id, name, description, status, promoted_at,
              metadata_json, actor_id, idempotency_key, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(scenario_id) DO UPDATE SET
              name = excluded.name,
              description = excluded.description,
              status = excluded.status,
              promoted_at = excluded.promoted_at,
              metadata_json = excluded.metadata_json,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.scenario_id,
                row.name,
                row.description,
                row.status,
                row.promoted_at.map(|d| d.to_rfc3339()),
                row.metadata_json.to_string(),
                row.actor_id,
                row.idempotency_key,
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn list_plan_scenarios(&self, status: Option<&str>) -> Result<Vec<PlanScenarioRecord>> {
        let conn = self.connection()?;
        let mut filters: Vec<&'static str> = Vec::new();
        let mut params: Vec<String> = Vec::new();
        if let Some(s) = status {
            filters.push("status = ?");
            params.push(s.to_string());
        }
        let where_sql = if filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filters.join(" AND "))
        };
        let sql = format!(
            "
            SELECT scenario_id, name, description, status, promoted_at,
                   metadata_json, actor_id, idempotency_key, created_at, updated_at
            FROM plan_scenario
            {where_sql}
            ORDER BY datetime(created_at) DESC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), plan_scenario_row_from_local)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn get_plan_scenario(&self, scenario_id: &str) -> Result<Option<PlanScenarioRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT scenario_id, name, description, status, promoted_at,
                   metadata_json, actor_id, idempotency_key, created_at, updated_at
            FROM plan_scenario
            WHERE scenario_id = ?1
            ",
        )?;
        let mut rows = stmt.query_map([scenario_id], plan_scenario_row_from_local)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    async fn set_plan_scenario_status(
        &self,
        scenario_id: &str,
        status: &str,
        actor_id: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now().to_rfc3339();
        let promoted_at = (status == "promovido").then(|| now.clone());
        let updated = conn.execute(
            "
            UPDATE plan_scenario
            SET status = ?2,
                promoted_at = COALESCE(?3, promoted_at),
                actor_id = ?4,
                updated_at = ?5
            WHERE scenario_id = ?1
            ",
            params![scenario_id, status, promoted_at, actor_id, now],
        )?;
        if updated == 0 {
            bail!("Cenário não encontrado: {scenario_id}");
        }
        Ok(())
    }

    async fn upsert_plan_changes(&self, rows: &[PlanChangeRecord]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT INTO plan_change (
              change_id, scenario_id, kind, target_forecast_id, target_template_id,
              month, effective_from, amount, months_count, description,
              category_id, account_id, status, payload_json, actor_id,
              idempotency_key, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)
            ON CONFLICT(change_id) DO UPDATE SET
              scenario_id = excluded.scenario_id,
              kind = excluded.kind,
              target_forecast_id = excluded.target_forecast_id,
              target_template_id = excluded.target_template_id,
              month = excluded.month,
              effective_from = excluded.effective_from,
              amount = excluded.amount,
              months_count = excluded.months_count,
              description = excluded.description,
              category_id = excluded.category_id,
              account_id = excluded.account_id,
              status = excluded.status,
              payload_json = excluded.payload_json,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              updated_at = excluded.updated_at
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.change_id,
                row.scenario_id,
                row.kind,
                row.target_forecast_id,
                row.target_template_id,
                row.month,
                row.effective_from,
                row.amount.as_ref().map(decimal_to_sql),
                row.months_count,
                row.description,
                row.category_id,
                row.account_id,
                row.status,
                row.payload_json.to_string(),
                row.actor_id,
                row.idempotency_key,
                row.created_at.to_rfc3339(),
                row.updated_at.to_rfc3339(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn list_plan_changes(
        &self,
        scenario_id: &str,
        status: Option<&str>,
    ) -> Result<Vec<PlanChangeRecord>> {
        let conn = self.connection()?;
        let mut filters: Vec<&'static str> = vec!["scenario_id = ?"];
        let mut params: Vec<String> = vec![scenario_id.to_string()];
        if let Some(s) = status {
            filters.push("status = ?");
            params.push(s.to_string());
        }
        let where_sql = format!("WHERE {}", filters.join(" AND "));
        let sql = format!(
            "
            SELECT change_id, scenario_id, kind, target_forecast_id, target_template_id,
                   month, effective_from, amount, months_count, description,
                   category_id, account_id, status, payload_json, actor_id,
                   idempotency_key, created_at, updated_at
            FROM plan_change
            {where_sql}
            ORDER BY datetime(created_at) ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params_dyn.as_slice(), plan_change_row_from_local)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn delete_plan_change(&self, change_id: &str) -> Result<()> {
        let conn = self.connection()?;
        conn.execute("DELETE FROM plan_change WHERE change_id = ?1", [change_id])?;
        Ok(())
    }

    async fn delete_plan_scenario(&self, scenario_id: &str) -> Result<()> {
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM plan_change WHERE scenario_id = ?1",
            [scenario_id],
        )?;
        tx.execute(
            "DELETE FROM plan_scenario WHERE scenario_id = ?1",
            [scenario_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    async fn upcoming_forecasts(
        &self,
        from: NaiveDate,
        until: NaiveDate,
    ) -> Result<Vec<ForecastRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT forecast_id, due_date, description, amount, category_id, account_id,
                   status, recurrence, actor_id, idempotency_key, metadata_json,
                   created_at, updated_at,
                   template_id, realized_transaction_id, realized_at
            FROM forecast
            WHERE LOWER(status) IN ('ativo', 'active')
              AND due_date IS NOT NULL
              AND date(due_date) BETWEEN date(?1) AND date(?2)
            ORDER BY date(due_date) ASC, CAST(amount AS REAL) DESC
            ",
        )?;
        let rows =
            stmt.query_map(
                params![
                    from.format("%Y-%m-%d").to_string(),
                    until.format("%Y-%m-%d").to_string()
                ],
                |row| {
                    let due_str: Option<String> = row.get(1)?;
                    let due_date = due_str
                        .as_deref()
                        .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
                    let amount_str: String = row.get(3)?;
                    let amount = parse_decimal(amount_str).unwrap_or(Decimal::ZERO);
                    let metadata_str: String = row.get(10)?;
                    let metadata_json = parse_sql_json(metadata_str, 10)?;
                    let created_str: String = row.get(11)?;
                    let updated_str: String = row.get(12)?;
                    Ok(ForecastRecord {
                        forecast_id: row.get(0)?,
                        due_date,
                        description: row.get(2)?,
                        amount,
                        category_id: row.get(4)?,
                        account_id: row.get(5)?,
                        status: row.get(6)?,
                        recurrence: row.get(7)?,
                        actor_id: row.get(8)?,
                        idempotency_key: row.get(9)?,
                        metadata_json,
                        created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                            .map(|d| d.with_timezone(&chrono::Utc))
                            .unwrap_or_else(|_| chrono::Utc::now()),
                        updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                            .map(|d| d.with_timezone(&chrono::Utc))
                            .unwrap_or_else(|_| chrono::Utc::now()),
                        // Columns 13-15 are nullable optional FKs (added by
                        // migration 034 — ADR-0016). Older callers can ignore.
                        template_id: row.get::<_, Option<String>>(13).unwrap_or(None),
                        realized_transaction_id: row.get::<_, Option<String>>(14).unwrap_or(None),
                        realized_at: row.get::<_, Option<String>>(15).ok().flatten().and_then(
                            |s| {
                                chrono::DateTime::parse_from_rfc3339(&s)
                                    .map(|d| d.with_timezone(&chrono::Utc))
                                    .ok()
                            },
                        ),
                    })
                },
            )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn list_forecasts(
        &self,
        status: Option<&str>,
        from: Option<NaiveDate>,
        until: Option<NaiveDate>,
    ) -> Result<Vec<ForecastRecord>> {
        let conn = self.connection()?;
        let mut filters: Vec<&'static str> = Vec::new();
        let mut params_vec: Vec<String> = Vec::new();
        if let Some(s) = status {
            filters.push("status = ?");
            params_vec.push(s.to_string());
        }
        if let Some(d) = from {
            filters.push("date(due_date) >= date(?)");
            params_vec.push(d.format("%Y-%m-%d").to_string());
        }
        if let Some(d) = until {
            filters.push("date(due_date) <= date(?)");
            params_vec.push(d.format("%Y-%m-%d").to_string());
        }
        let where_sql = if filters.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", filters.join(" AND "))
        };
        let sql = format!(
            "
            SELECT forecast_id, due_date, description, amount, category_id, account_id,
                   status, recurrence, actor_id, idempotency_key, metadata_json,
                   created_at, updated_at,
                   template_id, realized_transaction_id, realized_at
            FROM forecast
            {where_sql}
            ORDER BY date(due_date) ASC, CAST(amount AS REAL) DESC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let params_dyn: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|p| p as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(params_dyn.as_slice(), |row| {
            let due_str: Option<String> = row.get(1)?;
            let due_date = due_str
                .as_deref()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
            let amount_str: String = row.get(3)?;
            let amount = parse_decimal(amount_str).unwrap_or(Decimal::ZERO);
            let metadata_str: String = row.get(10)?;
            let metadata_json = parse_sql_json(metadata_str, 10)?;
            let created_str: String = row.get(11)?;
            let updated_str: String = row.get(12)?;
            Ok(ForecastRecord {
                forecast_id: row.get(0)?,
                due_date,
                description: row.get(2)?,
                amount,
                category_id: row.get(4)?,
                account_id: row.get(5)?,
                status: row.get(6)?,
                recurrence: row.get(7)?,
                actor_id: row.get(8)?,
                idempotency_key: row.get(9)?,
                metadata_json,
                created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                template_id: row.get::<_, Option<String>>(13).unwrap_or(None),
                realized_transaction_id: row.get::<_, Option<String>>(14).unwrap_or(None),
                realized_at: row
                    .get::<_, Option<String>>(15)
                    .ok()
                    .flatten()
                    .and_then(|s| {
                        chrono::DateTime::parse_from_rfc3339(&s)
                            .map(|d| d.with_timezone(&chrono::Utc))
                            .ok()
                    }),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn get_forecast(&self, forecast_id: &str) -> Result<Option<ForecastRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT forecast_id, due_date, description, amount, category_id, account_id,
                   status, recurrence, actor_id, idempotency_key, metadata_json,
                   created_at, updated_at,
                   template_id, realized_transaction_id, realized_at
            FROM forecast
            WHERE forecast_id = ?1
            ",
        )?;
        let mut rows = stmt.query_map([forecast_id], |row| {
            let due_str: Option<String> = row.get(1)?;
            let due_date = due_str
                .as_deref()
                .and_then(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
            let amount_str: String = row.get(3)?;
            let amount = parse_decimal(amount_str).unwrap_or(Decimal::ZERO);
            let metadata_str: String = row.get(10)?;
            let metadata_json = parse_sql_json(metadata_str, 10)?;
            let created_str: String = row.get(11)?;
            let updated_str: String = row.get(12)?;
            Ok(ForecastRecord {
                forecast_id: row.get(0)?,
                due_date,
                description: row.get(2)?,
                amount,
                category_id: row.get(4)?,
                account_id: row.get(5)?,
                status: row.get(6)?,
                recurrence: row.get(7)?,
                actor_id: row.get(8)?,
                idempotency_key: row.get(9)?,
                metadata_json,
                created_at: chrono::DateTime::parse_from_rfc3339(&created_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                template_id: row.get::<_, Option<String>>(13).unwrap_or(None),
                realized_transaction_id: row.get::<_, Option<String>>(14).unwrap_or(None),
                realized_at: row
                    .get::<_, Option<String>>(15)
                    .ok()
                    .flatten()
                    .and_then(|s| {
                        chrono::DateTime::parse_from_rfc3339(&s)
                            .map(|d| d.with_timezone(&chrono::Utc))
                            .ok()
                    }),
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    async fn find_forecast_by_idempotency_key(
        &self,
        idempotency_key: &str,
    ) -> Result<Option<ForecastRecord>> {
        let id: Option<String> = {
            let conn = self.connection()?;
            conn.query_row(
                "SELECT forecast_id FROM forecast \
                 WHERE idempotency_key = ?1 AND status != 'descartado' \
                 ORDER BY created_at ASC LIMIT 1",
                [idempotency_key],
                |row| row.get(0),
            )
            .optional()?
        };
        match id {
            Some(id) => self.get_forecast(&id).await,
            None => Ok(None),
        }
    }

    async fn get_categories(&self) -> Result<Vec<CategoryRecord>> {
        let conn = self.connection()?;
        let exists = Self::table_exists(&conn, "categories")?;
        if !exists {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "
            SELECT category_id, name, parent_category_id, metadata_json, actor_id, updated_at
            FROM categories
            ORDER BY name ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            let metadata_str: String = row.get(3)?;
            let metadata_json =
                serde_json::from_str(&metadata_str).unwrap_or(Value::Object(Default::default()));
            let updated_str: String = row.get(5)?;
            Ok(CategoryRecord {
                category_id: row.get(0)?,
                name: row.get(1)?,
                parent_category_id: row.get(2)?,
                metadata_json,
                actor_id: row.get(4)?,
                updated_at: chrono::DateTime::parse_from_rfc3339(&updated_str)
                    .map(|d| d.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    async fn apply_transaction_split(
        &self,
        _split: &TransactionSplitRecord,
        _lines: &[TransactionSplitLineRecord],
        _items: &[ReceiptItemRecord],
    ) -> Result<()> {
        Err(split_bigquery_only_error())
    }

    async fn insert_audit_events(&self, rows: &[AuditEvent]) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let mut conn = self.connection()?;
        let tx = conn.transaction()?;
        let mut stmt = tx.prepare(
            "
            INSERT OR IGNORE INTO audit_log (
              event_id, entity_type, entity_id, action, actor_id, event_timestamp, idempotency_key, diff_json
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ",
        )?;
        for row in rows {
            stmt.execute(params![
                row.event_id,
                row.entity_type,
                row.entity_id,
                row.action,
                row.actor_id,
                row.event_timestamp.to_rfc3339(),
                row.idempotency_key,
                row.diff_json.to_string(),
            ])?;
        }
        drop(stmt);
        tx.commit()?;
        Ok(rows.len())
    }

    async fn delete_transaction(&self, transaction_id: &str) -> Result<()> {
        let conn = self.connection()?;
        let affected = conn.execute(
            "DELETE FROM transactions WHERE transaction_id = ?1",
            params![transaction_id],
        )?;
        if affected == 0 {
            bail!("Transação {transaction_id} não encontrada");
        }
        Ok(())
    }

    async fn annotate_transaction(
        &self,
        transaction_id: &str,
        category_id: Option<&str>,
        category_source: Option<&str>,
        classifier_trace: Option<&str>,
        actor_id: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let affected = conn.execute(
            "
            UPDATE transactions
            SET category_id = COALESCE(?1, category_id),
                category_source = COALESCE(?2, category_source),
                classifier_trace = COALESCE(?3, classifier_trace),
                actor_id = ?4,
                idempotency_key = ?5,
                updated_at = ?6
            WHERE transaction_id = ?7
            ",
            params![
                category_id,
                category_source,
                classifier_trace,
                actor_id,
                idempotency_key,
                Utc::now().to_rfc3339(),
                transaction_id,
            ],
        )?;
        if affected == 0 {
            bail!("Transação {transaction_id} não encontrada");
        }
        Ok(())
    }

    async fn update_transaction_anatomy(
        &self,
        transaction_id: &str,
        patch: TransactionAnatomyPatch<'_>,
        actor_id: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let affected = conn.execute(
            "
            UPDATE transactions
            SET description = COALESCE(?1, description),
                merchant_name = COALESCE(?2, merchant_name),
                purpose = COALESCE(?3, purpose),
                classifier_trace = COALESCE(?4, classifier_trace),
                context = COALESCE(?5, context),
                actor_id = ?6,
                idempotency_key = ?7,
                updated_at = ?8
            WHERE transaction_id = ?9
            ",
            params![
                patch.description,
                patch.merchant_name,
                patch.purpose,
                patch.classifier_trace,
                patch.context,
                actor_id,
                idempotency_key,
                Utc::now().to_rfc3339(),
                transaction_id,
            ],
        )?;
        if affected == 0 {
            bail!("Transação {transaction_id} não encontrada");
        }
        Ok(())
    }

    async fn set_commitment_tier(
        &self,
        transaction_id: &str,
        tier: Option<&str>,
        actor_id: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        match tier {
            Some(tier) => {
                conn.execute(
                    "
                    INSERT INTO transaction_tier
                        (transaction_id, tier, actor_id, idempotency_key, updated_at)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(transaction_id) DO UPDATE SET
                        tier = excluded.tier,
                        actor_id = excluded.actor_id,
                        idempotency_key = excluded.idempotency_key,
                        updated_at = excluded.updated_at
                    ",
                    params![
                        transaction_id,
                        tier,
                        actor_id,
                        idempotency_key,
                        Utc::now().to_rfc3339(),
                    ],
                )?;
            }
            None => {
                conn.execute(
                    "DELETE FROM transaction_tier WHERE transaction_id = ?1",
                    params![transaction_id],
                )?;
            }
        }
        Ok(())
    }

    async fn commitment_tier_overrides(&self) -> Result<Vec<(String, String)>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare("SELECT transaction_id, tier FROM transaction_tier")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn existing_transaction_ids(&self, ids: &[String]) -> Result<BTreeSet<String>> {
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }
        let conn = self.connection()?;
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT transaction_id FROM transactions WHERE transaction_id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(ids.iter()), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().collect())
    }

    async fn find_transactions_by_description(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let pattern = format!("%{}%", query.to_ascii_lowercase());
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose,
              printf('%.2f', COALESCE(amount_cents, 0) / 100.0),
              tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE LOWER(raw_description) LIKE ?1
               OR LOWER(COALESCE(description, '')) LIKE ?1
               OR LOWER(COALESCE(merchant_name, '')) LIKE ?1
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?2
            ",
        )?;
        let rows = stmt
            .query_map(params![pattern, limit as i64], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn latest_uncategorized_transactions(
        &self,
        limit: usize,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE context IS NULL
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn pending_human_descriptions(&self, limit: usize) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE (description IS NULL OR TRIM(description) = '')
              AND ABS(amount_cents) > 0
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn pending_merchants(&self, limit: usize) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE (merchant_name IS NULL OR TRIM(merchant_name) = '')
              AND category_source != 'unclassified'
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn pending_purposes(
        &self,
        min_abs_amount: Decimal,
        limit: usize,
    ) -> Result<Vec<TransactionRecord>> {
        let min_cents = (min_abs_amount.abs() * Decimal::from(100_i64))
            .round()
            .to_i64()
            .unwrap_or(0);
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE (purpose IS NULL OR TRIM(purpose) = '')
              AND ABS(amount_cents) >= ?1
              AND category_id IS NOT NULL
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?2
            ",
        )?;
        let rows = stmt
            .query_map(
                params![min_cents, limit as i64],
                transaction_record_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn count_pending_human_descriptions(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn.query_row(
            "
            SELECT COUNT(*)
            FROM v_transactions_reportable
            WHERE (description IS NULL OR TRIM(description) = '')
              AND ABS(amount_cents) > 0
            ",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    async fn count_pending_merchants(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn.query_row(
            "
            SELECT COUNT(*)
            FROM v_transactions_reportable
            WHERE (merchant_name IS NULL OR TRIM(merchant_name) = '')
              AND category_source != 'unclassified'
            ",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    async fn count_pending_purposes(&self, min_abs_amount: Decimal) -> Result<i64> {
        let min_cents = (min_abs_amount.abs() * Decimal::from(100_i64))
            .round()
            .to_i64()
            .unwrap_or(0);
        let conn = self.connection()?;
        let count = conn.query_row(
            "
            SELECT COUNT(*)
            FROM v_transactions_reportable
            WHERE (purpose IS NULL OR TRIM(purpose) = '')
              AND ABS(amount_cents) >= ?1
              AND category_id IS NOT NULL
            ",
            params![min_cents],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    async fn transaction_by_id(&self, transaction_id: &str) -> Result<Option<TransactionRecord>> {
        let conn = self.connection()?;
        let row = conn
            .query_row(
                "
                SELECT
                  transaction_id, account_id, transaction_date, raw_description, description,
                  merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
                  category_source, context, classifier_trace, payment_status, source,
                  actor_id, idempotency_key, metadata_json, created_at, updated_at,
                  enrichment_attempted_at
                FROM transactions
                WHERE transaction_id = ?1
                ",
                [transaction_id],
                transaction_record_from_row,
            )
            .optional()?;
        Ok(row)
    }

    async fn transaction_split_detail(
        &self,
        _transaction_id: &str,
    ) -> Result<Option<TransactionSplitDetail>> {
        Err(split_bigquery_only_error())
    }

    async fn clear_transaction_split(
        &self,
        _transaction_id: &str,
        _actor_id: &str,
        _idempotency_key: &str,
        _reason: Option<&str>,
    ) -> Result<()> {
        Err(split_bigquery_only_error())
    }

    async fn split_candidates(&self, _since: NaiveDate) -> Result<Vec<SplitCandidateRow>> {
        Err(split_bigquery_only_error())
    }

    async fn item_prices(
        &self,
        _query: &str,
        _since: Option<NaiveDate>,
    ) -> Result<Vec<ItemPriceRow>> {
        Err(split_bigquery_only_error())
    }

    async fn all_rules(&self) -> Result<Vec<RuleRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT rule_id, body, status, actor_id, idempotency_key, created_at, updated_at
            FROM rules
            ORDER BY rule_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let created_at: String = row.get(5)?;
                let updated_at: String = row.get(6)?;
                Ok(RuleRecord {
                    rule_id: row.get(0)?,
                    body: row.get(1)?,
                    status: row.get(2)?,
                    actor_id: row.get(3)?,
                    idempotency_key: row.get(4)?,
                    created_at: parse_datetime_or_now(Some(&created_at)),
                    updated_at: parse_datetime_or_now(Some(&updated_at)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn latest_pluggy_transaction_date(&self) -> Result<Option<NaiveDate>> {
        let conn = self.connection()?;
        let value = conn.query_row(
            "SELECT MAX(transaction_date) FROM transactions WHERE source = 'pluggy'",
            [],
            |row| row.get::<_, Option<String>>(0),
        )?;
        value
            .map(|raw| parse_sql_date(raw, 0))
            .transpose()
            .context("Falha ao ler última data Pluggy no SQLite")
    }

    async fn active_rules(&self) -> Result<Vec<RuleRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT rule_id, body, status, actor_id, idempotency_key, created_at, updated_at
            FROM rules
            WHERE LOWER(status) = 'active'
            ORDER BY rule_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let created_at: String = row.get(5)?;
                let updated_at: String = row.get(6)?;
                Ok(RuleRecord {
                    rule_id: row.get(0)?,
                    body: row.get(1)?,
                    status: row.get(2)?,
                    actor_id: row.get(3)?,
                    idempotency_key: row.get(4)?,
                    created_at: parse_datetime_or_now(Some(&created_at)),
                    updated_at: parse_datetime_or_now(Some(&updated_at)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn internal_categories(&self) -> Result<BTreeSet<String>> {
        let conn = self.connection()?;
        let exists = Self::table_exists(&conn, "internal_categories")?;
        if !exists {
            return Ok(BTreeSet::new());
        }
        let mut stmt = conn.prepare(
            "
            SELECT category_id
            FROM internal_categories
            ORDER BY category_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().collect())
    }

    async fn list_all_category_ids(&self) -> Result<BTreeSet<String>> {
        let conn = self.connection()?;
        let mut set = BTreeSet::new();
        if Self::table_exists(&conn, "categories")? {
            let mut stmt = conn.prepare("SELECT category_id FROM categories")?;
            for row in stmt.query_map([], |row| row.get::<_, String>(0))? {
                let id = row?;
                if !id.trim().is_empty() {
                    set.insert(id);
                }
            }
        }
        if Self::table_exists(&conn, "transactions")? {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT category_id FROM transactions WHERE category_id IS NOT NULL",
            )?;
            for row in stmt.query_map([], |row| row.get::<_, String>(0))? {
                let id = row?;
                if !id.trim().is_empty() {
                    set.insert(id);
                }
            }
        }
        Ok(set)
    }

    async fn transactions_with_context(&self, limit: usize) -> Result<Vec<TransactionContextRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              t.transaction_id,
              t.transaction_date,
              t.raw_description,
              CAST(t.amount AS TEXT),
              t.account_id,
              a.label,
              t.category_id,
              t.context,
              t.payment_status,
              t.source
            FROM v_transactions_reportable t
            LEFT JOIN accounts a ON a.account_id = t.account_id
            WHERE t.context IS NOT NULL
              AND TRIM(t.context) <> ''
            ORDER BY t.transaction_date DESC, ABS(t.amount) DESC, t.transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                let transaction_date = row.get::<_, String>(1)?;
                let amount = row.get::<_, String>(3)?;
                Ok(TransactionContextRow {
                    transaction_id: row.get(0)?,
                    transaction_date: parse_sql_date(transaction_date, 1)?,
                    description: row.get(2)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    account_id: row.get(4)?,
                    account_label: row.get(5)?,
                    category_id: row.get(6)?,
                    context: row.get(7)?,
                    payment_status: row.get(8)?,
                    source: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn count_transactions_with_context(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn.query_row(
            "SELECT COUNT(*) FROM v_transactions_reportable
             WHERE context IS NOT NULL
               AND TRIM(context) <> ''",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    async fn daily_pulse(&self, since: NaiveDate) -> Result<Vec<DailyPulseItem>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT transaction_id, transaction_date, description, CAST(amount AS TEXT), category_id, source, payment_status, account_id
            FROM v_daily_pulse
            WHERE transaction_date >= ?1
            ORDER BY transaction_date DESC, amount ASC, transaction_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([since.format("%Y-%m-%d").to_string()], |row| {
                let transaction_date = row.get::<_, String>(1)?;
                let amount = row.get::<_, String>(3)?;
                Ok(DailyPulseItem {
                    transaction_id: row.get(0)?,
                    transaction_date: NaiveDate::parse_from_str(&transaction_date, "%Y-%m-%d")
                        .map_err(|err| {
                            rusqlite::Error::FromSqlConversionFailure(
                                1,
                                rusqlite::types::Type::Text,
                                Box::new(err),
                            )
                        })?,
                    description: row.get(2)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    category_id: row.get(4)?,
                    source: row.get(5)?,
                    payment_status: row.get(6)?,
                    account_id: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn effective_transactions_window(
        &self,
        account_id: Option<&str>,
        since: NaiveDate,
        until: NaiveDate,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM v_transactions_reportable
            WHERE transaction_date >= ?1
              AND transaction_date <= ?2
              AND (?3 IS NULL OR account_id = ?3)
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            ",
        )?;
        let rows = stmt
            .query_map(
                params![
                    since.format("%Y-%m-%d").to_string(),
                    until.format("%Y-%m-%d").to_string(),
                    account_id,
                ],
                transaction_record_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn transactions_in_date_range(
        &self,
        account_id: Option<&str>,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM transactions
            WHERE transaction_date >= ?1
              AND transaction_date <= ?2
              AND (?3 IS NULL OR account_id = ?3)
            ORDER BY transaction_date ASC, transaction_id ASC
            ",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    from.format("%Y-%m-%d").to_string(),
                    to.format("%Y-%m-%d").to_string(),
                    account_id,
                ],
                transaction_record_from_row,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn monthly_spend(&self, month_ref: Option<&str>) -> Result<Vec<MonthlySpendRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT month_ref, category_id, account_id, CAST(expenses AS TEXT), expense_count
            FROM v_monthly_spend
            WHERE (?1 IS NULL OR month_ref = ?1)
            ORDER BY month_ref DESC, expenses DESC, category_id ASC, account_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([month_ref], |row| {
                let expenses = row.get::<_, String>(3)?;
                Ok(MonthlySpendRow {
                    month_ref: row.get(0)?,
                    category_id: row.get(1)?,
                    account_id: row.get(2)?,
                    expenses: parse_decimal(expenses).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    expense_count: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn cashflow(&self, months: usize) -> Result<Vec<CashflowRow>> {
        // ADR-0003: exact integer-cent SUM via amount_cents.
        //
        // Cash-basis semantics: only checking accounts contribute. Credit
        // card swipes live on credit accounts and are intentionally dropped
        // — the card bill payment (category_id='credit-card-payment') on
        // the checking account IS the cash event we want to count.
        // 'transfer-internal' is still excluded because it represents
        // movement between own accounts (no household net effect).
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT strftime('%Y-%m', t.transaction_date) AS month_ref,
                   t.amount_cents,
                   COALESCE(t.category_id, '') AS category_id
            FROM v_transactions_reportable t
            JOIN accounts a ON a.account_id = t.account_id
            WHERE a.account_type IN ('checking', 'bank')
              AND COALESCE(t.category_id, '') != 'transfer-internal'
            ",
        )?;
        let mut by_month: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
        let rows = stmt.query_map([], |row| {
            let month_ref: String = row.get(0)?;
            let cents: i64 = row.get(1)?;
            let category_id: String = row.get(2)?;
            Ok((month_ref, cents, category_id))
        })?;
        for row in rows {
            let (month_ref, cents, category_id) = row?;
            let bucket = by_month.entry(month_ref).or_insert((0, 0, 0));
            if cents < 0 {
                bucket.1 += cents.abs();
            } else if category_id == "cashback" {
                bucket.2 += cents;
            } else {
                bucket.0 += cents;
            }
        }
        let mut out: Vec<CashflowRow> = by_month
            .into_iter()
            .map(|(month_ref, (inc, exp, er))| CashflowRow {
                month_ref,
                income: Decimal::new(inc, 2),
                expenses: Decimal::new(exp, 2),
                expense_reduction: Decimal::new(er, 2),
                net: Decimal::new(inc + er - exp, 2),
                opening_balance: None,
                closing_balance: None,
            })
            .collect();
        out.sort_by(|a, b| b.month_ref.cmp(&a.month_ref));
        out.truncate(months);
        Ok(out)
    }

    async fn cashflow_month(&self, month_ref: &str) -> Result<CashflowRow> {
        // Cash-basis cashflow restricted to a single month + checking accounts.
        // Shares the predicate/sign-bucket logic with `cashflow()` but bound
        // to one `YYYY-MM` to avoid scanning all history.
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT t.amount_cents,
                   COALESCE(t.category_id, '') AS category_id
            FROM v_transactions_reportable t
            JOIN accounts a ON a.account_id = t.account_id
            WHERE a.account_type IN ('checking', 'bank')
              AND COALESCE(t.category_id, '') != 'transfer-internal'
              AND strftime('%Y-%m', t.transaction_date) = ?1
            ",
        )?;
        let mut income_cents: i64 = 0;
        let mut expense_cents: i64 = 0;
        let mut reduction_cents: i64 = 0;
        let rows = stmt.query_map([month_ref], |row| {
            let cents: i64 = row.get(0)?;
            let category_id: String = row.get(1)?;
            Ok((cents, category_id))
        })?;
        for row in rows {
            let (cents, category_id) = row?;
            if cents < 0 {
                expense_cents += cents.abs();
            } else if category_id == "cashback" {
                reduction_cents += cents;
            } else {
                income_cents += cents;
            }
        }

        // Anchor opening at the last day of the previous month and closing
        // at the last day of `month_ref` (or `today` for the current month).
        let target_month_start = NaiveDate::parse_from_str(&format!("{month_ref}-01"), "%Y-%m-%d")
            .with_context(|| format!("month_ref inválido: {month_ref} (esperado YYYY-MM)"))?;
        let opening_anchor = target_month_start
            .checked_sub_days(Days::new(1))
            .context("Falha ao calcular dia anterior ao início do mês")?;
        let closing_anchor = last_day_of_target_month(target_month_start, Utc::now().date_naive())?;

        let opening = self.checking_balance_at(opening_anchor).await?;
        let closing = self.checking_balance_at(closing_anchor).await?;

        Ok(CashflowRow {
            month_ref: month_ref.to_string(),
            income: Decimal::new(income_cents, 2),
            expenses: Decimal::new(expense_cents, 2),
            expense_reduction: Decimal::new(reduction_cents, 2),
            net: Decimal::new(income_cents + reduction_cents - expense_cents, 2),
            opening_balance: opening.map(|b| b.balance),
            closing_balance: closing.map(|b| b.balance),
        })
    }

    async fn cashflow_reportable(&self) -> Result<Vec<CashflowRow>> {
        // Cash-flow basis over all reportable accounts — reads the canonical
        // `v_cashflow` view directly (single source of truth). The view buckets
        // by `cash_month` (a card purchase lands in the month its bill is paid,
        // ADR-0025) over `v_transactions_reportable`, which already drops
        // ofx-shadowed-by-pluggy and legacy-manual duplicates (ADR-0026), so no
        // dedup is re-implemented here.
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT month_ref, income, expenses, expense_reduction, net
            FROM v_cashflow
            ORDER BY month_ref ASC
            ",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (month_ref, income, expenses, expense_reduction, net) = row?;
            out.push(CashflowRow {
                month_ref,
                income: Decimal::from_str(&income).unwrap_or_default(),
                expenses: Decimal::from_str(&expenses).unwrap_or_default(),
                expense_reduction: Decimal::from_str(&expense_reduction).unwrap_or_default(),
                net: Decimal::from_str(&net).unwrap_or_default(),
                opening_balance: None,
                closing_balance: None,
            });
        }
        Ok(out)
    }

    async fn checking_balance_at(&self, target: NaiveDate) -> Result<Option<CheckingBalance>> {
        // Anchor strategy: for each checking account, pick the latest
        // snapshot whose `snapshot_date <= target`, then add the sum of
        // `amount_cents` for transactions with `snapshot_date < tx_date
        // <= target`. Sum across accounts. If any checking account lacks
        // a snapshot ≤ target, return None — guessing the missing anchor
        // would silently report a wrong saldo, so we prefer "unknown".
        let conn = self.connection()?;

        let mut accounts_stmt = conn.prepare(
            "SELECT account_id FROM accounts WHERE account_type IN ('checking', 'bank')",
        )?;
        let account_ids: Vec<String> = accounts_stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        if account_ids.is_empty() {
            return Ok(Some(CheckingBalance {
                balance: Decimal::ZERO,
                accounts_considered: 0,
                snapshot_anchor_date: None,
            }));
        }

        let target_str = target.format("%Y-%m-%d").to_string();
        let mut anchor_stmt = conn.prepare(
            "
            SELECT snapshot_date, balance
            FROM account_snapshots
            WHERE account_id = ?1
              AND snapshot_date <= ?2
            ORDER BY snapshot_date DESC, created_at DESC
            LIMIT 1
            ",
        )?;
        let mut delta_stmt = conn.prepare(
            "
            SELECT COALESCE(SUM(amount_cents), 0)
            FROM v_transactions_reportable
            WHERE account_id = ?1
              AND transaction_date > ?2
              AND transaction_date <= ?3
            ",
        )?;

        let mut total_cents: i64 = 0;
        let mut latest_anchor: Option<NaiveDate> = None;

        for account_id in &account_ids {
            let anchor: Option<(String, Option<String>)> = anchor_stmt
                .query_row(params![account_id, &target_str], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .optional()?;
            let Some((anchor_date_str, balance_str)) = anchor else {
                return Ok(None);
            };
            let anchor_date = parse_sql_date(anchor_date_str.clone(), 0)
                .map_err(|e| anyhow::anyhow!("snapshot_date inválido para {account_id}: {e}"))?;
            let snapshot_balance = balance_str
                .as_deref()
                .map(|s| parse_decimal(s.to_string()))
                .transpose()
                .with_context(|| format!("balance inválido no snapshot de {account_id}"))?
                .unwrap_or(Decimal::ZERO);
            let snapshot_cents = decimal_to_cents(snapshot_balance)?;

            let delta_cents: i64 = delta_stmt
                .query_row(params![account_id, &anchor_date_str, &target_str], |row| {
                    row.get::<_, i64>(0)
                })?;

            total_cents = total_cents
                .checked_add(snapshot_cents)
                .and_then(|v| v.checked_add(delta_cents))
                .context("Overflow ao agregar saldo de contas correntes")?;
            latest_anchor = Some(match latest_anchor {
                Some(prev) if prev > anchor_date => prev,
                _ => anchor_date,
            });
        }

        Ok(Some(CheckingBalance {
            balance: Decimal::new(total_cents, 2),
            accounts_considered: account_ids.len(),
            snapshot_anchor_date: latest_anchor,
        }))
    }

    async fn forecast_vs_actual(
        &self,
        month_ref: Option<&str>,
    ) -> Result<Vec<ForecastVsActualRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              forecast_id,
              month_ref,
              due_date,
              description,
              account_id,
              category_id,
              CAST(forecast_amount AS TEXT),
              CAST(actual_amount AS TEXT),
              CAST(variance AS TEXT),
              status
            FROM v_forecast_vs_actual
            WHERE (?1 IS NULL OR month_ref = ?1)
            ORDER BY month_ref DESC, due_date ASC, description ASC
            ",
        )?;
        let rows = stmt
            .query_map([month_ref], |row| {
                let forecast_amount = row.get::<_, String>(6)?;
                let actual_amount = row.get::<_, String>(7)?;
                let variance = row.get::<_, String>(8)?;
                Ok(ForecastVsActualRow {
                    forecast_id: row.get(0)?,
                    month_ref: row.get(1)?,
                    due_date: parse_optional_sql_date(row.get(2)?, 2)?,
                    description: row.get(3)?,
                    account_id: row.get(4)?,
                    category_id: row.get(5)?,
                    forecast_amount: parse_decimal(forecast_amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    actual_amount: parse_decimal(actual_amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    variance: parse_decimal(variance).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            8,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    status: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn card_summary(&self, month_ref: Option<&str>) -> Result<Vec<CardSummaryRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              month_ref,
              account_id,
              CAST(total_charges AS TEXT),
              CAST(open_amount AS TEXT),
              CAST(installments_future AS TEXT),
              transaction_count
            FROM v_card_summary
            WHERE (?1 IS NULL OR month_ref = ?1)
            ORDER BY month_ref DESC, total_charges DESC, account_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([month_ref], |row| {
                let total_charges = row.get::<_, String>(2)?;
                let open_amount = row.get::<_, String>(3)?;
                let installments_future = row.get::<_, String>(4)?;
                Ok(CardSummaryRow {
                    month_ref: row.get(0)?,
                    account_id: row.get(1)?,
                    total_charges: parse_decimal(total_charges).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    open_amount: parse_decimal(open_amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    installments_future: parse_decimal(installments_future).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    transaction_count: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn cards_open_now(&self) -> Result<Vec<CardSummaryRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              month_ref,
              account_id,
              CAST(total_charges AS TEXT),
              CAST(open_amount AS TEXT),
              CAST(installments_future AS TEXT),
              transaction_count
            FROM v_card_open_now
            ORDER BY account_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let total_charges = row.get::<_, String>(2)?;
                let open_amount = row.get::<_, String>(3)?;
                let installments_future = row.get::<_, String>(4)?;
                Ok(CardSummaryRow {
                    month_ref: row.get(0)?,
                    account_id: row.get(1)?,
                    total_charges: parse_decimal(total_charges).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    open_amount: parse_decimal(open_amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    installments_future: parse_decimal(installments_future).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    transaction_count: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn audit_duplicate_transactions(&self) -> Result<Vec<DuplicateTransactionGroup>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_date,
              account_id,
              amount_cents,
              LOWER(TRIM(raw_description)) AS norm_desc,
              COUNT(*) AS n,
              GROUP_CONCAT(transaction_id) AS ids,
              GROUP_CONCAT(DISTINCT source) AS sources
            FROM transactions
            GROUP BY transaction_date, COALESCE(account_id, ''), amount_cents, norm_desc
            HAVING COUNT(*) > 1
            ORDER BY n DESC, ABS(amount_cents) DESC, transaction_date DESC
            ",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let date_str = row.get::<_, String>(0)?;
                let amount_cents = row.get::<_, i64>(2)?;
                let ids_raw = row.get::<_, String>(5)?;
                let sources_raw = row.get::<_, Option<String>>(6)?.unwrap_or_default();
                let transaction_date =
                    NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?;
                Ok(DuplicateTransactionGroup {
                    transaction_ids: crate::models::split_csv_sorted(&ids_raw),
                    sources: crate::models::split_csv_sorted(&sources_raw),
                    transaction_date,
                    account_id: row.get::<_, Option<String>>(1)?,
                    amount: Decimal::new(amount_cents, 2),
                    description: row.get(3)?,
                    count: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn card_closed_transactions(
        &self,
        month_ref: Option<&str>,
    ) -> Result<Vec<CardClosedTransactionRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              strftime('%Y-%m', t.transaction_date) AS month_ref,
              t.account_id,
              t.transaction_id,
              t.transaction_date,
              t.raw_description,
              COALESCE(t.description, t.merchant_name, t.raw_description),
              CAST(t.amount AS TEXT) AS amount,
              t.category_id,
              t.payment_status,
              t.metadata_json
            FROM v_transactions_reportable t
            JOIN accounts a ON a.account_id = t.account_id
            WHERE a.account_type = 'credit'
              AND NOT (t.amount_cents > 0 AND LOWER(t.raw_description) LIKE '%pagamento recebido%')
              AND COALESCE(t.category_id, '') NOT IN (SELECT category_id FROM internal_categories)
              AND (?1 IS NULL OR strftime('%Y-%m', t.transaction_date) = ?1)
            ORDER BY t.transaction_date DESC, ABS(t.amount_cents) DESC, t.transaction_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([month_ref], |row| {
                let transaction_date = row.get::<_, String>(3)?;
                let amount = row.get::<_, String>(6)?;
                let metadata_json = row.get::<_, String>(9)?;
                Ok(CardClosedTransactionRow {
                    month_ref: row.get(0)?,
                    account_id: row.get(1)?,
                    transaction_id: row.get(2)?,
                    transaction_date: parse_sql_date(transaction_date, 3)?,
                    label: row.get(4)?,
                    description: row.get(5)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    category_id: row.get(7)?,
                    payment_status: row.get(8)?,
                    metadata_json: parse_sql_json(metadata_json, 9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn card_reportable_transactions(
        &self,
        month_ref: Option<&str>,
    ) -> Result<Vec<CardClosedTransactionRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              strftime('%Y-%m', t.transaction_date) AS month_ref,
              t.account_id,
              t.transaction_id,
              t.transaction_date,
              t.display_label,
              COALESCE(t.description, t.merchant_name, t.raw_description),
              CAST(ABS(t.amount_cents) / 100.0 AS TEXT) AS amount,
              t.category_id,
              t.payment_status,
              t.metadata_json
            FROM v_transactions_reportable t
            JOIN accounts a ON a.account_id = t.account_id
            WHERE a.account_type = 'credit'
              AND t.amount_cents < 0
              AND COALESCE(t.category_id, '') NOT IN (SELECT category_id FROM internal_categories)
              AND (?1 IS NULL OR strftime('%Y-%m', t.transaction_date) = ?1)
            ORDER BY t.transaction_date DESC, ABS(t.amount_cents) DESC, t.transaction_id ASC
            ",
        )?;
        let rows = stmt
            .query_map([month_ref], |row| {
                let transaction_date = row.get::<_, String>(3)?;
                let amount = row.get::<_, String>(6)?;
                let metadata_json = row.get::<_, String>(9)?;
                Ok(CardClosedTransactionRow {
                    month_ref: row.get(0)?,
                    account_id: row.get(1)?,
                    transaction_id: row.get(2)?,
                    transaction_date: parse_sql_date(transaction_date, 3)?,
                    label: row.get(4)?,
                    description: row.get(5)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    category_id: row.get(7)?,
                    payment_status: row.get(8)?,
                    metadata_json: parse_sql_json(metadata_json, 9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn uncategorized(&self, limit: usize) -> Result<Vec<UncategorizedRow>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              t.transaction_id,
              t.transaction_date,
              t.display_label,
              CAST(t.amount AS TEXT),
              t.account_id,
              a.label,
              t.tx_type,
              t.category_source,
              t.payment_status,
              t.source,
              t.metadata_json
            FROM v_transactions_reportable t
            LEFT JOIN accounts a ON a.account_id = t.account_id
            WHERE t.category_id IS NULL
               OR t.category_source IN ('unclassified', 'fallback')
            ORDER BY t.transaction_date DESC, ABS(t.amount) DESC, t.transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map([limit as i64], |row| {
                let transaction_date = row.get::<_, String>(1)?;
                let amount = row.get::<_, String>(3)?;
                let metadata_json = row.get::<_, String>(10)?;
                Ok(UncategorizedRow {
                    transaction_id: row.get(0)?,
                    transaction_date: parse_sql_date(transaction_date, 1)?,
                    description: row.get(2)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            3,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    account_id: row.get(4)?,
                    account_label: row.get(5)?,
                    tx_type: row.get(6)?,
                    category_source: row.get(7)?,
                    payment_status: row.get(8)?,
                    source: row.get(9)?,
                    metadata_json: parse_sql_json(metadata_json, 10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn count_uncategorized(&self) -> Result<i64> {
        let conn = self.connection()?;
        let count = conn.query_row(
            "SELECT COUNT(*) FROM v_transactions_reportable
             WHERE category_id IS NULL
                OR category_source IN ('unclassified', 'fallback')",
            [],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    async fn count_rows(&self, table: &str) -> Result<i64> {
        // `validate_table_name` enforces the allowlist, so interpolation is
        // safe; SQLite identifier quoting uses double quotes per the SQL
        // standard, not the SQL-Server-style brackets the previous code
        // borrowed.
        super::validate_table_name(table)?;
        let conn = self.connection()?;
        let sql = format!("SELECT COUNT(*) FROM \"{table}\"");
        let count = conn.query_row(&sql, [], |row| row.get(0))?;
        Ok(count)
    }

    async fn upsert_category_budget(&self, record: &CategoryBudgetRecord) -> Result<()> {
        let conn = self.connection()?;
        conn.execute(
            "
            INSERT INTO category_budgets (
              budget_id, category_id, subcategory_id, month_ref, amount,
              alert_threshold_pct, actor_id, idempotency_key, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(category_id, COALESCE(subcategory_id, ''), COALESCE(month_ref, '_default')) DO UPDATE SET
              budget_id = excluded.budget_id,
              amount = excluded.amount,
              alert_threshold_pct = excluded.alert_threshold_pct,
              actor_id = excluded.actor_id,
              idempotency_key = excluded.idempotency_key,
              updated_at = excluded.updated_at
            ",
            params![
                record.budget_id,
                record.category_id,
                record.subcategory_id,
                record.month_ref,
                decimal_to_sql(&record.amount),
                record.alert_threshold_pct,
                record.actor_id,
                record.idempotency_key,
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    async fn list_category_budgets(
        &self,
        month: Option<&str>,
    ) -> Result<Vec<CategoryBudgetRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              budget_id, category_id, subcategory_id, month_ref,
              CAST(amount AS TEXT), alert_threshold_pct,
              actor_id, idempotency_key, created_at, updated_at
            FROM category_budgets
            WHERE (?1 IS NULL OR month_ref = ?1 OR month_ref IS NULL)
            ORDER BY category_id ASC, subcategory_id ASC, month_ref ASC
            ",
        )?;
        let rows = stmt
            .query_map([month], |row| {
                let amount = row.get::<_, String>(4)?;
                let created_at: String = row.get(8)?;
                let updated_at: String = row.get(9)?;
                Ok(CategoryBudgetRecord {
                    budget_id: row.get(0)?,
                    category_id: row.get(1)?,
                    subcategory_id: row.get(2)?,
                    month_ref: row.get(3)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    alert_threshold_pct: row.get(5)?,
                    actor_id: row.get(6)?,
                    idempotency_key: row.get(7)?,
                    created_at: parse_datetime_or_now(Some(&created_at)),
                    updated_at: parse_datetime_or_now(Some(&updated_at)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn budget_status_for_month(&self, month: &str) -> Result<Vec<BudgetStatusRow>> {
        let conn = self.connection()?;
        // Fetch spend for this month aggregated by category (no subcategory split in v_monthly_spend)
        let mut spend_stmt = conn.prepare(
            "
            SELECT category_id, CAST(SUM(CAST(expenses AS REAL)) AS TEXT)
            FROM v_monthly_spend
            WHERE month_ref = ?1
            GROUP BY category_id
            ",
        )?;
        let mut spend_by_cat = std::collections::BTreeMap::<String, Decimal>::new();
        let spend_rows = spend_stmt
            .query_map([month], |row| {
                let cat: String = row.get(0)?;
                let expenses: String = row.get(1)?;
                Ok((cat, expenses))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (cat, expenses_str) in spend_rows {
            let expenses = parse_decimal(expenses_str).unwrap_or(Decimal::ZERO);
            spend_by_cat.insert(cat, expenses);
        }

        // Fetch budgets: explicit month wins over default
        let mut budget_stmt = conn.prepare(
            "
            SELECT
              budget_id, category_id, subcategory_id, month_ref,
              CAST(amount AS TEXT), alert_threshold_pct,
              actor_id, idempotency_key, created_at, updated_at
            FROM category_budgets
            WHERE month_ref = ?1 OR month_ref IS NULL
            ORDER BY category_id ASC, subcategory_id ASC,
                     CASE WHEN month_ref IS NOT NULL THEN 0 ELSE 1 END ASC
            ",
        )?;
        let budget_rows = budget_stmt
            .query_map([month], |row| {
                let amount: String = row.get(4)?;
                let created_at: String = row.get(8)?;
                let updated_at: String = row.get(9)?;
                Ok(CategoryBudgetRecord {
                    budget_id: row.get(0)?,
                    category_id: row.get(1)?,
                    subcategory_id: row.get(2)?,
                    month_ref: row.get(3)?,
                    amount: parse_decimal(amount).map_err(|err| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(io::Error::new(io::ErrorKind::InvalidData, err.to_string())),
                        )
                    })?,
                    alert_threshold_pct: row.get(5)?,
                    actor_id: row.get(6)?,
                    idempotency_key: row.get(7)?,
                    created_at: parse_datetime_or_now(Some(&created_at)),
                    updated_at: parse_datetime_or_now(Some(&updated_at)),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // Dedup: explicit month_ref takes precedence over default (null)
        let mut seen =
            std::collections::BTreeMap::<(String, Option<String>), CategoryBudgetRecord>::new();
        for record in budget_rows {
            let key = (record.category_id.clone(), record.subcategory_id.clone());
            let entry = seen.entry(key);
            match entry {
                std::collections::btree_map::Entry::Vacant(e) => {
                    e.insert(record);
                }
                std::collections::btree_map::Entry::Occupied(mut e) => {
                    // Explicit month_ref wins over default (null)
                    if record.month_ref.is_some() {
                        e.insert(record);
                    }
                }
            }
        }

        // Compute projection factors
        let today = Utc::now().date_naive();
        let current_month = today.format("%Y-%m").to_string();
        let (day_of_month, days_in_month) = if month == current_month {
            let day = today.day();
            // Last day of month: advance to next month day 1, subtract 1
            let first_next = if today.month() == 12 {
                NaiveDate::from_ymd_opt(today.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(today.year(), today.month() + 1, 1)
            }
            .unwrap_or(today);
            let last = first_next.checked_sub_days(Days::new(1)).unwrap_or(today);
            (day, last.day())
        } else {
            (0u32, 0u32)
        };

        let mut results = Vec::new();
        for ((cat, _sub), record) in seen {
            let actual = spend_by_cat.get(&cat).copied().unwrap_or(Decimal::ZERO);
            let budget = record.amount;
            let usage_pct = if budget.is_zero() {
                Decimal::ZERO
            } else {
                (actual / budget * Decimal::from(100)).round_dp(2)
            };
            let projected_pct = if month == current_month && day_of_month > 0 {
                let projected = actual / Decimal::from(day_of_month) * Decimal::from(days_in_month);
                if budget.is_zero() {
                    Decimal::ZERO
                } else {
                    (projected / budget * Decimal::from(100)).round_dp(2)
                }
            } else {
                usage_pct
            };
            let alert = usage_pct >= Decimal::from(record.alert_threshold_pct);
            results.push(BudgetStatusRow {
                category_id: cat,
                subcategory_id: record.subcategory_id,
                month_ref: month.to_string(),
                budget_amount: budget,
                actual_amount: actual,
                usage_pct,
                projected_pct,
                alert,
                alert_threshold_pct: record.alert_threshold_pct,
            });
        }
        results.sort_by(|a, b| a.category_id.cmp(&b.category_id));
        Ok(results)
    }

    async fn transactions_on_date(
        &self,
        date: NaiveDate,
        account_id: &str,
        exclude_id: &str,
    ) -> Result<Vec<crate::enrichment::types::ContextTx>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              raw_description,
              CAST(amount AS TEXT),
              metadata_json,
              CAST(json_extract(metadata_json, '$.raw.order') AS INTEGER) AS pluggy_order
            FROM transactions
            WHERE transaction_date = ?1
              AND account_id = ?2
              AND transaction_id != ?3
            ORDER BY pluggy_order IS NULL, pluggy_order ASC, raw_description ASC
            ",
        )?;
        let rows = stmt
            .query_map(
                params![date.format("%Y-%m-%d").to_string(), account_id, exclude_id,],
                |row| {
                    let description: String = row.get(0)?;
                    let amount: String = row.get(1)?;
                    let metadata_json: String = row.get(2)?;
                    let order: Option<i64> = row.get(3)?;
                    Ok((description, amount, metadata_json, order))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut out = Vec::with_capacity(rows.len());
        for (description, amount_text, metadata_text, order) in rows {
            let amount = parse_decimal(amount_text)
                .map_err(|err| anyhow::anyhow!("amount inválido em transactions_on_date: {err}"))?;
            let metadata: Value = serde_json::from_str(&metadata_text).unwrap_or(Value::Null);
            let pluggy_category = crate::enrichment::context::extract_pluggy_category(&metadata);
            out.push(crate::enrichment::types::ContextTx {
                description,
                amount,
                pluggy_category,
                order,
            });
        }
        Ok(out)
    }

    async fn find_anatomy_donors(
        &self,
        merchant_name: &str,
        exclude_id: &str,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let normalized = merchant_name.trim().to_lowercase();
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM transactions
            WHERE LOWER(TRIM(COALESCE(NULLIF(TRIM(merchant_name), ''), NULLIF(TRIM(raw_description), '')))) = ?1
              AND transaction_id != ?2
              AND (
                NULLIF(TRIM(COALESCE(description, '')), '') IS NOT NULL
                OR NULLIF(TRIM(COALESCE(purpose, '')), '') IS NOT NULL
                OR (
                  NULLIF(TRIM(COALESCE(category_id, '')), '') IS NOT NULL
                  AND category_source IN ('manual', 'enriched:user')
                )
              )
            ORDER BY transaction_date DESC
            LIMIT 20
            ",
        )?;
        let rows = stmt
            .query_map(params![normalized, exclude_id], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn replicable_anatomy_candidates(&self, limit: usize) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM transactions
            WHERE COALESCE(NULLIF(TRIM(merchant_name), ''), NULLIF(TRIM(raw_description), '')) IS NOT NULL
              AND (
                description IS NULL OR TRIM(description) = ''
                OR purpose IS NULL OR TRIM(purpose) = ''
                OR category_id IS NULL OR TRIM(category_id) = ''
                OR category_source IN ('unclassified', 'fallback', 'pluggy')
              )
            ORDER BY transaction_date DESC, ABS(amount_cents) DESC, transaction_id ASC
            LIMIT ?1
            ",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn similar_transactions(
        &self,
        keyword: &str,
        exclude_id: &str,
        only_uncategorized: bool,
    ) -> Result<Vec<TransactionRecord>> {
        let conn = self.connection()?;
        let pattern = format!("%{}%", keyword.to_ascii_lowercase());
        let category_filter = if only_uncategorized {
            "AND (category_id IS NULL OR category_source IN ('unclassified', 'fallback', 'pluggy'))"
        } else {
            ""
        };
        let sql = format!(
            "
            SELECT
              transaction_id, account_id, transaction_date, raw_description, description,
              merchant_name, purpose, CAST(amount AS TEXT), tx_type, category_id,
              category_source, context, classifier_trace, payment_status, source,
              actor_id, idempotency_key, metadata_json, created_at, updated_at,
              enrichment_attempted_at
            FROM transactions
            WHERE LOWER(raw_description) LIKE ?1
              AND transaction_id != ?2
              {category_filter}
            ORDER BY transaction_date DESC, transaction_id ASC
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![pattern, exclude_id], transaction_record_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    async fn mark_enrichment_attempted(
        &self,
        transaction_id: &str,
        actor_id: &str,
        idempotency_key: &str,
    ) -> Result<()> {
        let conn = self.connection()?;
        let now = Utc::now();
        let affected = conn.execute(
            "UPDATE transactions
             SET enrichment_attempted_at = ?1
             WHERE transaction_id = ?2",
            params![now.to_rfc3339(), transaction_id],
        )?;
        if affected == 0 {
            bail!("Transação {transaction_id} não encontrada");
        }
        conn.execute(
            "INSERT OR IGNORE INTO audit_log (
                event_id, entity_type, entity_id, action, actor_id,
                event_timestamp, idempotency_key, diff_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                uuid::Uuid::now_v7().to_string(),
                "transaction",
                transaction_id,
                "enrich_attempted",
                actor_id,
                now.to_rfc3339(),
                idempotency_key,
                serde_json::json!({"enrichment_attempted_at": now.to_rfc3339()}).to_string(),
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LocalStore;
    use crate::config::AppConfig;
    use crate::migrations::run_migrations;
    use crate::models::{AccountRecord, TransactionRecord};
    use crate::storage::FinanceStore;
    use chrono::{NaiveDate, Utc};
    use rust_decimal::Decimal;

    fn test_config(path: std::path::PathBuf) -> AppConfig {
        AppConfig {
            local_db_path: Some(path),
            actor_id: "test".to_string(),
            ..AppConfig::default()
        }
    }

    fn tx(id: &str, source: &str, description: &str, amount: Decimal) -> TransactionRecord {
        let now = Utc::now();
        TransactionRecord {
            transaction_id: id.to_string(),
            account_id: Some("acc-1".to_string()),
            transaction_date: NaiveDate::from_ymd_opt(2026, 5, 10).unwrap(),
            raw_description: description.to_string(),
            description: None,
            merchant_name: None,
            purpose: None,
            amount,
            amount_cents: None,
            tx_type: "debit".to_string(),
            category_id: Some("alimentacao:mercado".to_string()),
            category_source: "rule".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: source.to_string(),
            actor_id: "test".to_string(),
            idempotency_key: format!("{source}:{id}"),
            metadata_json: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            enrichment_attempted_at: None,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cashflow_reportable_dedupes_ofx_shadowed_by_pluggy() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        store
            .upsert_transactions(&[
                tx(
                    "pluggy",
                    "pluggy",
                    "Compra Exemplo",
                    Decimal::new(-11603, 2),
                ),
                tx("ofx-dupe", "ofx", "Compra Exemplo", Decimal::new(-11603, 2)),
                tx("ofx-unique", "ofx", "Compra Unica", Decimal::new(-5000, 2)),
            ])
            .await
            .unwrap();

        let rows = store.cashflow_reportable().await.unwrap();
        let may = rows
            .iter()
            .find(|row| row.month_ref == "2026-05")
            .expect("may cashflow");

        assert_eq!(may.expenses, Decimal::new(16603, 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cashflow_reportable_dedupes_ofx_shifted_one_day_from_pluggy() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        let mut ofx_next_day = tx("ofx-dupe", "ofx", "Compra Exemplo", Decimal::new(-3400, 2));
        ofx_next_day.transaction_date = NaiveDate::from_ymd_opt(2026, 5, 11).unwrap();
        let mut ofx_far = tx("ofx-far", "ofx", "Compra Exemplo", Decimal::new(-3400, 2));
        ofx_far.transaction_date = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        store
            .upsert_transactions(&[
                tx("pluggy", "pluggy", "Compra Exemplo", Decimal::new(-3400, 2)),
                ofx_next_day,
                ofx_far,
            ])
            .await
            .unwrap();

        let rows = store.cashflow_reportable().await.unwrap();
        let may = rows
            .iter()
            .find(|row| row.month_ref == "2026-05")
            .expect("may cashflow");

        assert_eq!(may.expenses, Decimal::new(6800, 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reportable_prefers_posted_installment_over_projected_twin() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        let installment = |id: &str, day: u32, desc: &str, cents: i64, bill: Option<&str>| {
            let mut row = tx(id, "pluggy", desc, Decimal::new(cents, 2));
            row.transaction_date = NaiveDate::from_ymd_opt(2026, 8, day).unwrap();
            let marker = desc.rsplit(' ').next().unwrap();
            let (k, n) = marker.split_once('/').unwrap();
            row.metadata_json = serde_json::json!({ "raw": { "creditCardMetadata": {
                "installmentNumber": k.parse::<u32>().unwrap(),
                "totalInstallments": n.parse::<u32>().unwrap(),
                "billId": bill,
            }}});
            row
        };
        store
            .upsert_transactions(&[
                installment("projected", 17, "Loja Exemplo 4/6", -36283, None),
                installment("posted", 3, "Loja Exemplo 4/6", -36283, Some("bill-1")),
                installment("lonely", 20, "Outra Loja 1/3", -10000, None),
            ])
            .await
            .unwrap();

        let rows = store.cashflow_reportable().await.unwrap();
        let august = rows
            .iter()
            .find(|row| row.month_ref == "2026-08")
            .expect("august cashflow");

        assert_eq!(august.expenses, Decimal::new(46283, 2));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn plan_scenario_and_changes_round_trip() {
        use crate::models::{PlanChangeRecord, PlanScenarioRecord};

        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        let now = Utc::now();
        let scenario = PlanScenarioRecord {
            scenario_id: "scn-1".to_string(),
            name: "plano de teste".to_string(),
            description: None,
            status: "ativo".to_string(),
            promoted_at: None,
            metadata_json: serde_json::json!({}),
            actor_id: "test".to_string(),
            idempotency_key: "scn-1".to_string(),
            created_at: now,
            updated_at: now,
        };
        store
            .upsert_plan_scenarios(std::slice::from_ref(&scenario))
            .await
            .unwrap();

        // Upsert is idempotent on the primary key.
        store.upsert_plan_scenarios(&[scenario]).await.unwrap();
        let listed = store.list_plan_scenarios(Some("ativo")).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "plano de teste");

        let change = PlanChangeRecord {
            change_id: "chg-1".to_string(),
            scenario_id: "scn-1".to_string(),
            kind: "add_one_shot".to_string(),
            target_forecast_id: None,
            target_template_id: None,
            month: Some("2026-09".to_string()),
            effective_from: None,
            amount: Some(Decimal::new(-200000, 2)),
            months_count: None,
            description: Some("viagem".to_string()),
            category_id: Some("lazer".to_string()),
            account_id: None,
            status: "ativo".to_string(),
            payload_json: serde_json::json!({}),
            actor_id: "test".to_string(),
            idempotency_key: "chg-1".to_string(),
            created_at: now,
            updated_at: now,
        };
        store.upsert_plan_changes(&[change]).await.unwrap();

        let changes = store.list_plan_changes("scn-1", None).await.unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "add_one_shot");
        assert_eq!(changes[0].amount, Some(Decimal::new(-200000, 2)));
        assert_eq!(changes[0].month.as_deref(), Some("2026-09"));

        // Status transition sets promoted_at only for `promovido`.
        store
            .set_plan_scenario_status("scn-1", "promovido", "test")
            .await
            .unwrap();
        let promoted = store.get_plan_scenario("scn-1").await.unwrap().unwrap();
        assert_eq!(promoted.status, "promovido");
        assert!(promoted.promoted_at.is_some());

        // Unknown scenario errors instead of silently no-oping.
        assert!(store
            .set_plan_scenario_status("scn-missing", "arquivado", "test")
            .await
            .is_err());

        store.delete_plan_change("chg-1").await.unwrap();
        assert!(store
            .list_plan_changes("scn-1", None)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commitment_tier_override_upserts_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        // Set then read back.
        store
            .set_commitment_tier("t1", Some("locked"), "test", "idem-1")
            .await
            .unwrap();
        assert_eq!(
            store.commitment_tier_overrides().await.unwrap(),
            vec![("t1".to_string(), "locked".to_string())]
        );

        // Re-setting the same transaction overwrites in place (no duplicate row).
        store
            .set_commitment_tier("t1", Some("variable"), "test", "idem-2")
            .await
            .unwrap();
        assert_eq!(
            store.commitment_tier_overrides().await.unwrap(),
            vec![("t1".to_string(), "variable".to_string())]
        );

        // None clears the override.
        store
            .set_commitment_tier("t1", None, "test", "idem-3")
            .await
            .unwrap();
        assert!(store.commitment_tier_overrides().await.unwrap().is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn anatomy_replication_candidates_include_weak_or_missing_categories() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        let mut donor = tx(
            "donor",
            "pluggy",
            "Condominio Exemplo",
            Decimal::new(-120_000, 2),
        );
        donor.merchant_name = Some("Condominio Exemplo".to_string());
        donor.description = None;
        donor.purpose = None;
        donor.category_id = Some("moradia:condominio".to_string());
        donor.category_source = "manual".to_string();

        let mut target = tx(
            "target",
            "pluggy",
            "Condominio Exemplo",
            Decimal::new(-121_000, 2),
        );
        target.merchant_name = Some("Condominio Exemplo".to_string());
        target.description = Some("Condominio Exemplo".to_string());
        target.purpose = Some("Moradia".to_string());
        target.category_id = None;
        target.category_source = "unclassified".to_string();

        store
            .upsert_transactions(&[donor, target.clone()])
            .await
            .unwrap();

        let candidates = store.replicable_anatomy_candidates(10).await.unwrap();
        assert!(candidates
            .iter()
            .any(|row| row.transaction_id == target.transaction_id));

        let donors = store
            .find_anatomy_donors("Condominio Exemplo", &target.transaction_id)
            .await
            .unwrap();
        assert!(donors.iter().any(|row| row.transaction_id == "donor"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checking_balance_counts_bank_typed_accounts() {
        // Regression: Pluggy types checking/savings accounts as `bank`, not
        // `checking`. The cash-balance query must count them or the consolidated
        // balance reads 0 even with real snapshots.
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        let mut acc = card_account("nubank-cc", "0", "0");
        acc.account_type = "bank".to_string();
        store.upsert_accounts(&[acc]).await.unwrap();
        use crate::models::AccountSnapshotRecord;
        store
            .insert_account_snapshots(&[AccountSnapshotRecord {
                snapshot_id: "snap-1".into(),
                account_id: "nubank-cc".into(),
                snapshot_date: NaiveDate::from_ymd_opt(2026, 6, 18).unwrap(),
                balance: Some(Decimal::new(736075, 2)),
                credit_limit: None,
                currency_code: Some("BRL".into()),
                source: "pluggy".into(),
                actor_id: "test".into(),
                idempotency_key: "snap-1".into(),
                metadata_json: serde_json::Value::Object(Default::default()),
                created_at: Utc::now(),
            }])
            .await
            .unwrap();

        let bal = store
            .checking_balance_at(NaiveDate::from_ymd_opt(2026, 6, 20).unwrap())
            .await
            .unwrap();
        assert_eq!(bal.map(|b| b.balance), Some(Decimal::new(736075, 2)));
    }

    fn card_account(account_id: &str, closing_day: &str, due_day: &str) -> AccountRecord {
        let now = Utc::now();
        AccountRecord {
            account_id: account_id.to_string(),
            owner: "test".to_string(),
            account_type: "credit".to_string(),
            bank: "test-bank".to_string(),
            label: "Test Card".to_string(),
            pluggy_account_id: None,
            pluggy_item_id: None,
            status: "active".to_string(),
            actor_id: "test".to_string(),
            idempotency_key: format!("acct:{account_id}"),
            metadata_json: serde_json::json!({
                "billing_closing_day": closing_day,
                "billing_due_day": due_day,
            }),
            created_at: now,
            updated_at: now,
        }
    }

    fn card_tx(id: &str, account_id: &str, date: NaiveDate, amount: Decimal) -> TransactionRecord {
        let now = Utc::now();
        TransactionRecord {
            transaction_id: id.to_string(),
            account_id: Some(account_id.to_string()),
            transaction_date: date,
            raw_description: id.to_string(),
            description: None,
            merchant_name: None,
            purpose: None,
            amount,
            amount_cents: None,
            tx_type: "debit".to_string(),
            category_id: Some("alimentacao:mercado".to_string()),
            category_source: "rule".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: "pluggy".to_string(),
            actor_id: "test".to_string(),
            idempotency_key: format!("pluggy:{id}"),
            metadata_json: serde_json::json!({}),
            created_at: now,
            updated_at: now,
            enrichment_attempted_at: None,
        }
    }

    /// A card purchase belongs to the month its bill is paid, not its posting
    /// month. Closing day 3, due day 10: a 2026-04-28 swipe (day 28 > 3) closes
    /// in the 2026-05 cycle, due 2026-05-10 → cash_month 2026-05. See ADR-0025.
    #[tokio::test(flavor = "current_thread")]
    async fn cashflow_reportable_buckets_card_purchase_in_bill_due_month() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        store
            .upsert_accounts(&[card_account("cc-1", "3", "10")])
            .await
            .unwrap();
        store
            .upsert_transactions(&[card_tx(
                "swipe-1",
                "cc-1",
                NaiveDate::from_ymd_opt(2026, 4, 28).unwrap(),
                Decimal::new(-15000, 2),
            )])
            .await
            .unwrap();

        let rows = store.cashflow_reportable().await.unwrap();
        assert!(
            rows.iter().all(|r| r.month_ref != "2026-04"),
            "card purchase must not appear in its posting month (April)"
        );
        let may = rows
            .iter()
            .find(|r| r.month_ref == "2026-05")
            .expect("may cashflow");
        assert_eq!(may.expenses, Decimal::new(15000, 2));
    }

    /// When the due day precedes the closing day the bill is paid the following
    /// month. Closing 25, due 5: a 2026-04-10 swipe (day 10 <= 25) closes in the
    /// 2026-04 cycle but is due 2026-05-05 → cash_month 2026-05. See ADR-0025.
    #[tokio::test(flavor = "current_thread")]
    async fn cashflow_reportable_rolls_due_before_closing_to_next_month() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        store
            .upsert_accounts(&[card_account("cc-2", "25", "5")])
            .await
            .unwrap();
        store
            .upsert_transactions(&[card_tx(
                "swipe-2",
                "cc-2",
                NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(),
                Decimal::new(-8000, 2),
            )])
            .await
            .unwrap();

        let rows = store.cashflow_reportable().await.unwrap();
        assert!(
            rows.iter().all(|r| r.month_ref != "2026-04"),
            "bill paid in May must not appear in April"
        );
        let may = rows
            .iter()
            .find(|r| r.month_ref == "2026-05")
            .expect("may cashflow");
        assert_eq!(may.expenses, Decimal::new(8000, 2));
    }

    /// Two rows sharing a dedup fingerprint (same date/account/amount/desc) but
    /// different transaction_ids — the Pluggy-id-drift shape — surface as one
    /// duplicate group; a unique row does not.
    #[tokio::test(flavor = "current_thread")]
    async fn audit_duplicate_transactions_groups_by_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();

        store
            .upsert_transactions(&[
                tx("dup-a", "pluggy", "Compra Exemplo", Decimal::new(-11603, 2)),
                tx("dup-b", "pluggy", "Compra Exemplo", Decimal::new(-11603, 2)),
                tx("solo", "pluggy", "Compra Unica", Decimal::new(-5000, 2)),
            ])
            .await
            .unwrap();

        let groups = store.audit_duplicate_transactions().await.unwrap();
        assert_eq!(groups.len(), 1, "exactly one duplicate group");
        let group = &groups[0];
        assert_eq!(group.count, 2);
        assert_eq!(group.transaction_ids, vec!["dup-a", "dup-b"]);
        assert_eq!(group.amount, Decimal::new(-11603, 2));
    }

    /// Parity guard: the Rust `cash_month_for` must equal the `cash_month`
    /// column of `v_transactions_cashbasis` (the SQL view) for the same inputs.
    /// They are two hand-maintained copies of one boundary-sensitive rule
    /// (ADR-0025/0026); this test turns silent drift into a red test. Dates
    /// straddle the closing day and the year boundary.
    #[tokio::test(flavor = "current_thread")]
    async fn cash_month_for_matches_sql_view_column() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_config(dir.path().join("phai.db"));
        let store = LocalStore::new(config.clone()).unwrap();
        run_migrations(&store, &config).await.unwrap();
        store
            .upsert_accounts(&[card_account("cc-p", "3", "10")])
            .await
            .unwrap();
        let dates = [
            (2026, 4, 2),
            (2026, 4, 3),
            (2026, 4, 28),
            (2026, 5, 2),
            (2026, 12, 28),
            (2027, 1, 2),
        ];
        let txs: Vec<_> = dates
            .iter()
            .enumerate()
            .map(|(i, &(y, m, d))| {
                card_tx(
                    &format!("p{i}"),
                    "cc-p",
                    NaiveDate::from_ymd_opt(y, m, d).unwrap(),
                    Decimal::new(-1000, 2),
                )
            })
            .collect();
        store.upsert_transactions(&txs).await.unwrap();

        let conn = store.connection().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT transaction_date, cash_month FROM v_transactions_cashbasis \
                 WHERE account_id = 'cc-p'",
            )
            .unwrap();
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(rows.len(), dates.len());
        for (date_str, view_cash_month) in rows {
            let date = NaiveDate::parse_from_str(&date_str, "%Y-%m-%d").unwrap();
            let rust_cash_month = crate::cashflow::cash_month_for(date, true, Some(3), Some(10));
            assert_eq!(rust_cash_month, view_cash_month, "drift on {date_str}");
        }
    }
}
