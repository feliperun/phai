use crate::config::{AppConfig, BackendKind};
use crate::storage::FinanceStore;
use anyhow::{bail, Result};

type Migration = (&'static str, &'static str);

const SQLITE_MIGRATIONS: [Migration; 42] = [
    (
        "001_initial",
        include_str!("../../../schema/sqlite/001_initial.sql"),
    ),
    (
        "002_add_forecast",
        include_str!("../../../schema/sqlite/002_add_forecast.sql"),
    ),
    (
        "003_create_views",
        include_str!("../../../schema/sqlite/003_create_views.sql"),
    ),
    (
        "004_create_reporting_views",
        include_str!("../../../schema/sqlite/004_create_reporting_views.sql"),
    ),
    (
        "005_refine_reporting_views",
        include_str!("../../../schema/sqlite/005_refine_reporting_views.sql"),
    ),
    (
        "006_harden",
        include_str!("../../../schema/sqlite/006_harden.sql"),
    ),
    (
        "007_fix_credit_card_sign",
        include_str!("../../../schema/sqlite/007_fix_credit_card_sign.sql"),
    ),
    (
        "008_exclude_internal_from_reports",
        include_str!("../../../schema/sqlite/008_exclude_internal_from_reports.sql"),
    ),
    (
        "009_fix_remaining_credit_card_sign",
        include_str!("../../../schema/sqlite/009_fix_remaining_credit_card_sign.sql"),
    ),
    (
        "010_internal_categories_table",
        include_str!("../../../schema/sqlite/010_internal_categories_table.sql"),
    ),
    (
        "011_reclassify_internal_transfers",
        include_str!("../../../schema/sqlite/011_reclassify_internal_transfers.sql"),
    ),
    (
        "012_effective_transactions_view",
        include_str!("../../../schema/sqlite/012_effective_transactions_view.sql"),
    ),
    (
        "013_display_labels_view",
        include_str!("../../../schema/sqlite/013_display_labels_view.sql"),
    ),
    (
        "014_reportable_transactions_view",
        include_str!("../../../schema/sqlite/014_reportable_transactions_view.sql"),
    ),
    (
        "015_update_reportable_legacy_filter",
        include_str!("../../../schema/sqlite/015_update_reportable_legacy_filter.sql"),
    ),
    (
        "016_account_snapshots",
        include_str!("../../../schema/sqlite/016_account_snapshots.sql"),
    ),
    (
        "017_category_budgets",
        include_str!("../../../schema/sqlite/017_category_budgets.sql"),
    ),
    (
        "018_enrichment",
        include_str!("../../../schema/sqlite/018_enrichment.sql"),
    ),
    (
        "019_card_billing_cycle",
        include_str!("../../../schema/sqlite/019_card_billing_cycle.sql"),
    ),
    (
        "020_card_open_now_fix",
        include_str!("../../../schema/sqlite/020_card_open_now_fix.sql"),
    ),
    (
        "021_normalize_payment_status",
        include_str!("../../../schema/sqlite/021_normalize_payment_status.sql"),
    ),
    (
        "022_consolidate_slug_dashes",
        include_str!("../../../schema/sqlite/022_consolidate_slug_dashes.sql"),
    ),
    (
        "023_revisar_fallback",
        include_str!("../../../schema/sqlite/023_revisar_fallback.sql"),
    ),
    (
        "024_streaming_to_assinaturas",
        include_str!("../../../schema/sqlite/024_streaming_to_assinaturas.sql"),
    ),
    (
        "025_cashback_as_expense_reduction",
        include_str!("../../../schema/sqlite/025_cashback_as_expense_reduction.sql"),
    ),
    (
        "026_drop_phantom_account",
        include_str!("../../../schema/sqlite/026_drop_phantom_account.sql"),
    ),
    (
        "027_amount_cents",
        include_str!("../../../schema/sqlite/027_amount_cents.sql"),
    ),
    (
        "028_amount_cents_views",
        include_str!("../../../schema/sqlite/028_amount_cents_views.sql"),
    ),
    (
        "029_dedup_cashflow_card_summary",
        include_str!("../../../schema/sqlite/029_dedup_cashflow_card_summary.sql"),
    ),
    (
        "030_dedup_cashflow_card_summary",
        include_str!("../../../schema/sqlite/030_dedup_cashflow_card_summary.sql"),
    ),
    (
        "031_amount_cents_views_fix",
        include_str!("../../../schema/sqlite/031_amount_cents_views_fix.sql"),
    ),
    (
        "032_fix_reportable_dedup",
        include_str!("../../../schema/sqlite/032_fix_reportable_dedup.sql"),
    ),
    (
        "033_transaction_anatomy",
        include_str!("../../../schema/sqlite/033_transaction_anatomy.sql"),
    ),
    (
        "034_forecast_template",
        include_str!("../../../schema/sqlite/034_forecast_template.sql"),
    ),
    (
        "035_fix_card_summary_double_count",
        include_str!("../../../schema/sqlite/035_fix_card_summary_double_count.sql"),
    ),
    (
        "036_internal_same_person_transfer",
        include_str!("../../../schema/sqlite/036_internal_same_person_transfer.sql"),
    ),
    (
        "037_cashflow_basis",
        include_str!("../../../schema/sqlite/037_cashflow_basis.sql"),
    ),
    (
        "038_reportable_ofx_dedup",
        include_str!("../../../schema/sqlite/038_reportable_ofx_dedup.sql"),
    ),
    (
        "039_card_summary_closing_boundary",
        include_str!("../../../schema/sqlite/039_card_summary_closing_boundary.sql"),
    ),
    (
        "040_transaction_tier",
        include_str!("../../../schema/sqlite/040_transaction_tier.sql"),
    ),
    (
        "041_plan_scenario",
        include_str!("../../../schema/sqlite/041_plan_scenario.sql"),
    ),
    (
        "042_reportable_installment_posted_dedup",
        include_str!("../../../schema/sqlite/042_reportable_installment_posted_dedup.sql"),
    ),
];

const BIGQUERY_MIGRATIONS: [Migration; 42] = [
    (
        "001_initial",
        include_str!("../../../schema/bigquery/001_initial.sql"),
    ),
    (
        "002_add_forecast",
        include_str!("../../../schema/bigquery/002_add_forecast.sql"),
    ),
    (
        "003_create_views",
        include_str!("../../../schema/bigquery/003_create_views.sql"),
    ),
    (
        "004_create_reporting_views",
        include_str!("../../../schema/bigquery/004_create_reporting_views.sql"),
    ),
    (
        "005_refine_reporting_views",
        include_str!("../../../schema/bigquery/005_refine_reporting_views.sql"),
    ),
    (
        "006_harden",
        include_str!("../../../schema/bigquery/006_harden.sql"),
    ),
    (
        "007_fix_credit_card_sign",
        include_str!("../../../schema/bigquery/007_fix_credit_card_sign.sql"),
    ),
    (
        "008_exclude_internal_from_reports",
        include_str!("../../../schema/bigquery/008_exclude_internal_from_reports.sql"),
    ),
    (
        "009_fix_remaining_credit_card_sign",
        include_str!("../../../schema/bigquery/009_fix_remaining_credit_card_sign.sql"),
    ),
    (
        "010_internal_categories_table",
        include_str!("../../../schema/bigquery/010_internal_categories_table.sql"),
    ),
    (
        "011_reclassify_internal_transfers",
        include_str!("../../../schema/bigquery/011_reclassify_internal_transfers.sql"),
    ),
    (
        "012_effective_transactions_view",
        include_str!("../../../schema/bigquery/012_effective_transactions_view.sql"),
    ),
    (
        "013_display_labels_view",
        include_str!("../../../schema/bigquery/013_display_labels_view.sql"),
    ),
    (
        "014_transaction_splits",
        include_str!("../../../schema/bigquery/014_transaction_splits.sql"),
    ),
    (
        "015_reportable_transactions_view",
        include_str!("../../../schema/bigquery/015_reportable_transactions_view.sql"),
    ),
    (
        "016_update_reportable_legacy_filter",
        include_str!("../../../schema/bigquery/016_update_reportable_legacy_filter.sql"),
    ),
    (
        "017_account_snapshots",
        include_str!("../../../schema/bigquery/017_account_snapshots.sql"),
    ),
    (
        "018_category_budgets",
        include_str!("../../../schema/bigquery/018_category_budgets.sql"),
    ),
    (
        "019_enrichment",
        include_str!("../../../schema/bigquery/019_enrichment.sql"),
    ),
    (
        "020_card_billing_cycle",
        include_str!("../../../schema/bigquery/020_card_billing_cycle.sql"),
    ),
    (
        "021_card_open_now_fix",
        include_str!("../../../schema/bigquery/021_card_open_now_fix.sql"),
    ),
    (
        "022_normalize_payment_status",
        include_str!("../../../schema/bigquery/022_normalize_payment_status.sql"),
    ),
    (
        "023_consolidate_slug_dashes",
        include_str!("../../../schema/bigquery/023_consolidate_slug_dashes.sql"),
    ),
    (
        "024_revisar_fallback",
        include_str!("../../../schema/bigquery/024_revisar_fallback.sql"),
    ),
    (
        "025_streaming_to_assinaturas",
        include_str!("../../../schema/bigquery/025_streaming_to_assinaturas.sql"),
    ),
    (
        "026_cashback_as_expense_reduction",
        include_str!("../../../schema/bigquery/026_cashback_as_expense_reduction.sql"),
    ),
    (
        "027_drop_phantom_account",
        include_str!("../../../schema/bigquery/027_drop_phantom_account.sql"),
    ),
    (
        "028_amount_cents",
        include_str!("../../../schema/bigquery/028_amount_cents.sql"),
    ),
    (
        "029_amount_cents_views",
        include_str!("../../../schema/bigquery/029_amount_cents_views.sql"),
    ),
    (
        "030_dedup_cashflow_card_summary",
        include_str!("../../../schema/bigquery/030_dedup_cashflow_card_summary.sql"),
    ),
    (
        "031_amount_cents_views_fix",
        include_str!("../../../schema/bigquery/031_amount_cents_views_fix.sql"),
    ),
    (
        "032_fix_reportable_dedup",
        include_str!("../../../schema/bigquery/032_fix_reportable_dedup.sql"),
    ),
    (
        "033_transaction_anatomy",
        include_str!("../../../schema/bigquery/033_transaction_anatomy.sql"),
    ),
    (
        "034_forecast_template",
        include_str!("../../../schema/bigquery/034_forecast_template.sql"),
    ),
    (
        "035_fix_card_summary_double_count",
        include_str!("../../../schema/bigquery/035_fix_card_summary_double_count.sql"),
    ),
    (
        "036_internal_same_person_transfer",
        include_str!("../../../schema/bigquery/036_internal_same_person_transfer.sql"),
    ),
    (
        "037_cashflow_basis",
        include_str!("../../../schema/bigquery/037_cashflow_basis.sql"),
    ),
    (
        "038_reportable_ofx_dedup",
        include_str!("../../../schema/bigquery/038_reportable_ofx_dedup.sql"),
    ),
    (
        "039_card_summary_closing_boundary",
        include_str!("../../../schema/bigquery/039_card_summary_closing_boundary.sql"),
    ),
    (
        "040_transaction_tier",
        include_str!("../../../schema/bigquery/040_transaction_tier.sql"),
    ),
    (
        "041_plan_scenario",
        include_str!("../../../schema/bigquery/041_plan_scenario.sql"),
    ),
    (
        "042_reportable_installment_posted_dedup",
        include_str!("../../../schema/bigquery/042_reportable_installment_posted_dedup.sql"),
    ),
];

fn backend_migrations(backend: BackendKind) -> &'static [Migration] {
    match backend {
        BackendKind::Bigquery => &BIGQUERY_MIGRATIONS,
        BackendKind::Local => &SQLITE_MIGRATIONS,
    }
}

fn render_sql(config: &AppConfig, sql: &str) -> Result<String> {
    let mut rendered = sql.to_string();
    if matches!(config.effective_backend(), BackendKind::Bigquery) {
        rendered = rendered.replace("{{project_id}}", config.project_id()?);
        rendered = rendered.replace("{{dataset_id}}", config.dataset_id()?);
    }
    Ok(rendered)
}

pub async fn run_migrations(store: &dyn FinanceStore, config: &AppConfig) -> Result<Vec<String>> {
    let applied = store.applied_migrations().await?;
    let mut executed = Vec::new();

    for (version, raw_sql) in backend_migrations(config.effective_backend()) {
        if applied.contains(*version) {
            continue;
        }
        let sql = render_sql(config, raw_sql)?;
        eprintln!(
            "[migration] applying {version} ({len} bytes)",
            len = sql.len()
        );
        if let Err(ref e) = store.apply_sql(&sql).await {
            eprintln!("[migration] FAILED {version}: {e}\n--- SQL ---\n{sql}\n--- END SQL ---",);
            bail!("{e}");
        }
        store.record_migration(version).await?;
        executed.push((*version).to_string());
    }

    Ok(executed)
}

#[cfg(test)]
mod tests {
    use super::{BIGQUERY_MIGRATIONS, SQLITE_MIGRATIONS};
    use rusqlite::Connection;

    #[test]
    fn bigquery_migrations_include_transaction_splits() {
        assert!(BIGQUERY_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "014_transaction_splits"));
    }

    #[test]
    fn sqlite_migrations_do_not_include_transaction_splits() {
        assert!(SQLITE_MIGRATIONS
            .iter()
            .all(|(version, _)| *version != "014_transaction_splits"));
    }

    #[test]
    fn sqlite_migrations_include_category_budgets() {
        assert!(SQLITE_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "017_category_budgets"));
    }

    #[test]
    fn bigquery_migrations_include_category_budgets() {
        assert!(BIGQUERY_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "018_category_budgets"));
    }

    #[test]
    fn internal_same_person_transfer_migration_is_registered() {
        assert!(SQLITE_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "036_internal_same_person_transfer"));
        assert!(BIGQUERY_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "036_internal_same_person_transfer"));
    }

    #[test]
    fn sqlite_internal_same_person_transfer_migration_inserts_category() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        let sql = SQLITE_MIGRATIONS
            .iter()
            .find(|(version, _)| *version == "036_internal_same_person_transfer")
            .map(|(_, sql)| *sql)
            .expect("036 migration");
        conn.execute_batch(sql).expect("apply migration");

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM internal_categories WHERE category_id = 'same-person-transfer'",
                [],
                |row| row.get(0),
            )
            .expect("same-person-transfer row");
        assert_eq!(count, 1);
    }

    #[test]
    fn plan_scenario_migration_is_registered_in_both_backends() {
        assert!(SQLITE_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "041_plan_scenario"));
        assert!(BIGQUERY_MIGRATIONS
            .iter()
            .any(|(version, _)| *version == "041_plan_scenario"));
    }

    #[test]
    fn sqlite_plan_scenario_migration_creates_tables() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        let sql = SQLITE_MIGRATIONS
            .iter()
            .find(|(version, _)| *version == "041_plan_scenario")
            .map(|(_, sql)| *sql)
            .expect("041 migration");
        conn.execute_batch(sql).expect("apply migration");
        // Idempotent: applying twice must not fail.
        conn.execute_batch(sql).expect("re-apply migration");

        for table in ["plan_scenario", "plan_change"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .expect("table lookup");
            assert_eq!(count, 1, "missing table {table}");
        }
    }

    #[test]
    fn sqlite_transaction_anatomy_migration_preserves_raw_and_trace() {
        let conn = Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(
            "
            CREATE TABLE accounts (
              account_id TEXT PRIMARY KEY,
              account_type TEXT NOT NULL,
              metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE internal_categories (category_id TEXT PRIMARY KEY);
            CREATE TABLE forecast (
              forecast_id TEXT PRIMARY KEY,
              due_date TEXT,
              description TEXT NOT NULL,
              account_id TEXT,
              category_id TEXT,
              amount TEXT NOT NULL,
              created_at TEXT NOT NULL,
              status TEXT NOT NULL
            );
            CREATE TABLE transactions (
              transaction_id TEXT PRIMARY KEY,
              account_id TEXT,
              transaction_date TEXT NOT NULL,
              description TEXT NOT NULL,
              amount TEXT NOT NULL,
              amount_cents INTEGER GENERATED ALWAYS AS (CAST(ROUND(amount * 100) AS INTEGER)) VIRTUAL,
              tx_type TEXT NOT NULL,
              category_id TEXT,
              category_source TEXT NOT NULL,
              context TEXT,
              payment_status TEXT NOT NULL,
              source TEXT NOT NULL,
              actor_id TEXT NOT NULL,
              idempotency_key TEXT NOT NULL,
              metadata_json TEXT NOT NULL DEFAULT '{}',
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL,
              enrichment_attempted_at TEXT
            );
            INSERT INTO transactions (
              transaction_id, account_id, transaction_date, description, amount, tx_type,
              category_id, category_source, context, payment_status, source, actor_id,
              idempotency_key, metadata_json, created_at, updated_at, enrichment_attempted_at
            ) VALUES (
              'tx-raw-1', NULL, '2026-05-01', 'Mp *Emporioexemplo', '-53.00', 'debit',
              'alimentacao:restaurante', 'rule', 'MCC 5499; regra aplicada',
              'posted', 'pluggy', 'test', 'pluggy:tx-raw-1', '{}',
              '2026-05-01T00:00:00Z', '2026-05-01T00:00:00Z', NULL
            );
            ",
        )
        .expect("seed old schema");

        let sql = SQLITE_MIGRATIONS
            .iter()
            .find(|(version, _)| *version == "033_transaction_anatomy")
            .map(|(_, sql)| *sql)
            .expect("033 migration");
        conn.execute_batch(sql).expect("apply migration");

        let (raw, description, trace): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT raw_description, description, classifier_trace
                 FROM transactions
                 WHERE transaction_id = 'tx-raw-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("transaction anatomy row");
        assert_eq!(raw, "Mp *Emporioexemplo");
        assert!(description.is_none());
        assert_eq!(trace.as_deref(), Some("MCC 5499; regra aplicada"));

        let display_label: String = conn
            .query_row(
                "SELECT display_label FROM v_transactions_reportable WHERE transaction_id = 'tx-raw-1'",
                [],
                |row| row.get(0),
            )
            .expect("display label");
        assert!(display_label.contains("Mp *Emporioexemplo"));
    }
}
