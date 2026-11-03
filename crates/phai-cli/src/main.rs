use anyhow::{bail, Context, Result};
use chrono::{Datelike, Duration, NaiveDate, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use phai_core::enrichment::replication::{find_and_replicate, ReplicationOutcome};
use phai_core::idempotency::{
    category_id, ensure_account_idempotency, ensure_forecast_idempotency, ensure_rule_idempotency,
    ensure_transaction_idempotency, manual_transaction_idempotency, pluggy_transaction_fingerprint,
};
use phai_core::installments::{group_into_chains, InstallmentChain};
use phai_core::legacy::load_legacy_bundle;
use phai_core::migrations::run_migrations;
use phai_core::models::{
    decimal_from_str, is_checking_account_type, AccountRecord, AccountSnapshotRecord, AuditEvent,
    BudgetStatusRow, CardClosedTransactionRow, CashflowRow, CategoryBudgetRecord, CategoryRecord,
    ForecastRecord, ForecastVsActualRow, RuleRecord, TransactionRecord,
};
use phai_core::pluggy::{sync_pluggy, SyncPluggyParams};
use phai_core::rules::{apply_rules_with_facts, compile_rules};
use phai_core::splits::{
    build_split_records, parse_split_payload, validate_split_payload, SplitPayload, SplitPreview,
};
use phai_core::storage::{open_store, FinanceStore, TransactionAnatomyPatch};
use phai_core::{AppConfig, BackendKind, ConfigPaths};
use rust_decimal::Decimal;
use self_cmd::SelfCommand;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;
use uuid::Uuid;

mod cashflow_chart;
mod enrich;
mod forecast_cmd;
mod human_format;
mod mcp;
mod pulse;
mod scenario_cmd;
mod self_cmd;
mod serve;
mod serve_assets;
mod serve_cache;
mod serve_install;
mod serve_lock;
mod sync_notify;
mod update;
mod update_state;

use human_format::{
    bold, brl as hf_brl, category_emoji, month_label, progress_bar, subsection_header,
};

const UPSERT_BATCH_SIZE: usize = 50;
const AUDIT_BATCH_SIZE: usize = 25;
const DEFAULT_SYNC_LOOKBACK_DAYS: i64 = 14;
const DEFAULT_REVIEW_LIMIT: usize = 10;

/// Branded `--version` output. Plain text by design: it is piped, screenshotted,
/// and copied into issues, so it must render identically without a TTY.
const VERSION_BANNER: &str = concat!(
    "φ phai v",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "finanças da casa, inteligência de verdade.\n",
    "phai.run · github.com/phai-run/phai\n",
);

#[derive(Parser)]
#[command(
    name = "phai",
    disable_version_flag = true,
    about = "phai — finanças da casa, inteligência de verdade."
)]
struct Cli {
    /// Skip the automatic update check on startup.
    #[arg(long, global = true)]
    no_auto_update: bool,
    /// Print version information.
    #[arg(short = 'V', long = "version")]
    version: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Generate an encrypted activation invite for another device (owner-only).
    Invite {
        #[command(subcommand)]
        command: InviteCommand,
    },
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    Report {
        /// Emit report data as CSV instead of the WhatsApp-friendly summary.
        #[arg(long, global = true)]
        csv: bool,
        #[command(subcommand)]
        command: ReportCommand,
    },
    Tx {
        #[command(subcommand)]
        command: TxCommand,
    },
    Forecast {
        #[command(subcommand)]
        command: ForecastCommand,
    },
    /// Named what-if planning scenarios: persisted deltas over the live
    /// forecast baseline (ADR-0037).
    Scenario {
        #[command(subcommand)]
        command: scenario_cmd::ScenarioCommand,
    },
    Rule {
        #[command(subcommand)]
        command: RuleCommand,
    },
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    Budget {
        #[command(subcommand)]
        command: BudgetCommand,
    },
    /// Serve phai's read-only reports as MCP tools over stdio (for Claude,
    /// IDEs and other MCP clients). See ADR-0027.
    #[command(name = "mcp")]
    Mcp,
    /// Start the local web app (LiveStore UI) bridged to your store.
    #[command(name = "serve")]
    Serve {
        #[arg(
            long,
            default_value_t = 80,
            help = "porta do servidor web (80 = http://phai.localhost; portas <1024 exigem sudo)"
        )]
        port: u16,
        #[command(subcommand)]
        command: Option<ServeCommand>,
    },
    /// Push the daily pulse to an external channel (WhatsApp via webhook).
    Notify {
        #[command(subcommand)]
        command: NotifyCommand,
    },
    #[command(name = "self")]
    SelfCmd {
        #[command(subcommand)]
        command: SelfCommand,
    },
}

#[derive(Subcommand)]
enum ServeCommand {
    /// Install a launchd agent (or, with --system, a root daemon) so the web
    /// app runs at login/boot and stays up.
    Install(serve_install::InstallArgs),
    /// Stop and remove the launchd agent (or, with --system, the root daemon).
    Uninstall(serve_install::UninstallArgs),
}

#[derive(Subcommand)]
enum NotifyCommand {
    /// POST the rendered daily-pulse to a webhook (WhatsApp gateway).
    ///
    /// Reads `PHAI_WHATSAPP_WEBHOOK_URL` (required) and
    /// `PHAI_WHATSAPP_WEBHOOK_TOKEN` (optional, sent as
    /// `Authorization: Bearer <token>`); the legacy `FINANCE_OS_*` names are
    /// still honored. Posts JSON `{ "text": "..." }`.
    /// Designed for cron / scheduled tasks.
    Whatsapp(NotifyWhatsappArgs),
}

#[derive(Args)]
struct NotifyWhatsappArgs {
    /// Window of the rendered pulse, same semantics as `report daily-pulse --days`.
    #[arg(long, default_value_t = 1)]
    days: i64,
    /// Print the body to stdout in addition to posting. Useful for cron logs.
    #[arg(long)]
    echo: bool,
    /// Don't post — just render and print. Used to preview without sending.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum AuthCommand {
    Setup(AuthSetupArgs),
}

#[derive(Subcommand)]
enum InviteCommand {
    /// Seal a BigQuery service account + coordinates into an encrypted invite.
    Create(InviteCreateArgs),
}

#[derive(Args)]
struct InviteCreateArgs {
    /// Dedicated service-account JSON to embed (Data Editor + Job User only).
    #[arg(long)]
    service_account_path: PathBuf,
    /// Audit actor for the target device.
    #[arg(long)]
    actor_id: String,
    /// Friendly label for the target device, shown during activation.
    #[arg(long)]
    label: String,
    /// Capability hint surfaced in the UI. The real grant is the SA's GCP role.
    #[arg(long, default_value = "rw")]
    role: String,
    /// GCP project id. Defaults to the owner's configured project.
    #[arg(long)]
    project_id: Option<String>,
    /// BigQuery dataset id. Defaults to the owner's configured dataset.
    #[arg(long)]
    dataset_id: Option<String>,
    /// Read the passphrase from the first line of stdin instead of prompting
    /// (for scripting). Otherwise phai prompts twice with hidden input.
    #[arg(long)]
    passphrase_stdin: bool,
}

#[derive(Args)]
struct AuthSetupArgs {
    #[arg(long, value_enum)]
    backend: BackendArg,
    #[arg(long)]
    actor_id: String,
    #[arg(long)]
    project_id: Option<String>,
    #[arg(long)]
    dataset_id: Option<String>,
    #[arg(long)]
    service_account_path: Option<PathBuf>,
    #[arg(long)]
    local_db_path: Option<PathBuf>,
    #[arg(long)]
    pluggy_start_date: Option<String>,
}

#[derive(Subcommand)]
enum AdminCommand {
    Migrate,
    ImportLegacy(ImportLegacyArgs),
    Reclassify(ReclassifyArgs),
}

#[derive(Args)]
struct ReclassifyArgs {
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct ImportLegacyArgs {
    #[arg(long, default_value = "finance")]
    finance_root: PathBuf,
}

#[derive(Subcommand)]
enum SyncCommand {
    Pluggy(SyncPluggyArgs),
}

#[derive(Args)]
struct SyncPluggyArgs {
    /// Pluggy account/item config. Defaults to `pluggy_config_path` from the
    /// runtime config, then `finance/pluggy-config.json`.
    #[arg(long)]
    pluggy_config: Option<PathBuf>,
    /// Account registry (owner, type, bank, label). Defaults to
    /// `pluggy_accounts_csv_path` from the runtime config, then
    /// `finance/data/contas.csv`.
    #[arg(long)]
    accounts_csv: Option<PathBuf>,
    #[arg(long)]
    fixture: Option<PathBuf>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    json_summary: bool,
    #[arg(long)]
    notify_summary: bool,
    /// Skip the automatic post-sync enrichment hook (Phase 5).
    /// Useful in CI / batch jobs where the LLM is intentionally
    /// unavailable. Defaults to false — enrichment runs by default.
    #[arg(long)]
    no_enrich: bool,
    /// Skip the automatic post-sync forecast orchestrator
    /// (`fin forecast refresh`). Defaults to false — the orchestrator
    /// runs by default, but a non-zero exit from the refresh never
    /// fails the sync itself.
    #[arg(long)]
    no_forecast_refresh: bool,
}

#[derive(Subcommand)]
enum ReportCommand {
    #[command(
        about = "recent activity grouped by category, last N days",
        long_about = "Shows transactions from the last N days, grouped by spending category \
                      with totals and per-category breakdowns. Useful for a quick health-check \
                      on recent spending without waiting for a full monthly close. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    DailyPulse(DailyPulseArgs),
    #[command(
        about = "monthly expenses broken down by category and subcategory",
        long_about = "Summarises all expenses for a given calendar month, organised by top-level \
                      category and subcategory, with subtotals and a grand total. Defaults to the \
                      current month when --month is omitted. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    MonthlySpend(MonthlySpendArgs),
    #[command(
        about = "cash-basis monthly cashflow for checking accounts, with optional details and forecast",
        long_about = "Single-month cash-basis summary restricted to checking accounts \
                      (account_type='checking'). Shows opening balance (anchored on the latest \
                      Pluggy snapshot ≤ last day of the previous month), inflows, outflows, and \
                      closing balance. Credit card transactions are intentionally excluded — only \
                      the bill payment on the checking account counts as an outflow. Use \
                      --details to replace paid-card bill payments with bill components, and \
                      --forecast to add remaining forecast values for month-end simulation. \
                      Defaults to the current month when --month is omitted. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Cashflow(CashflowArgs),
    #[command(
        about = "renderiza um gráfico SVG da evolução de caixa (saldo, entradas, saídas) por mês",
        long_about = "Gera um gráfico SVG mostrando a evolução do caixa das contas correntes \
                      ao longo dos últimos N meses (default: 6). Inclui linha de saldo final \
                      por mês (snapshot-anchored), barras pareadas de entradas e saídas e, \
                      quando --forecast é passado, sobrepõe linhas tracejadas com os totais \
                      previstos de entrada e saída por mês a partir da tabela de forecasts. \
                      Use --text para um sparkline ASCII no terminal em vez (ou além) do SVG."
    )]
    CashflowChart(CashflowChartArgs),
    #[command(
        about = "planned amounts vs what actually happened, sorted by variance",
        long_about = "Compares each budget envelope's forecasted amount against actual spend for \
                      the selected month, ranked by the size of the variance so the biggest \
                      over/under-runs surface first. Useful for post-month budget reviews. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    ForecastVsActual(ForecastVsActualArgs),
    #[command(
        about = "(deprecated alias for `report cards`) credit card cycle totals",
        long_about = "Summarises credit card spending per card for the selected billing cycle. \
                      This subcommand is a deprecated alias — prefer `finance report cards` once \
                      that subcommand is available. Kept for backwards-compatibility with existing \
                      scripts and agent integrations. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    CardSummary(CardSummaryArgs),
    #[command(
        about = "(deprecated alias for `report cards --closed`) most recent closed bill insights",
        long_about = "Shows analysis of the most recently closed credit card bill: top merchants, \
                      category breakdown, and any anomalies worth reviewing before payment. \
                      This subcommand is a deprecated alias — prefer `finance report cards --closed` \
                      once available. WhatsApp-friendly by default; pass --raw for JSON."
    )]
    CardClosedInsights(CardClosedInsightsArgs),
    #[command(
        about = "active installment chains (parcelas), with projected end dates and amounts",
        long_about = "Lists all active installment purchase chains (parcelas), showing how many \
                      installments remain, the monthly amount, and the projected final payment date. \
                      Optionally filtered to a single account via --account-id. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Installments(InstallmentsArgs),
    #[command(
        about = "transactions still needing a category, grouped by description similarity",
        long_about = "Finds transactions that have no category or are tagged as unclassified, \
                      clusters them by description similarity so similar merchants appear together, \
                      and shows the most impactful ones first (by total amount). Use this to \
                      prioritise categorisation work. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Uncategorized(UncategorizedArgs),
    #[command(
        about = "read-only audit of duplicate transactions inflating expense totals",
        long_about = "Finds groups of transactions that share a dedup fingerprint \
                      (same date, account, amount and normalised description) but have \
                      different transaction ids — typically Pluggy re-assigning ids across \
                      syncs — which double-count in cashflow and per-category reports. \
                      This is READ-ONLY: it reports the groups so you can verify before any \
                      cleanup; deleting the extras is a separate guided step (see ADR-0025). \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Duplicates(DuplicatesArgs),
    #[command(
        about = "transactions whose total matches the sum of multiple smaller items in the same \
                 window (potential splits)",
        long_about = "Detects transactions that look like they should be split into sub-items — \
                      for example, a single supermarket charge whose amount equals the sum of \
                      several receipt line items imported from a different source. Scans the last \
                      N days. WhatsApp-friendly by default; pass --raw for JSON."
    )]
    SplitCandidates(SplitCandidatesArgs),
    #[command(
        about = "historical prices for a specific item across receipts (BigQuery only)",
        long_about = "Searches receipt line-item data (BigQuery backend only) for a specific \
                      product name and returns the price history across all matching purchases. \
                      Useful for tracking price inflation on frequently bought items. \
                      Pass --raw for JSON output instead of a formatted table."
    )]
    ItemPrices(ItemPricesArgs),
    #[command(
        about = "consistency checks across accounts, categories, rules, and idempotency keys",
        long_about = "Runs a battery of data-quality checks: orphaned transactions, missing \
                      account references, duplicate idempotency keys, rule conflicts, and more. \
                      Intended for periodic audits or after bulk imports to confirm data integrity. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    DataHealth(DataHealthArgs),
    #[command(
        about = "cash projection under a hypothetical extra income/expense for the month",
        long_about = "Projects the month-end balance assuming an extra one-off income or expense \
                      on top of the current actuals and the historical average for the remaining \
                      days. Useful for quick what-if questions before a large purchase or bonus. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Scenario(ScenarioArgs),
    #[command(
        about = "compare an OFX file against the database transaction-by-transaction",
        long_about = "Loads an OFX export from your bank and matches each transaction against \
                      what is already in the database, flagging mismatches in amount, date, or \
                      description and listing items present in only one source. Helpful for \
                      reconciliation after a manual import. Pass --raw for JSON output."
    )]
    OfxConsistency(OfxConsistencyArgs),
    #[command(
        about = "category budgets vs actual spend for the month, with progress bars and alerts",
        long_about = "Compares each category's defined budget against actual spend for the given \
                      month, renders a progress bar for each envelope, and highlights categories \
                      that are over budget or approaching their limit. Requires --month (YYYY-MM). \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    BudgetStatus(BudgetStatusArgs),
    #[command(
        about = "credit-card insights: charges, payment status, installments, and by-category breakdown",
        long_about = "Shows everything that hit your credit cards in a given month: per-card \
                      totals with payment status (paid/open/overdue, inferred by matching \
                      credit-card-payment debits in checking accounts), the active installment \
                      commitment, and every charge grouped by category. Use --month YYYY-MM for \
                      a specific cycle or --closed for each card's most recent closed bill. \
                      Replaces the deprecated `card-summary` and `card-closed-insights` reports. \
                      WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Cards(CardsArgs),
    #[command(
        about = "saldo em conta por dono e total, do snapshot mais recente do Pluggy",
        long_about = "Mostra o saldo em conta mais recente sincronizado do Pluggy para cada \
                      conta corrente, agrupado por dono (owner) com subtotais e o total geral. \
                      Cartões de crédito não aparecem aqui — para ver fatura em aberto, use \
                      `report cards` ou o pulse. WhatsApp-friendly by default; pass --raw for JSON."
    )]
    Balances(BalancesArgs),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportOutputFormat {
    Human,
    Json,
    Csv,
}

impl ReportOutputFormat {
    fn structured_output(self) -> bool {
        !matches!(self, Self::Human)
    }
}

fn report_output_format(csv: bool, json_output: bool) -> Result<ReportOutputFormat> {
    if csv && json_output {
        bail!("Use apenas uma saída estruturada: --raw/--json ou --csv");
    }

    if csv {
        Ok(ReportOutputFormat::Csv)
    } else if json_output {
        Ok(ReportOutputFormat::Json)
    } else {
        Ok(ReportOutputFormat::Human)
    }
}

fn print_report_output<T: Serialize>(format: ReportOutputFormat, value: &T) -> Result<()> {
    match format {
        ReportOutputFormat::Human => Ok(()),
        ReportOutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
            Ok(())
        }
        ReportOutputFormat::Csv => {
            let value = serde_json::to_value(value)?;
            let stdout = io::stdout();
            write_csv_from_value(&value, stdout.lock())
        }
    }
}

fn write_csv_from_value<W: io::Write>(value: &Value, mut writer: W) -> Result<()> {
    let rows = match value {
        Value::Array(rows) => rows.as_slice(),
        single => std::slice::from_ref(single),
    };
    if rows.is_empty() {
        return Ok(());
    }

    let mut headers = BTreeSet::new();
    for row in rows {
        collect_csv_headers(row, &mut headers);
    }
    let headers = headers.into_iter().collect::<Vec<_>>();
    write_csv_record(&headers, &mut writer)?;

    for row in rows {
        let cells = headers
            .iter()
            .map(|header| csv_cell_for_header(row, header))
            .collect::<Result<Vec<_>>>()?;
        write_csv_record(&cells, &mut writer)?;
    }

    Ok(())
}

fn collect_csv_headers(value: &Value, headers: &mut BTreeSet<String>) {
    if let Value::Object(map) = value {
        headers.extend(map.keys().cloned());
    } else {
        headers.insert("value".to_string());
    }
}

fn csv_cell_for_header(row: &Value, header: &str) -> Result<String> {
    match row {
        Value::Object(map) => csv_cell_value(map.get(header).unwrap_or(&Value::Null)),
        value if header == "value" => csv_cell_value(value),
        _ => Ok(String::new()),
    }
}

fn csv_cell_value(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => Ok(value.clone()),
        Value::Array(_) | Value::Object(_) => {
            serde_json::to_string(value).context("Falha ao serializar célula CSV")
        }
    }
}

fn write_csv_record<W: io::Write>(cells: &[String], writer: &mut W) -> Result<()> {
    for (idx, cell) in cells.iter().enumerate() {
        if idx > 0 {
            writer.write_all(b",")?;
        }
        write_csv_cell(cell, writer)?;
    }
    writer.write_all(b"\n")?;
    Ok(())
}

fn write_csv_cell<W: io::Write>(cell: &str, writer: &mut W) -> Result<()> {
    if cell.contains([',', '"', '\n', '\r']) {
        writer.write_all(b"\"")?;
        writer.write_all(cell.replace('"', "\"\"").as_bytes())?;
        writer.write_all(b"\"")?;
    } else {
        writer.write_all(cell.as_bytes())?;
    }
    Ok(())
}

#[derive(Args)]
struct BalancesArgs {
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
}

impl BalancesArgs {
    fn structured_output(&self) -> bool {
        self.raw
    }
}

#[derive(Args)]
struct DailyPulseArgs {
    /// Number of days to look back (e.g. 7 for the past week, 30 for a month).
    #[arg(long, default_value_t = 7)]
    days: i64,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`. Kept for backwards-compatibility with
    /// agents and scripts that still pass `--json`.
    #[arg(long, hide = true)]
    json: bool,
}

impl DailyPulseArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct MonthlySpendArgs {
    /// Target month in YYYY-MM format (e.g. 2024-11). Defaults to the current month.
    #[arg(long)]
    month: Option<String>,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`. Kept for backwards-compat with scripts/skills.
    #[arg(long, hide = true)]
    json: bool,
}

impl MonthlySpendArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct CashflowArgs {
    /// Target month in YYYY-MM format (e.g. 2024-11). Defaults to the current month.
    #[arg(long)]
    month: Option<String>,
    /// Show income and expense details grouped by category.
    #[arg(long)]
    details: bool,
    /// Add remaining forecast amounts to simulate month-end cashflow.
    #[arg(long)]
    forecast: bool,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`. Kept for backwards-compat with scripts/skills.
    #[arg(long, hide = true)]
    json: bool,
}

impl CashflowArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct CashflowChartArgs {
    /// Janela "para trás" em meses (incluindo o mês atual). Default 6, mín 2, máx 24.
    #[arg(long, default_value_t = 6)]
    months: usize,
    /// Meses "para a frente" a incluir (forecast-only). Quando `--forecast` é
    /// passado e este flag é omitido, o default é 6.
    #[arg(long)]
    months_ahead: Option<usize>,
    /// Caminho do arquivo SVG de saída. Default: ./finance-cashflow.svg
    #[arg(long)]
    output: Option<std::path::PathBuf>,
    /// Também imprime um sparkline ASCII no stdout (útil em terminal).
    #[arg(long)]
    text: bool,
    /// Empilha o forecast restante (entrada/saída ainda não realizada) em
    /// cima da barra realizada como extensão hachurada, e desenha o saldo
    /// projetado como continuação tracejada da linha de saldo.
    #[arg(long)]
    forecast: bool,
    /// Não escreve o SVG (útil junto com --text para sair só o sparkline).
    #[arg(long)]
    no_svg: bool,
    /// Sobrepõe um cenário hipotético no chart: linha pontilhada adicional
    /// e callout com o saldo final projetado. Aceita valor mensal (positivo
    /// = entrada extra, negativo = saída). Requer --forecast.
    #[arg(long, value_name = "BRL")]
    scenario_amount: Option<String>,
    /// Descrição livre do cenário para legenda/callout, ex.: "terapia 2x/mês".
    #[arg(long, value_name = "TEXTO", requires = "scenario_amount")]
    scenario_description: Option<String>,
    /// Mês de início do cenário no formato YYYY-MM. Default: próximo mês.
    #[arg(long, value_name = "YYYY-MM", requires = "scenario_amount")]
    scenario_start: Option<String>,
    /// Duração do cenário em meses. Default: usa todos os meses futuros do chart.
    #[arg(long, requires = "scenario_amount")]
    scenario_months: Option<u32>,
}

#[derive(Args)]
struct ForecastVsActualArgs {
    /// Target month in YYYY-MM format. Defaults to the current month.
    #[arg(long)]
    month: Option<String>,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`. Kept for backwards-compat with scripts/skills.
    #[arg(long, hide = true)]
    json: bool,
}

impl ForecastVsActualArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct CardSummaryArgs {
    /// Billing month in YYYY-MM format. Defaults to the current month.
    #[arg(long)]
    month: Option<String>,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl CardSummaryArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct CardClosedInsightsArgs {
    /// Billing month of the closed bill in YYYY-MM format. Defaults to the most recently closed cycle.
    #[arg(long)]
    month: Option<String>,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl CardClosedInsightsArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct InstallmentsArgs {
    /// Filter results to a single account (e.g. `nubank-credit`).
    #[arg(long)]
    account_id: Option<String>,
    /// How many months back to search for installment chains (default 12).
    #[arg(long, default_value_t = 12)]
    lookback_months: u32,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
    /// Show individual installment transactions in addition to chain summaries.
    #[arg(long)]
    verbose: bool,
}

impl InstallmentsArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct UncategorizedArgs {
    /// Maximum number of uncategorized transaction groups to show (default 20).
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl UncategorizedArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct DuplicatesArgs {
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl DuplicatesArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct SplitCandidatesArgs {
    /// Window (in days) to scan for matching sub-item sums (default 30).
    #[arg(long, default_value_t = 30)]
    days: i64,
    /// Emit machine-readable JSON.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl SplitCandidatesArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct ItemPricesArgs {
    /// Product name or keyword to search for across receipt line items (e.g. "leite integral").
    #[arg(long)]
    query: String,
    /// Earliest date to include, in YYYY-MM-DD format. Defaults to all available history.
    #[arg(long)]
    since: Option<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl ItemPricesArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct DataHealthArgs {
    /// How many days of recent data to include in recency-sensitive checks (default 180).
    #[arg(long, default_value_t = 180)]
    days: i64,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl DataHealthArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct ScenarioArgs {
    /// Month to project, in YYYY-MM format. Defaults to the current month.
    #[arg(long)]
    month: Option<String>,
    /// Number of past months used to estimate the daily spending rate (default 3).
    #[arg(long, default_value_t = 3)]
    history_months: usize,
    /// Hypothetical one-off extra expense to add to the projection (e.g. "500.00").
    #[arg(long, default_value = "0")]
    extra_expense: String,
    /// Hypothetical one-off extra income to add to the projection (e.g. "1000.00").
    #[arg(long, default_value = "0")]
    extra_income: String,
    /// Emit machine-readable JSON.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl ScenarioArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct OfxConsistencyArgs {
    /// Path to the OFX file exported from the bank (e.g. ~/Downloads/nubank.ofx).
    #[arg(long)]
    ofx: PathBuf,
    /// Account ID to scope the database lookup (e.g. `nubank-checking`). Auto-detected if omitted.
    #[arg(long)]
    account_id: Option<String>,
    /// How many days of date difference to still consider a match (default 1).
    #[arg(long, default_value_t = 1)]
    date_tolerance_days: i64,
    /// Emit machine-readable JSON.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl OfxConsistencyArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncSummaryTransaction {
    transaction_id: String,
    transaction_date: String,
    day_of_week: String,
    description: String,
    amount: String,
    tx_type: String,
    category_id: Option<String>,
    category_source: String,
    context: Option<String>,
    account_id: Option<String>,
    account_label: Option<String>,
    payment_status: String,
    source: String,
    metadata_json: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncSummaryPending {
    transaction_id: String,
    transaction_date: String,
    day_of_week: String,
    description: String,
    amount: String,
    tx_type: String,
    account_id: Option<String>,
    account_label: Option<String>,
    category_source: String,
    payment_status: String,
    source: String,
    metadata_json: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SyncSummaryOutput {
    actor_id: String,
    backend: String,
    generated_at: String,
    summary_status: String,
    warnings: Vec<String>,
    new_transactions_count: usize,
    needs_context_count: i64,
    needs_context_returned_count: usize,
    needs_context_truncated: bool,
    new_transactions: Vec<SyncSummaryTransaction>,
    needs_context: Vec<SyncSummaryPending>,
}

struct SyncPendingSummaryResult {
    summary_status: String,
    warnings: Vec<String>,
    needs_context_count: i64,
    needs_context_returned_count: usize,
    needs_context_truncated: bool,
    needs_context: Vec<SyncSummaryPending>,
}

fn compact_error_message(err: &anyhow::Error) -> String {
    format!("{err:#}")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ")
}

fn transaction_needs_context(row: &TransactionRecord) -> bool {
    row.category_id.is_none() || matches!(row.category_source.as_str(), "unclassified" | "fallback")
}

fn build_sync_pending_summary_fallback(
    transactions: &[TransactionRecord],
    existing_ids: &BTreeSet<String>,
    account_labels: &BTreeMap<String, String>,
    warnings: Vec<String>,
) -> SyncPendingSummaryResult {
    const NEEDS_CONTEXT_LIMIT: usize = 100;
    let total = transactions
        .iter()
        .filter(|row| !existing_ids.contains(&row.transaction_id))
        .filter(|row| transaction_needs_context(row))
        .count();
    let needs_context = transactions
        .iter()
        .filter(|row| !existing_ids.contains(&row.transaction_id))
        .filter(|row| transaction_needs_context(row))
        .take(NEEDS_CONTEXT_LIMIT)
        .map(|row| SyncSummaryPending {
            transaction_id: row.transaction_id.clone(),
            transaction_date: row.transaction_date.format("%Y-%m-%d").to_string(),
            day_of_week: day_of_week_text(row.transaction_date),
            description: row.display_description().to_string(),
            amount: decimal_text(row.amount),
            tx_type: row.tx_type.clone(),
            account_id: row.account_id.clone(),
            account_label: row
                .account_id
                .as_ref()
                .and_then(|account_id| account_labels.get(account_id).cloned()),
            category_source: row.category_source.clone(),
            payment_status: row.payment_status.clone(),
            source: row.source.clone(),
            metadata_json: row.metadata_json.clone(),
        })
        .collect::<Vec<_>>();

    SyncPendingSummaryResult {
        summary_status: "partial".to_string(),
        warnings,
        needs_context_count: total as i64,
        needs_context_returned_count: needs_context.len(),
        needs_context_truncated: total > needs_context.len(),
        needs_context,
    }
}

async fn load_sync_pending_summary(
    store: &dyn FinanceStore,
    transactions: &[TransactionRecord],
    existing_ids: &BTreeSet<String>,
    account_labels: &BTreeMap<String, String>,
) -> SyncPendingSummaryResult {
    const NEEDS_CONTEXT_LIMIT: usize = 100;

    let needs_context_count = match store.count_uncategorized().await {
        Ok(total) => total,
        Err(err) => {
            return build_sync_pending_summary_fallback(
                transactions,
                existing_ids,
                account_labels,
                vec![format!(
                    "needs_context_fallback_sync_only: count_unavailable: {}",
                    compact_error_message(&err)
                )],
            );
        }
    };

    let needs_context = match store.uncategorized(NEEDS_CONTEXT_LIMIT).await {
        Ok(rows) => rows
            .into_iter()
            .map(|row| SyncSummaryPending {
                transaction_id: row.transaction_id,
                transaction_date: row.transaction_date.format("%Y-%m-%d").to_string(),
                day_of_week: day_of_week_text(row.transaction_date),
                description: row.description,
                amount: decimal_text(row.amount),
                tx_type: row.tx_type,
                account_id: row.account_id,
                account_label: row.account_label,
                category_source: row.category_source,
                payment_status: row.payment_status,
                source: row.source,
                metadata_json: row.metadata_json,
            })
            .collect::<Vec<_>>(),
        Err(err) => {
            return build_sync_pending_summary_fallback(
                transactions,
                existing_ids,
                account_labels,
                vec![format!(
                    "needs_context_fallback_sync_only: list_unavailable: {}",
                    compact_error_message(&err)
                )],
            );
        }
    };

    SyncPendingSummaryResult {
        summary_status: "complete".to_string(),
        warnings: Vec::new(),
        needs_context_count,
        needs_context_returned_count: needs_context.len(),
        needs_context_truncated: needs_context_count > needs_context.len() as i64,
        needs_context,
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FlatCategoryCount {
    category_id: String,
    count: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DataHealthOutput {
    actor_id: String,
    backend: String,
    generated_at: String,
    window_start: String,
    window_days: i64,
    latest_pluggy_transaction_date: Option<String>,
    pluggy_lag_days: Option<i64>,
    total_transactions: i64,
    transactions_with_context: i64,
    context_coverage_ratio: f64,
    uncategorized_count: i64,
    active_rules_count: usize,
    window_rows: usize,
    window_pluggy_rows: usize,
    window_legacy_rows: usize,
    window_other_rows: usize,
    categorized_rows: usize,
    flat_category_rows: usize,
    overlap_candidates_count: usize,
    overlap_candidates: Vec<OverlapCandidate>,
    flat_categories: Vec<FlatCategoryCount>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OverlapCandidate {
    account_id: Option<String>,
    amount: String,
    legacy_transaction_id: String,
    legacy_date: String,
    legacy_description: String,
    pluggy_transaction_id: String,
    pluggy_date: String,
    pluggy_description: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScenarioOutput {
    target_month: String,
    baseline_months: Vec<String>,
    avg_income: Decimal,
    avg_expenses: Decimal,
    avg_net: Decimal,
    planning_expenses: Decimal,
    known_forecast_expenses: Decimal,
    known_forecast_count: usize,
    carryover_open_card_month: String,
    carryover_open_card_amount: Decimal,
    extra_income: Decimal,
    extra_expense: Decimal,
    projected_net: Decimal,
    projected_cash_after_card_carry: Decimal,
    looks_ok_after_card_carry: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowDetailItem {
    description: String,
    amount: Decimal,
    category_id: Option<String>,
    account_id: Option<String>,
    transaction_id: Option<String>,
    transaction_date: Option<NaiveDate>,
    source: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowCategoryGroup {
    category_id: String,
    subtotal: Decimal,
    transactions: usize,
    items: Vec<CashflowDetailItem>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowCardBillDetail {
    account_id: String,
    payment_transaction_id: Option<String>,
    payment_date: Option<NaiveDate>,
    paid_amount: Decimal,
    bill_total: Decimal,
    installments: CashflowCategoryGroup,
    subscriptions: CashflowCategoryGroup,
    other_categories: Vec<CashflowCategoryGroup>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowDetailSections {
    income: Vec<CashflowCategoryGroup>,
    expenses: Vec<CashflowCategoryGroup>,
    card_bills: Vec<CashflowCardBillDetail>,
    forecast: Vec<CashflowCategoryGroup>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowAccountBalance {
    account_id: String,
    owner: String,
    label: String,
    balance: Option<Decimal>,
    snapshot_date: NaiveDate,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowCardSummary {
    total: Decimal,
    paid_on: Vec<NaiveDate>,
    installments_total: Decimal,
    installment_transactions: usize,
    installments_released_this_month: Decimal,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CashflowMonthReport {
    month_ref: String,
    actual_summary: CashflowRow,
    forecast_summary: Option<CashflowRow>,
    summary: CashflowRow,
    account_balances: Vec<CashflowAccountBalance>,
    previous_details: Option<CashflowDetailSections>,
    card_summary: CashflowCardSummary,
    details: Option<CashflowDetailSections>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedAccountInsight {
    account_id: String,
    total_charges: Decimal,
    open_amount: Decimal,
    closed_amount: Decimal,
    closed_transactions: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedCategoryInsight {
    category_id: String,
    amount: Decimal,
    transactions: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedRecurringInsight {
    merchant_key: String,
    amount: Decimal,
    transactions: usize,
    months_detected: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedSubscriptionInsight {
    merchant_key: String,
    amount: Decimal,
    transactions: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedInstallmentInsight {
    merchant_key: String,
    marker: String,
    amount: Decimal,
    transactions: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CardClosedInsightsOutput {
    month_ref: String,
    accounts: Vec<CardClosedAccountInsight>,
    categories: Vec<CardClosedCategoryInsight>,
    recurring: Vec<CardClosedRecurringInsight>,
    subscriptions: Vec<CardClosedSubscriptionInsight>,
    closed_installments: Vec<CardClosedInstallmentInsight>,
    open_installments: Vec<CardClosedInstallmentInsight>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallmentChainView {
    account_id: String,
    base_description: String,
    total: u32,
    current: u32,
    first_date: String,
    projected_end: String,
    remaining: u32,
    released_next_month: bool,
    total_amount: Decimal,
}

impl InstallmentChainView {
    fn from_chain(chain: &InstallmentChain) -> Self {
        Self {
            account_id: chain.account_id.clone(),
            base_description: chain.base_description.clone(),
            total: chain.total,
            current: chain.current,
            first_date: chain.first_date.format("%Y-%m-%d").to_string(),
            projected_end: chain.projected_end.format("%Y-%m-%d").to_string(),
            remaining: chain.remaining,
            released_next_month: chain.released_next_month,
            total_amount: chain.total_amount,
        }
    }
}

#[derive(Debug, Clone)]
struct OfxTransaction {
    fit_id: Option<String>,
    transaction_date: NaiveDate,
    amount: Decimal,
    description: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfxConsistencyMatch {
    status: String,
    ofx_fit_id: Option<String>,
    ofx_date: String,
    ofx_amount: Decimal,
    ofx_description: String,
    transaction_id: Option<String>,
    account_id: Option<String>,
    transaction_date: Option<String>,
    transaction_amount: Option<Decimal>,
    transaction_description: Option<String>,
    date_diff_days: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfxConsistencyDbOnly {
    transaction_id: String,
    transaction_date: String,
    amount: Decimal,
    description: String,
    account_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OfxConsistencyOutput {
    ofx_path: String,
    account_filter: Option<String>,
    range_start: String,
    range_end: String,
    date_tolerance_days: i64,
    ofx_transactions: usize,
    db_candidates: usize,
    matched: usize,
    missing_in_finance: usize,
    extra_in_finance: usize,
    consistent: bool,
    matches: Vec<OfxConsistencyMatch>,
    extra_transactions: Vec<OfxConsistencyDbOnly>,
}

#[derive(Subcommand)]
enum TxCommand {
    UpsertManual(ManualTransactionArgs),
    Categorize(CategorizeTransactionArgs),
    SetAnatomy(SetAnatomyArgs),
    SetContext(SetContextArgs),
    ListContext(ListContextArgs),
    Find(TxFindArgs),
    Pending(TxPendingArgs),
    PendingHuman(PendingHumanArgs),
    ReviewHuman(ReviewHumanArgs),
    /// Merge a duplicate manual transaction into its canonical Pluggy row.
    Merge(MergeTransactionsArgs),
    SetContextByDesc(SetContextByDescArgs),
    Split {
        #[command(subcommand)]
        command: TxSplitCommand,
    },
    /// Run the LLM-driven enrichment pipeline over uncategorized
    /// transactions. Supports human + machine (NDJSON) modes.
    Enrich(enrich::EnrichArgs),
    /// Propagate human-curated description and purpose from prior
    /// same-merchant transactions to those that are still missing them.
    ReplicateAnatomy(ReplicateAnatomyArgs),
}

#[derive(Subcommand)]
enum TxSplitCommand {
    Preview(TxSplitPreviewArgs),
    Apply(TxSplitApplyArgs),
    Show(TxSplitShowArgs),
    Clear(TxSplitClearArgs),
}

#[derive(Args)]
struct ManualTransactionArgs {
    #[arg(long)]
    transaction_id: Option<String>,
    #[arg(long)]
    account_id: Option<String>,
    #[arg(long)]
    date: String,
    #[arg(long)]
    description: String,
    #[arg(long)]
    amount: String,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    subcategory: Option<String>,
    #[arg(long)]
    context: Option<String>,
    #[arg(long, default_value = "confirmed")]
    payment_status: String,
}

#[derive(Args)]
struct CategorizeTransactionArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    category: String,
    #[arg(long)]
    subcategory: Option<String>,
    #[arg(long)]
    context: Option<String>,
}

#[derive(Args)]
struct MergeTransactionsArgs {
    /// Duplicate transaction to remove after copying human fields.
    #[arg(long)]
    from: String,
    /// Canonical transaction to keep, usually the Pluggy row.
    #[arg(long = "into")]
    into_transaction_id: String,
    /// Maximum allowed absolute amount difference.
    #[arg(long, default_value = "1.00")]
    max_amount_diff: String,
    /// Maximum allowed date distance between the two rows.
    #[arg(long, default_value_t = 7)]
    max_days: i64,
    /// Allow deleting a duplicate whose source is not manual/legacy.
    #[arg(long)]
    allow_any_source: bool,
    /// Allow keeping a canonical row whose source is not pluggy.
    #[arg(long)]
    allow_non_pluggy_target: bool,
    /// Validate and print the merge plan without writing.
    #[arg(long)]
    dry_run: bool,
}

pub(crate) fn category_key_from_input(category: &str, subcategory: Option<&str>) -> String {
    match subcategory {
        Some(value) => category_id(category, Some(value)),
        None => match category.split_once(':') {
            Some((base, sub)) => category_id(base, Some(sub)),
            None => category_id(category, None),
        },
    }
}

#[derive(Args)]
struct SetContextArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    context: String,
}

#[derive(Args)]
struct SetAnatomyArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    description: Option<String>,
    #[arg(long)]
    merchant_name: Option<String>,
    #[arg(long)]
    purpose: Option<String>,
    #[arg(long)]
    classifier_trace: Option<String>,
}

#[derive(Args)]
struct ReplicateAnatomyArgs {
    /// Max transactions to process (default: 200).
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Show what would be replicated without writing any changes.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum PendingHumanKind {
    Description,
    Merchant,
    Purpose,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum ReviewHumanKind {
    All,
    Description,
    Merchant,
    Purpose,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewFilters {
    pub(crate) month: Option<String>,
    pub(crate) account_id: Option<String>,
    /// Set of account_ids belonging to a specific owner. Populated when
    /// `--owner` is supplied (resolved against `accounts.owner` at startup).
    /// When `Some`, a row matches only if its `account_id` is in this set;
    /// `account_id` (singular) is still ANDed on top for finer scoping.
    pub(crate) owner_accounts: Option<BTreeSet<String>>,
    /// Original owner name (for display/summary purposes).
    pub(crate) owner: Option<String>,
    pub(crate) merchant: Option<String>,
    pub(crate) category: Option<String>,
}

impl ReviewFilters {
    fn from_review_args(args: &ReviewHumanArgs) -> Self {
        let category = if args.transaction_id.is_some() {
            args.filter_category.as_deref()
        } else {
            args.filter_category.as_deref().or(args.category.as_deref())
        };
        Self {
            month: args.month.clone(),
            account_id: args.account_id.clone(),
            owner_accounts: None,
            owner: args.owner.clone(),
            merchant: args.merchant.clone(),
            category: category.map(|value| category_key_from_input(value, None)),
        }
    }

    fn is_empty(&self) -> bool {
        self.month.is_none()
            && self.account_id.is_none()
            && self.owner_accounts.is_none()
            && self.owner.is_none()
            && self.merchant.is_none()
            && self.category.is_none()
    }

    fn matches(&self, row: &TransactionRecord) -> bool {
        self.matches_month(row)
            && self.matches_account(row)
            && self.matches_category(row)
            && self.matches_merchant(row)
    }

    fn matches_month(&self, row: &TransactionRecord) -> bool {
        self.month
            .as_deref()
            .is_none_or(|month| row.transaction_date.format("%Y-%m").to_string() == month)
    }

    fn matches_account(&self, row: &TransactionRecord) -> bool {
        // 1. If --owner was provided, the row's account must belong to that
        //    owner's account set. An unbound account_id never matches.
        if let Some(allowed) = &self.owner_accounts {
            match row.account_id.as_deref() {
                Some(account) if allowed.contains(account) => {}
                _ => return false,
            }
        }
        // 2. --account-id is still ANDed on top — useful for narrowing
        //    even further (e.g. owner=aline + account_id=aline_cartao).
        self.account_id
            .as_deref()
            .is_none_or(|account| row.account_id.as_deref() == Some(account))
    }

    fn matches_category(&self, row: &TransactionRecord) -> bool {
        self.category
            .as_deref()
            .is_none_or(|category| row.category_id.as_deref() == Some(category))
    }

    fn matches_merchant(&self, row: &TransactionRecord) -> bool {
        let Some(needle) = self.merchant.as_deref().map(normalize_filter_text) else {
            return true;
        };
        if needle.is_empty() {
            return true;
        }
        review_filter_merchant_haystack(row).contains(&needle)
    }
}

fn normalize_filter_text(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn review_filter_merchant_haystack(row: &TransactionRecord) -> String {
    [
        row.merchant_name.as_deref(),
        Some(row.raw_description.as_str()),
        row.description.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(normalize_filter_text)
    .collect::<Vec<_>>()
    .join(" ")
}

#[derive(Args)]
struct ReviewHumanArgs {
    /// Queue to review when running interactively or listing with --json.
    #[arg(long, value_enum, default_value_t = ReviewHumanKind::All)]
    kind: ReviewHumanKind,
    /// Maximum number of transactions to load.
    #[arg(long)]
    limit: Option<usize>,
    /// Minimum absolute amount for purpose-review candidates.
    #[arg(long, default_value = "30")]
    min_abs_amount: String,
    /// Emit machine-readable JSON queue/result for OpenClaw.
    #[arg(long)]
    json: bool,
    /// Show only counts and a phone-friendly invitation to review.
    #[arg(long)]
    summary: bool,
    /// Filter queue to a month in YYYY-MM.
    #[arg(long)]
    month: Option<String>,
    /// Filter queue to a single account_id.
    #[arg(long)]
    account_id: Option<String>,
    /// Filter queue to every account owned by `<name>` (resolved against
    /// the `accounts.owner` column at runtime). Useful for assistants
    /// that should only see one person's transactions — e.g. Aline's
    /// OpenClaw passes `--owner aline`. Combines with `--account-id`.
    #[arg(long)]
    owner: Option<String>,
    /// Filter queue by merchant/raw description text.
    #[arg(long)]
    merchant: Option<String>,
    /// Explicit category filter for queue mode. In queue mode, --category also filters.
    #[arg(long)]
    filter_category: Option<String>,
    /// Apply a single review non-interactively.
    #[arg(long)]
    transaction_id: Option<String>,
    /// Human short description to save.
    #[arg(long)]
    description: Option<String>,
    /// Clean merchant/establishment name to save.
    #[arg(long)]
    merchant_name: Option<String>,
    /// Human purchase purpose to save.
    #[arg(long)]
    purpose: Option<String>,
    /// New category. Accepts either "categoria" or "categoria:subcategoria".
    #[arg(long)]
    category: Option<String>,
    /// Optional subcategory when --category is provided as a top-level category.
    #[arg(long)]
    subcategory: Option<String>,
}

#[derive(Args)]
struct PendingHumanArgs {
    #[arg(long, value_enum, default_value_t = PendingHumanKind::Description)]
    kind: PendingHumanKind,
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long, default_value = "30")]
    min_abs_amount: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ListContextArgs {
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TxFindArgs {
    #[arg(long)]
    query: String,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TxPendingArgs {
    #[arg(long, default_value_t = 10)]
    limit: usize,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct SetContextByDescArgs {
    #[arg(long)]
    query: String,
    #[arg(long)]
    context: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TxSplitPreviewArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    payload: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TxSplitApplyArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    payload: PathBuf,
}

#[derive(Args)]
struct TxSplitShowArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct TxSplitClearArgs {
    #[arg(long)]
    transaction_id: String,
    #[arg(long)]
    reason: Option<String>,
}

#[derive(Subcommand)]
enum ForecastCommand {
    Upsert(Box<ForecastUpsertArgs>),
    #[command(
        name = "refresh-installments",
        about = "detecta cadeias de parcelamento ativas e gera os forecasts restantes",
        long_about = "Detecta cadeias de parcelamento (X/N) na janela de --lookback-months \
                      via phai-core::installments, agrupa por (account, descrição base, total), \
                      e materializa: 1 forecast_template por cadeia + N forecasts (um por parcela \
                      restante), ambos com idempotency keys estáveis para que execuções repetidas \
                      sejam no-ops. Camada 1 do pipeline da ADR-0016."
    )]
    RefreshInstallments(ForecastRefreshInstallmentsArgs),
    #[command(
        about = "orquestrador completo: parcelamentos + reconciliação + materializa próximos meses + detecta novos recorrentes",
        long_about = "Roda o pipeline completo do ADR-0016 em uma passada: \
                      (1) `refresh-installments` (Layer 1), \
                      (2) reconciliação dos forecasts ativos com as transações reais já presentes no banco, \
                      (3) materialização de N meses de forecasts para cada template ativo (subscription / fixed / envelope), \
                      (4) detecção silenciosa de novos candidatos recorrentes (Layer 2/3/4) — \
                      eles ficam em status `proposto` aguardando `fin forecast suggest` ou `accept`. \
                      Use `--skip-suggest` para pular a detecção e fazer apenas a manutenção."
    )]
    Refresh(ForecastRefreshArgs),
    #[command(
        about = "reconcilia forecasts ativos com as transações reais já presentes no banco",
        long_about = "Para cada forecast em status='ativo' com due_date nos últimos N dias, \
                      procura uma transação na mesma conta dentro de ±3 dias da due_date e com \
                      o valor dentro de ±5%. Se houver match único, marca o forecast como \
                      `realizado` e popula `realized_transaction_id` + `realized_at`. \
                      Matches ambíguos (mais de um candidato) são pulados — o usuário pode \
                      resolver manualmente via `fin forecast upsert`."
    )]
    Reconcile(ForecastReconcileArgs),
    #[command(
        about = "lista candidatos a forecasts recorrentes (subscriptions + fixed bills + envelopes) detectados no histórico",
        long_about = "Roda o detector de recorrentes (Camadas 2, 3 e 4 do ADR-0016): para cada \
                      par (conta, merchant), exige ≥3 meses de ocorrências, cadência mensal e \
                      coeficiente de variação ≤ 10% (subscription) ou ≤ 30% (fixed). Também \
                      detecta envelopes por categoria (Camada 4): ≥4 meses na categoria, \
                      variação ≤ 40%, excluindo merchants já cobertos por subscriptions/fixed \
                      ativos. Cada candidato novo é persistido como forecast_template com \
                      status='proposto' para que execuções futuras não o re-sugiram. Use \
                      `fin forecast accept` ou `fin forecast dismiss` para resolver."
    )]
    Suggest(ForecastSuggestArgs),
    #[command(
        about = "aceita um template em status 'proposto' e materializa seus forecasts",
        long_about = "Marca o forecast_template como 'ativo' e gera N forecasts futuros \
                      (default 6 meses) ancorados no `next_due_day` do template. Idempotente: \
                      re-executar não duplica forecasts."
    )]
    Accept(ForecastAcceptArgs),
    #[command(
        about = "descarta um template em status 'proposto' para que o detector não o re-sugira",
        long_about = "Marca o forecast_template como 'descartado'. Em scans futuros do \
                      detector, candidatos com o mesmo template_id (mesma conta+merchant+kind) \
                      são pulados — útil quando o detector pega um falso positivo."
    )]
    Dismiss(ForecastDismissArgs),
    #[command(
        about = "simula o impacto de um compromisso recorrente hipotético sobre o saldo projetado",
        long_about = "Pergunta what-if read-only: sem escrever no banco, calcula a projeção \
                      de saldo (cashflow-chart --forecast) considerando um compromisso recorrente \
                      adicional. Retorna o saldo projetado com e sem o cenário, o delta, e — quando \
                      --minimum-balance é passado — o primeiro mês em que o saldo cairia abaixo \
                      do limite. Usado pelo agente para responder 'posso afford esse gasto?'. \
                      Para cenários nomeados persistidos (editar/comparar/promover), veja \
                      `phai scenario` (ADR-0037)."
    )]
    Scenario(ForecastScenarioArgs),
}

#[derive(Args)]
pub(crate) struct ForecastRefreshInstallmentsArgs {
    /// Quantos meses olhar para trás ao detectar cadeias. Default: 12.
    #[arg(long, default_value_t = 12)]
    pub lookback_months: u32,
    /// Emite o resumo como JSON em vez do formato humano.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
pub(crate) struct ForecastRefreshArgs {
    /// Janela em meses usada por todas as camadas: detecção de cadeias de
    /// parcelamento e detecção de novos recorrentes. Default: 12.
    #[arg(long, default_value_t = 12)]
    pub lookback_months: u32,
    /// Quantos meses materializar à frente para cada template ativo
    /// (subscriptions, fixed bills, envelopes). Default: 6.
    #[arg(long, default_value_t = 6)]
    pub materialize_months: u32,
    /// Pula a detecção de novos candidatos (Layer 2/3/4); só faz a
    /// manutenção dos templates existentes + reconciliação.
    #[arg(long)]
    pub skip_suggest: bool,
    /// Emite o resumo como JSON em vez do formato humano.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
pub(crate) struct ForecastReconcileArgs {
    /// Quantos dias para trás varrer atrás de forecasts ativos com due_date
    /// no passado. Default: 45.
    #[arg(long, default_value_t = 45)]
    pub lookback_days: i64,
    /// Emite o resumo como JSON em vez do formato humano.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
pub(crate) struct ForecastSuggestArgs {
    /// Janela em meses para análise. Mín: 3, default: 6.
    #[arg(long, default_value_t = 6)]
    pub lookback_months: u32,
    /// Emite o resumo como JSON em vez do formato humano.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
pub(crate) struct ForecastAcceptArgs {
    /// ID do template em status 'proposto' a ser aceito.
    #[arg(long)]
    pub template_id: String,
    /// Quantos meses materializar imediatamente após aceitar. Default: 6.
    #[arg(long, default_value_t = 6)]
    pub materialize_months: u32,
}

#[derive(Args)]
pub(crate) struct ForecastDismissArgs {
    /// ID do template em status 'proposto' a ser descartado.
    #[arg(long)]
    pub template_id: String,
}

#[derive(Args)]
pub(crate) struct ForecastScenarioArgs {
    /// Valor mensal do compromisso (positivo = entrada extra, negativo =
    /// saída). Aceita formato livre `"250"` ou `"250.00"`.
    #[arg(long)]
    pub amount: String,
    /// Descrição livre do cenário, ex.: "atividade extracurricular".
    #[arg(long)]
    pub description: String,
    /// Mês de início no formato YYYY-MM. Default: próximo mês.
    #[arg(long)]
    pub start: Option<String>,
    /// Quantos meses o compromisso dura. Default: 12.
    #[arg(long, default_value_t = 12)]
    pub months: u32,
    /// Saldo mínimo aceitável; se passado, retorna o primeiro mês em que
    /// o saldo projetado cairia abaixo deste valor.
    #[arg(long)]
    pub minimum_balance: Option<String>,
    /// Janela "para a frente" para projetar (default 12 meses incluindo o atual).
    #[arg(long, default_value_t = 12)]
    pub project_months: u32,
    /// Emite o resultado como JSON.
    #[arg(long)]
    pub raw: bool,
}

#[derive(Args)]
struct ForecastUpsertArgs {
    #[arg(long)]
    forecast_id: Option<String>,
    #[arg(long)]
    date: Option<String>,
    #[arg(long)]
    description: String,
    #[arg(long)]
    amount: String,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    subcategory: Option<String>,
    #[arg(long)]
    account_id: Option<String>,
    #[arg(long, default_value = "active")]
    status: String,
    #[arg(long)]
    recurrence: Option<String>,
}

#[derive(Subcommand)]
enum RuleCommand {
    Upsert(RuleUpsertArgs),
    List(RuleListArgs),
    Inspect(RuleInspectArgs),
}

#[derive(Args)]
struct RuleUpsertArgs {
    #[arg(long)]
    rule_id: String,
    #[arg(long)]
    body: String,
    #[arg(long, default_value = "active")]
    status: String,
}

#[derive(Args)]
struct RuleListArgs {
    #[arg(long, value_enum, default_value_t = RuleStatusFilter::Active)]
    status: RuleStatusFilter,
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum RuleStatusFilter {
    Active,
    Disabled,
    All,
}

impl RuleStatusFilter {
    fn as_str(self) -> &'static str {
        match self {
            RuleStatusFilter::Active => "active",
            RuleStatusFilter::Disabled => "disabled",
            RuleStatusFilter::All => "all",
        }
    }
}

#[derive(Args)]
struct RuleInspectArgs {
    #[arg(long)]
    rule_id: String,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum AccountCommand {
    Upsert(AccountUpsertArgs),
    #[command(
        about = "set a credit card's billing closing/due day (merges into metadata)",
        long_about = "Sets `billing_closing_day` and `billing_due_day` on an existing \
                      account's metadata without clobbering the rest (Pluggy creditData, \
                      balance, etc.). These drive the cash-flow `cash_month` projection — a \
                      card purchase lands in the month its bill is paid (ADR-0025). Required \
                      for the bill-explosion view; Pluggy does not populate them reliably."
    )]
    SetBillingCycle(AccountBillingCycleArgs),
}

#[derive(Args)]
struct AccountBillingCycleArgs {
    /// Account id to update (e.g. the credit-card account).
    #[arg(long)]
    account_id: String,
    /// Day of month the bill closes (1–28).
    #[arg(long)]
    closing_day: u32,
    /// Day of month the bill is due/paid (1–28).
    #[arg(long)]
    due_day: u32,
}

#[derive(Args)]
struct AccountUpsertArgs {
    #[arg(long)]
    account_id: String,
    #[arg(long)]
    owner: String,
    #[arg(long)]
    account_type: String,
    #[arg(long)]
    bank: String,
    #[arg(long)]
    label: String,
    #[arg(long)]
    pluggy_account_id: Option<String>,
    #[arg(long)]
    pluggy_item_id: Option<String>,
    #[arg(long, default_value = "active")]
    status: String,
}

#[derive(Subcommand)]
enum BudgetCommand {
    Upsert(BudgetUpsertArgs),
    List(BudgetListArgs),
}

#[derive(Args)]
struct BudgetUpsertArgs {
    #[arg(long)]
    category_id: String,
    #[arg(long)]
    subcategory_id: Option<String>,
    #[arg(long)]
    month: Option<String>,
    #[arg(long)]
    amount: String,
    #[arg(long, default_value_t = 80)]
    alert_threshold_pct: i64,
}

#[derive(Args)]
struct BudgetListArgs {
    #[arg(long)]
    month: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct BudgetStatusArgs {
    /// Month to report on, in YYYY-MM format (required, e.g. 2024-11).
    #[arg(long)]
    month: String,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

impl BudgetStatusArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Args)]
struct CardsArgs {
    /// Target month in YYYY-MM. Defaults to the most recent closed bill
    /// (same as `--closed`).
    #[arg(long, conflicts_with_all = ["closed", "next"])]
    month: Option<String>,
    /// Show each card's most recent closed bill. This is the default when no
    /// month is given. Mutually exclusive with --month and --next.
    #[arg(long, conflicts_with = "next")]
    closed: bool,
    /// Show the partial state of the currently open cycle (what's been
    /// charged so far this month — payment status is not inferred since
    /// the bill hasn't closed). Mutually exclusive with --closed and --month.
    #[arg(long)]
    next: bool,
    /// Limit the report to a single credit-card account.
    #[arg(long)]
    account_id: Option<String>,
    /// List every transaction in each category instead of truncating to the
    /// top 5 by absolute amount.
    #[arg(long)]
    all: bool,
    /// Show only installment (parcelada) transactions — hides one-off and
    /// subscription charges so you can focus on multi-month commitments.
    #[arg(long)]
    installments_only: bool,
    /// Emit machine-readable JSON instead of the WhatsApp-friendly summary.
    #[arg(long)]
    raw: bool,
    /// Deprecated alias for `--raw`.
    #[arg(long, hide = true)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardsMode {
    Closed,
    Specific,
    Next,
}

impl CardsArgs {
    fn structured_output(&self) -> bool {
        self.raw || self.json
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum BackendArg {
    Local,
    Bigquery,
}

impl From<BackendArg> for BackendKind {
    fn from(value: BackendArg) -> Self {
        match value {
            BackendArg::Local => BackendKind::Local,
            BackendArg::Bigquery => BackendKind::Bigquery,
        }
    }
}

fn brl(value: Decimal) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "+" };
    let rounded = format!("{:.2}", value.abs().round_dp(2)).replace('.', ",");
    format!("{sign}R$ {rounded}")
}

/// Formats a Decimal as BRL with sign and thousands separator: `-R$ 1.500,00`.
fn brl_signed(value: Decimal) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "+" };
    let abs_val = value.abs().round_dp(2);
    let formatted = format!("{:.2}", abs_val).replace('.', ",");
    // Insert thousands separators (period) before the comma
    let (integer_part, decimal_part) = formatted.split_once(',').unwrap_or((&formatted, "00"));
    let with_thousands = insert_thousands(integer_part);
    format!("{sign}R$ {with_thousands},{decimal_part}")
}

/// Formats a Decimal as unsigned BRL with thousands separator: `R$ 1.500,00`.
fn brl_abs(value: Decimal) -> String {
    let abs_val = value.abs().round_dp(2);
    let formatted = format!("{:.2}", abs_val).replace('.', ",");
    let (integer_part, decimal_part) = formatted.split_once(',').unwrap_or((&formatted, "00"));
    let with_thousands = insert_thousands(integer_part);
    format!("R$ {with_thousands},{decimal_part}")
}

fn insert_thousands(s: &str) -> String {
    let digits: Vec<char> = s.chars().collect();
    let n = digits.len();
    let mut result = String::with_capacity(n + n / 3);
    for (i, ch) in digits.iter().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            result.push('.');
        }
        result.push(*ch);
    }
    result
}

/// Formats a NaiveDate as short Portuguese date: `14/mai/2026`.
fn short_date_pt(date: NaiveDate) -> String {
    let month_abbr = match date.month() {
        1 => "jan",
        2 => "fev",
        3 => "mar",
        4 => "abr",
        5 => "mai",
        6 => "jun",
        7 => "jul",
        8 => "ago",
        9 => "set",
        10 => "out",
        11 => "nov",
        12 => "dez",
        _ => "???",
    };
    format!("{:02}/{}/{}", date.day(), month_abbr, date.year())
}

fn decimal_text(value: Decimal) -> String {
    format!("{:.2}", value.round_dp(2))
}

fn day_of_week_text(date: NaiveDate) -> String {
    date.format("%A").to_string().to_ascii_lowercase()
}

fn normalize_inline_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render the sync notify summary as a phone-readable WhatsApp message.
/// Delegates to the structured `sync_notify::render_sync_message` formatter.
fn render_sync_notify_summary(
    summary: &SyncSummaryOutput,
    accounts: &[AccountRecord],
    snapshots: &[phai_core::models::AccountSnapshotRecord],
) -> String {
    // Compute total checking balance from snapshots.
    let checking_ids: BTreeSet<String> = accounts
        .iter()
        .filter(|a| is_checking_account_type(&a.account_type) && !a.account_id.is_empty())
        .map(|a| a.account_id.clone())
        .collect();
    let balance: Option<Decimal> = {
        let total: Decimal = snapshots
            .iter()
            .filter(|s| checking_ids.contains(&s.account_id) && s.balance.is_some())
            .filter_map(|s| s.balance)
            .sum();
        if total == Decimal::ZERO && snapshots.iter().all(|s| s.balance.is_none()) {
            None
        } else {
            Some(total)
        }
    };

    // Convert transactions to the formatter's type.
    let txs: Vec<sync_notify::SyncSummaryTransaction> = summary
        .new_transactions
        .iter()
        .map(|tx| sync_notify::SyncSummaryTransaction {
            transaction_id: tx.transaction_id.clone(),
            transaction_date: tx.transaction_date.clone(),
            description: tx.description.clone(),
            amount: tx.amount.clone(),
            tx_type: tx.tx_type.clone(),
            category_id: tx.category_id.clone(),
            category_source: tx.category_source.clone(),
            context: tx.context.clone(),
            account_id: tx.account_id.clone(),
            account_label: tx.account_label.clone(),
            payment_status: tx.payment_status.clone(),
            source: tx.source.clone(),
            metadata_json: tx.metadata_json.clone(),
        })
        .collect();

    let review_items = sync_notify::build_review_items(&txs);

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string());

    let input = sync_notify::SyncMessageInput {
        new_transactions: txs,
        accounts: accounts.to_vec(),
        snapshots: snapshots.to_vec(),
        review_items,
        balance,
        sync_time: Utc::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        hostname,
    };

    sync_notify::render_sync_message(&input)
}

/// Copy billing metadata keys from an existing account record into a freshly
/// built Pluggy account so manual overrides survive re-syncs.
fn keep_stored_account_identity(account: &mut AccountRecord, stored: &AccountRecord) {
    account.owner = stored.owner.clone();
    account.account_type = stored.account_type.clone();
    account.bank = stored.bank.clone();
    account.label = stored.label.clone();
}

fn merge_billing_metadata(new_meta: &mut serde_json::Value, existing_meta: &serde_json::Value) {
    for key in ["billing_closing_day", "billing_due_day"] {
        if new_meta.get(key).is_none() {
            if let Some(v) = existing_meta.get(key) {
                new_meta[key] = v.clone();
            }
        }
    }
}

pub(crate) fn parse_month_ref(value: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(&format!("{value}-01"), "%Y-%m-%d")
        .with_context(|| format!("month inválido: {value} (esperado YYYY-MM)"))
}

pub(crate) fn month_ref_for(date: NaiveDate) -> String {
    date.format("%Y-%m").to_string()
}

pub(crate) fn shift_month(date: NaiveDate, delta: i32) -> Result<NaiveDate> {
    let mut year = date.year();
    let mut month = date.month() as i32 + delta;
    while month > 12 {
        year += 1;
        month -= 12;
    }
    while month < 1 {
        year -= 1;
        month += 12;
    }
    NaiveDate::from_ymd_opt(year, month as u32, 1)
        .with_context(|| format!("Falha ao calcular mês deslocado a partir de {}", date))
}

fn default_scenario_month(today: NaiveDate) -> Result<String> {
    Ok(month_ref_for(shift_month(
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
            .context("Falha ao calcular mês atual")?,
        1,
    )?))
}

fn default_closed_cards_month(today: NaiveDate) -> Result<String> {
    Ok(month_ref_for(shift_month(
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
            .context("Falha ao calcular mês atual")?,
        -1,
    )?))
}

/// Whether a `payment_status` should be summed into a card's "open bill"
/// total. ADR-0011 narrows this to the canonical `pending` value —
/// `installment` rows are future parcelas and surface separately via the
/// `installments_future` column.
///
/// The PT aliases (`em_aberto`, `parcial`) and the legacy `confirmed` /
/// `unconfirmed` aliases are still accepted as input so reports stay correct
/// against a database that hasn't yet been re-migrated (e.g. during a
/// rolling deploy where the row migration in 021 hasn't finished).
fn is_open_card_payment_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "pending" | "em_aberto" | "unconfirmed"
    )
}

fn is_flat_category(category_id: &str) -> bool {
    !category_id.contains(':')
}

pub(crate) fn normalize_description(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut prev_space = false;
    for ch in value.chars().flat_map(char::to_lowercase) {
        let mapped = if ch.is_ascii_alphanumeric() { ch } else { ' ' };
        if mapped == ' ' {
            if !prev_space {
                normalized.push(mapped);
            }
            prev_space = true;
        } else {
            normalized.push(mapped);
            prev_space = false;
        }
    }
    normalized.trim().to_string()
}

fn compact_token_ascii(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>()
}

fn extract_installment_marker_from_tokens(tokens: &[&str], idx: usize) -> Option<String> {
    let token = tokens.get(idx)?;
    let cleaned = token.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '/');
    let mut parts = cleaned.split('/');
    if let (Some(left), Some(right), None) = (parts.next(), parts.next(), parts.next()) {
        if !left.is_empty()
            && !right.is_empty()
            && left.chars().all(|ch| ch.is_ascii_digit())
            && right.chars().all(|ch| ch.is_ascii_digit())
        {
            return Some(format!("{left}/{right}"));
        }
    }

    let compact = compact_token_ascii(token);
    if let Some(number) = compact
        .strip_prefix("parcela")
        .or_else(|| compact.strip_prefix("parc"))
    {
        if !number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit()) {
            return Some(format!("parcela-{number}"));
        }
        if number.is_empty() {
            let next = tokens.get(idx + 1)?;
            let next_clean = next.trim_matches(|ch: char| !ch.is_ascii_digit());
            if !next_clean.is_empty() && next_clean.chars().all(|ch| ch.is_ascii_digit()) {
                return Some(format!("parcela-{next_clean}"));
            }
        }
    }
    None
}

fn extract_installment_marker(value: &str) -> Option<String> {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    for idx in 0..tokens.len() {
        if let Some(marker) = extract_installment_marker_from_tokens(&tokens, idx) {
            return Some(marker);
        }
    }
    None
}

pub(crate) fn strip_installment_marker(value: &str) -> String {
    let tokens = value.split_whitespace().collect::<Vec<_>>();
    let mut kept = Vec::new();
    let mut idx = 0usize;
    while idx < tokens.len() {
        if extract_installment_marker_from_tokens(&tokens, idx).is_some() {
            let compact = compact_token_ascii(tokens[idx]);
            if (compact == "parcela" || compact == "parc") && idx + 1 < tokens.len() {
                idx += 2;
            } else {
                idx += 1;
            }
            continue;
        }
        kept.push(tokens[idx]);
        idx += 1;
    }
    kept.join(" ")
}

fn metadata_contains_installment_signal(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| {
            let key_lc = key.to_ascii_lowercase();
            key_lc.contains("installment")
                || key_lc.contains("parcela")
                || metadata_contains_installment_signal(child)
        }),
        Value::Array(items) => items.iter().any(metadata_contains_installment_signal),
        Value::String(text) => {
            let text_lc = text.to_ascii_lowercase();
            extract_installment_marker(text).is_some()
                || text_lc.contains("parc")
                || text_lc.contains("parcela")
        }
        _ => false,
    }
}

fn merchant_key_for_card_row(row: &CardClosedTransactionRow) -> String {
    let base = if row.label.trim().is_empty() {
        row.description.as_str()
    } else {
        row.label.as_str()
    };
    let normalized = normalize_description(&strip_installment_marker(base));
    if normalized.is_empty() {
        "sem-chave".to_string()
    } else {
        normalized
    }
}

fn detect_installment_marker(row: &CardClosedTransactionRow) -> Option<String> {
    extract_installment_marker(&row.label)
        .or_else(|| extract_installment_marker(&row.description))
        .or_else(|| {
            // Also try descriptionRaw stored by Pluggy sync in metadata.
            row.metadata_json
                .get("raw")
                .and_then(|r| r.get("descriptionRaw"))
                .and_then(|v| v.as_str())
                .and_then(extract_installment_marker)
        })
        .or_else(|| {
            metadata_contains_installment_signal(&row.metadata_json).then(|| "metadata".to_string())
        })
}

/// Returns the description enriched with an installment marker sourced from
/// Pluggy metadata when the stored description text lacks one. Used to make
/// `report installments` work with data synced before the Pluggy fix landed.
pub(crate) fn enrich_description_from_metadata(description: &str, metadata: &Value) -> String {
    if phai_core::installments::parse_installment_description(description).is_some() {
        return description.to_string();
    }
    // Try descriptionRaw first.
    if let Some(raw) = metadata
        .get("raw")
        .and_then(|r| r.get("descriptionRaw"))
        .and_then(|v| v.as_str())
        .filter(|s| phai_core::installments::parse_installment_description(s).is_some())
    {
        return raw.to_string();
    }
    // Fall back to creditCardMetadata structured fields.
    if let (Some(current), Some(total)) = (
        metadata
            .get("raw")
            .and_then(|r| r.get("creditCardMetadata"))
            .and_then(|m| m.get("installmentNumber"))
            .and_then(|v| v.as_u64()),
        metadata
            .get("raw")
            .and_then(|r| r.get("creditCardMetadata"))
            .and_then(|m| m.get("totalInstallments"))
            .and_then(|v| v.as_u64()),
    ) {
        if current > 0 && total > 0 && current <= total && total <= 99 {
            return format!("{} {}/{}", description.trim(), current, total);
        }
    }
    description.to_string()
}

fn is_subscription_row(row: &CardClosedTransactionRow) -> bool {
    if row
        .category_id
        .as_deref()
        .is_some_and(|category| category.starts_with("assinaturas"))
    {
        return true;
    }
    let label = format!("{} {}", row.label, row.description).to_ascii_lowercase();
    ["assinatura", "subscription", "mensalidade", "anuidade"]
        .iter()
        .any(|needle| label.contains(needle))
}

fn descriptions_look_related(left: &str, right: &str) -> bool {
    let left_norm = normalize_description(left);
    let right_norm = normalize_description(right);
    if left_norm.is_empty() || right_norm.is_empty() {
        return false;
    }
    if left_norm == right_norm || left_norm.contains(&right_norm) || right_norm.contains(&left_norm)
    {
        return true;
    }
    if extract_installment_marker(left) == extract_installment_marker(right)
        && extract_installment_marker(left).is_some()
    {
        return true;
    }
    let left_tokens = left_norm
        .split_whitespace()
        .filter(|token| token.len() >= 4)
        .collect::<BTreeSet<_>>();
    let right_tokens = right_norm
        .split_whitespace()
        .filter(|token| token.len() >= 4)
        .collect::<BTreeSet<_>>();
    left_tokens.intersection(&right_tokens).count() >= 2
}

fn extract_ofx_tag_value_from_line(line: &str, tag: &str) -> Option<String> {
    let trimmed = line.trim();
    let open_tag = format!("<{tag}>");
    let prefix = trimmed.get(..open_tag.len())?;
    if !prefix.eq_ignore_ascii_case(&open_tag) {
        return None;
    }
    let tail = trimmed.get(open_tag.len()..)?.trim();
    let value = tail.split('<').next().unwrap_or_default().trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_ofx_tag_values(raw: &str, tag: &str) -> Vec<String> {
    let mut values = Vec::new();
    for line in raw.lines() {
        if let Some(value) = extract_ofx_tag_value_from_line(line, tag) {
            values.push(value);
        }
    }
    values
}

fn parse_ofx_date(value: &str) -> Result<NaiveDate> {
    let digits = value
        .chars()
        .filter(char::is_ascii_digit)
        .take(8)
        .collect::<String>();
    if digits.len() != 8 {
        bail!("Data OFX inválida: {value}");
    }
    NaiveDate::parse_from_str(&digits, "%Y%m%d")
        .with_context(|| format!("Falha ao parsear data OFX {value}"))
}

fn parse_ofx_amount(value: &str) -> Result<Decimal> {
    let normalized = value.trim().replace(',', ".");
    decimal_from_str(&normalized).with_context(|| format!("Valor OFX inválido: {value}"))
}

fn parse_ofx_transactions(raw: &str) -> Result<Vec<OfxTransaction>> {
    let mut blocks = Vec::<String>::new();
    let mut current_block = Vec::<String>::new();
    let mut in_statement = false;

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("<STMTTRN>") {
            if in_statement && !current_block.is_empty() {
                blocks.push(current_block.join("\n"));
                current_block.clear();
            }
            in_statement = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("</STMTTRN>") {
            if in_statement {
                blocks.push(current_block.join("\n"));
                current_block.clear();
                in_statement = false;
            }
            continue;
        }
        if in_statement {
            current_block.push(trimmed.to_string());
        }
    }

    if in_statement && !current_block.is_empty() {
        blocks.push(current_block.join("\n"));
    }

    let mut rows = Vec::with_capacity(blocks.len());
    for block in blocks {
        let date_raw = parse_ofx_tag_values(&block, "DTPOSTED")
            .into_iter()
            .next()
            .context("Transação OFX sem DTPOSTED")?;
        let amount_raw = parse_ofx_tag_values(&block, "TRNAMT")
            .into_iter()
            .next()
            .context("Transação OFX sem TRNAMT")?;
        let fit_id = parse_ofx_tag_values(&block, "FITID").into_iter().next();
        let description = parse_ofx_tag_values(&block, "MEMO")
            .into_iter()
            .next()
            .or_else(|| parse_ofx_tag_values(&block, "NAME").into_iter().next())
            .or_else(|| parse_ofx_tag_values(&block, "PAYEE").into_iter().next())
            .unwrap_or_else(|| "sem-descricao".to_string());
        rows.push(OfxTransaction {
            fit_id,
            transaction_date: parse_ofx_date(&date_raw)?,
            amount: parse_ofx_amount(&amount_raw)?,
            description: normalize_inline_text(&description),
        });
    }
    Ok(rows)
}

fn description_distance(left: &str, right: &str) -> usize {
    let left_norm = normalize_description(left);
    let right_norm = normalize_description(right);
    if left_norm.is_empty() || right_norm.is_empty() {
        return usize::MAX / 2;
    }
    if left_norm == right_norm {
        return 0;
    }
    if left_norm.contains(&right_norm) || right_norm.contains(&left_norm) {
        return 1;
    }
    let left_tokens = left_norm
        .split_whitespace()
        .filter(|token| token.len() >= 3)
        .collect::<BTreeSet<_>>();
    let right_tokens = right_norm
        .split_whitespace()
        .filter(|token| token.len() >= 3)
        .collect::<BTreeSet<_>>();
    let overlap = left_tokens.intersection(&right_tokens).count();
    if overlap >= 3 {
        2
    } else if overlap == 2 {
        3
    } else if overlap == 1 {
        6
    } else {
        10
    }
}

fn metadata_pluggy_account_id(row: &TransactionRecord) -> Option<&str> {
    row.metadata_json
        .get("pluggy_account_id")
        .and_then(Value::as_str)
        .or_else(|| {
            row.metadata_json
                .get("raw")
                .and_then(|raw| raw.get("accountId"))
                .and_then(Value::as_str)
        })
}

fn infer_account_id_from_fit_ids(
    ofx_rows: &[OfxTransaction],
    db_rows: &[TransactionRecord],
) -> Option<String> {
    let fit_ids = ofx_rows
        .iter()
        .filter_map(|row| row.fit_id.as_deref())
        .collect::<BTreeSet<_>>();
    if fit_ids.is_empty() {
        return None;
    }

    let mut counts = BTreeMap::<String, usize>::new();
    for row in db_rows {
        if !fit_ids.contains(row.transaction_id.as_str()) {
            continue;
        }
        if let Some(account_id) = row.account_id.as_deref() {
            *counts.entry(account_id.to_string()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|(left_id, left_count), (right_id, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| right_id.cmp(left_id))
        })
        .map(|(account_id, _)| account_id)
}

fn infer_account_id_from_overlap(
    ofx_rows: &[OfxTransaction],
    db_rows: &[TransactionRecord],
    date_tolerance_days: i64,
) -> Option<String> {
    let mut counts = BTreeMap::<String, usize>::new();
    for row in db_rows {
        let Some(account_id) = row.account_id.as_deref() else {
            continue;
        };
        let matches_any = ofx_rows.iter().any(|ofx_row| {
            let amount_diff = (row.amount - ofx_row.amount).abs();
            if amount_diff > Decimal::new(1, 2) {
                return false;
            }
            let date_diff = (row.transaction_date - ofx_row.transaction_date)
                .num_days()
                .abs();
            date_diff <= date_tolerance_days
        });
        if matches_any {
            *counts.entry(account_id.to_string()).or_insert(0) += 1;
        }
    }

    counts
        .into_iter()
        .max_by(|(left_id, left_count), (right_id, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| right_id.cmp(left_id))
        })
        .map(|(account_id, _)| account_id)
}

fn find_best_ofx_match(
    ofx_tx: &OfxTransaction,
    db_rows: &[TransactionRecord],
    used_rows: &[bool],
    date_tolerance_days: i64,
) -> Option<(usize, Decimal, i64, usize)> {
    let mut best: Option<(usize, Decimal, i64, usize)> = None;
    for (idx, candidate) in db_rows.iter().enumerate() {
        if used_rows.get(idx).copied().unwrap_or(false) {
            continue;
        }
        let amount_diff = (candidate.amount - ofx_tx.amount).abs();
        if amount_diff > Decimal::new(1, 2) {
            continue;
        }
        let date_diff = (candidate.transaction_date - ofx_tx.transaction_date)
            .num_days()
            .abs();
        if date_diff > date_tolerance_days {
            continue;
        }
        let desc_distance = description_distance(&ofx_tx.description, &candidate.raw_description);
        let replace_best = match best {
            None => true,
            Some((_, best_amount, best_date, best_desc)) => {
                amount_diff < best_amount
                    || (amount_diff == best_amount
                        && (date_diff < best_date
                            || (date_diff == best_date && desc_distance < best_desc)))
            }
        };
        if replace_best {
            best = Some((idx, amount_diff, date_diff, desc_distance));
        }
    }
    best
}

fn build_category_records_from_transactions(
    actor_id: &str,
    rows: &[TransactionRecord],
) -> Vec<CategoryRecord> {
    let mut categories = BTreeMap::<String, CategoryRecord>::new();
    let now = Utc::now();
    for row in rows {
        if let Some(category_id) = &row.category_id {
            let mut parts = category_id.splitn(2, ':');
            if let Some(parent) = parts.next() {
                categories
                    .entry(parent.to_string())
                    .or_insert_with(|| CategoryRecord {
                        category_id: parent.to_string(),
                        name: parent.replace('-', " "),
                        parent_category_id: None,
                        metadata_json: json!({"source": "pluggy_sync"}),
                        actor_id: actor_id.to_string(),
                        updated_at: now,
                    });
                if let Some(child) = parts.next() {
                    categories
                        .entry(category_id.clone())
                        .or_insert_with(|| CategoryRecord {
                            category_id: category_id.clone(),
                            name: child.replace('-', " "),
                            parent_category_id: Some(parent.to_string()),
                            metadata_json: json!({"source": "pluggy_sync"}),
                            actor_id: actor_id.to_string(),
                            updated_at: now,
                        });
                }
            }
        }
    }
    categories.into_values().collect()
}

async fn upsert_accounts_chunked(
    store: &dyn FinanceStore,
    rows: &[AccountRecord],
) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.upsert_accounts(chunk).await?;
    }
    Ok(total)
}

async fn upsert_transactions_chunked(
    store: &dyn FinanceStore,
    rows: &[TransactionRecord],
) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.upsert_transactions(chunk).await?;
    }
    Ok(total)
}

/// Fingerprint used to detect Pluggy emitting two distinct `transaction_id`s
/// for what is logically the same posted event.
fn dedup_fingerprint(row: &TransactionRecord) -> Result<String> {
    pluggy_transaction_fingerprint(
        row.account_id.as_deref(),
        row.transaction_date,
        row.amount,
        &row.raw_description,
    )
}

/// Filter out rows whose fingerprint collides with an existing transaction
/// (different `transaction_id`, both `source='pluggy'`). Returns the
/// filtered list and an audit event for each skipped row.
///
/// The check pulls existing rows for the batch's date range × accounts via
/// `transactions_in_date_range` and dedupes in-process. We don't go to the
/// store per row.
async fn dedup_pluggy_duplicates(
    store: &dyn FinanceStore,
    actor_id: &str,
    incoming: Vec<TransactionRecord>,
) -> Result<(Vec<TransactionRecord>, Vec<AuditEvent>)> {
    if incoming.is_empty() {
        return Ok((incoming, Vec::new()));
    }
    // Safe: `incoming.is_empty()` was checked above, so the iterator has
    // at least one element. Using a guarded fallback instead of `.unwrap()`
    // keeps us aligned with the project's no-unwrap-in-prod-paths rule.
    let (date_min, date_max) = incoming
        .iter()
        .map(|t| t.transaction_date)
        .fold(None::<(NaiveDate, NaiveDate)>, |acc, d| {
            Some(match acc {
                None => (d, d),
                Some((lo, hi)) => (lo.min(d), hi.max(d)),
            })
        })
        .context("incoming batch unexpectedly empty after non-empty check")?;
    // Existing rows in the window. Per-account filtering happens in-Rust so
    // we only need a single query.
    let existing = store
        .transactions_in_date_range(None, date_min, date_max)
        .await
        .unwrap_or_default();
    let mut fingerprint_to_existing: BTreeMap<String, String> = BTreeMap::new();
    for row in &existing {
        if row.source == "pluggy" {
            fingerprint_to_existing.insert(dedup_fingerprint(row)?, row.transaction_id.clone());
        }
    }
    let mut kept = Vec::with_capacity(incoming.len());
    let mut audit = Vec::new();
    for row in incoming {
        if row.source != "pluggy" {
            kept.push(row);
            continue;
        }
        let fp = dedup_fingerprint(&row)?;
        match fingerprint_to_existing.get(&fp) {
            Some(existing_id) if existing_id != &row.transaction_id => {
                let diff = json!({
                    "skipped_transaction_id": row.transaction_id,
                    "matched_existing_id": existing_id,
                    "fingerprint": fp,
                });
                audit.push(AuditEvent::from_entity(
                    "transaction",
                    &row.transaction_id,
                    "dedup_skipped",
                    actor_id,
                    &format!("dedup:{}", row.transaction_id),
                    diff,
                ));
                // No insert — leave the existing row alone.
            }
            _ => {
                fingerprint_to_existing.insert(fp, row.transaction_id.clone());
                kept.push(row);
            }
        }
    }
    Ok((kept, audit))
}

async fn upsert_rules_chunked(store: &dyn FinanceStore, rows: &[RuleRecord]) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.upsert_rules(chunk).await?;
    }
    Ok(total)
}

async fn upsert_categories_chunked(
    store: &dyn FinanceStore,
    rows: &[CategoryRecord],
) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.upsert_categories(chunk).await?;
    }
    Ok(total)
}

async fn upsert_forecasts_chunked(
    store: &dyn FinanceStore,
    rows: &[ForecastRecord],
) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.upsert_forecasts(chunk).await?;
    }
    Ok(total)
}

async fn insert_audit_chunked(store: &dyn FinanceStore, rows: &[AuditEvent]) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(AUDIT_BATCH_SIZE) {
        total += store.insert_audit_events(chunk).await?;
    }
    Ok(total)
}

async fn insert_snapshots_chunked(
    store: &dyn FinanceStore,
    rows: &[AccountSnapshotRecord],
) -> Result<usize> {
    let mut total = 0;
    for chunk in rows.chunks(UPSERT_BATCH_SIZE) {
        total += store.insert_account_snapshots(chunk).await?;
    }
    Ok(total)
}

pub(crate) async fn load_config() -> Result<(ConfigPaths, AppConfig)> {
    let paths = ConfigPaths::discover()?;
    paths.ensure()?;
    let config = AppConfig::load(&paths)?;
    Ok((paths, config))
}

fn ensure_bigquery_split_backend(config: &AppConfig) -> Result<()> {
    if !matches!(config.effective_backend(), BackendKind::Bigquery) {
        bail!("transaction split/detailing is supported only on the BigQuery backend");
    }
    Ok(())
}

async fn load_split_payload_preview(
    store: &dyn FinanceStore,
    transaction_id: &str,
    payload_path: &PathBuf,
) -> Result<(TransactionRecord, SplitPayload, SplitPreview)> {
    let parent = store
        .transaction_by_id(transaction_id)
        .await?
        .with_context(|| format!("Transação {transaction_id} não encontrada"))?;
    let raw = fs::read_to_string(payload_path)
        .with_context(|| format!("Falha ao ler payload {}", payload_path.display()))?;
    let payload = parse_split_payload(&raw)?;
    let preview = validate_split_payload(transaction_id, parent.amount, payload.clone())?;
    Ok((parent, payload, preview))
}

fn should_run_auto_check(cli: &Cli) -> bool {
    if cli.no_auto_update
        || phai_core::compat::env_var_os("PHAI_NO_AUTO_UPDATE", "FINANCE_OS_NO_AUTO_UPDATE")
            .is_some()
    {
        return false;
    }
    if let Some(updated_ver) = phai_core::compat::env_var_os("PHAI_UPDATED", "FINANCE_OS_UPDATED") {
        let updated_ver = updated_ver.to_string_lossy();
        if updated_ver != env!("CARGO_PKG_VERSION") {
            eprintln!(
                "warning: re-exec sentinel mismatch — old binary still running? \
                 (sentinel={updated_ver}, binary={})",
                env!("CARGO_PKG_VERSION")
            );
        }
        return false;
    }
    if matches!(cli.command, Some(Commands::SelfCmd { .. })) {
        return false;
    }
    true
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if cli.version {
        print!("{VERSION_BANNER}");
        return Ok(());
    }

    // If we were just re-execed by a self-update, show the release notes
    // for the new version once, then continue with the user's command.
    // The sentinel only lives in the immediate post-exec env, so this
    // fires exactly once per upgrade.
    if let Some(updated_to) = phai_core::compat::env_var_os("PHAI_UPDATED", "FINANCE_OS_UPDATED") {
        let version = updated_to.to_string_lossy().to_string();
        if version == env!("CARGO_PKG_VERSION") {
            update::print_release_notes(&version).await;
        }
    }

    // Auto-update check (never blocks command execution)
    if should_run_auto_check(&cli) {
        let paths = ConfigPaths::discover().ok();
        if let Some(ref p) = paths {
            update::auto_check(&p.data_dir).await;
        }
    }

    run_command(cli.command).await
}

async fn run_command(command: Option<Commands>) -> Result<()> {
    match command {
        None => {
            // Bare `phai` is no longer an interactive surface — `phai serve`
            // is. Print the help so the user can discover the command set.
            use clap::CommandFactory as _;
            Cli::command().print_help()?;
            println!();
            println!("Para a interface interativa, rode `phai serve`.");
            Ok(())
        }
        Some(Commands::SelfCmd { command }) => self_cmd::run(command).await,
        Some(Commands::Auth { command }) => match command {
            AuthCommand::Setup(args) => auth_setup(args).await,
        },
        Some(Commands::Invite { command }) => run_invite(command).await,
        Some(Commands::Admin { command }) => run_admin(command).await,
        Some(Commands::Sync { command }) => run_sync(command).await,
        Some(Commands::Report { csv, command }) => match command {
            ReportCommand::DailyPulse(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_daily_pulse(args, format).await
            }
            ReportCommand::MonthlySpend(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_monthly_spend(args, format).await
            }
            ReportCommand::Cashflow(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_cashflow(args, format).await
            }
            ReportCommand::CashflowChart(args) => {
                if csv {
                    bail!("report cashflow-chart não suporta --csv; use --text ou o SVG de saída");
                }
                report_cashflow_chart(args).await
            }
            ReportCommand::ForecastVsActual(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_forecast_vs_actual(args, format).await
            }
            ReportCommand::CardSummary(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_card_summary(args, format).await
            }
            ReportCommand::CardClosedInsights(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_card_closed_insights(args, format).await
            }
            ReportCommand::Installments(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_installments(args, format).await
            }
            ReportCommand::Uncategorized(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_uncategorized(args, format).await
            }
            ReportCommand::Duplicates(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_duplicates(args, format).await
            }
            ReportCommand::SplitCandidates(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_split_candidates(args, format).await
            }
            ReportCommand::ItemPrices(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_item_prices(args, format).await
            }
            ReportCommand::DataHealth(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_data_health(args, format).await
            }
            ReportCommand::Scenario(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_scenario(args, format).await
            }
            ReportCommand::OfxConsistency(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_ofx_consistency(args, format).await
            }
            ReportCommand::BudgetStatus(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_budget_status(args, format).await
            }
            ReportCommand::Cards(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_cards(args, format).await
            }
            ReportCommand::Balances(args) => {
                let format = report_output_format(csv, args.structured_output())?;
                report_balances(args, format).await
            }
        },
        Some(Commands::Tx { command }) => match command {
            TxCommand::UpsertManual(args) => tx_upsert_manual(args).await,
            TxCommand::Categorize(args) => tx_categorize(args).await,
            TxCommand::SetAnatomy(args) => tx_set_anatomy(args).await,
            TxCommand::SetContext(args) => tx_set_context(args).await,
            TxCommand::ListContext(args) => tx_list_context(args).await,
            TxCommand::Find(args) => tx_find(args).await,
            TxCommand::Pending(args) => tx_pending(args).await,
            TxCommand::PendingHuman(args) => tx_pending_human(args).await,
            TxCommand::ReviewHuman(args) => tx_review_human(args).await,
            TxCommand::Merge(args) => tx_merge(args).await,
            TxCommand::SetContextByDesc(args) => tx_set_context_by_desc(args).await,
            TxCommand::Split { command } => match command {
                TxSplitCommand::Preview(args) => tx_split_preview(args).await,
                TxSplitCommand::Apply(args) => tx_split_apply(args).await,
                TxSplitCommand::Show(args) => tx_split_show(args).await,
                TxSplitCommand::Clear(args) => tx_split_clear(args).await,
            },
            TxCommand::Enrich(args) => tx_enrich(args).await,
            TxCommand::ReplicateAnatomy(args) => tx_replicate_anatomy(args).await,
        },
        Some(Commands::Forecast { command }) => match command {
            ForecastCommand::Upsert(args) => forecast_upsert(*args).await,
            ForecastCommand::RefreshInstallments(args) => {
                forecast_cmd::run_refresh_installments(args).await
            }
            ForecastCommand::Refresh(args) => forecast_cmd::run_refresh(args).await,
            ForecastCommand::Reconcile(args) => forecast_cmd::run_reconcile(args).await,
            ForecastCommand::Suggest(args) => forecast_cmd::run_suggest(args).await,
            ForecastCommand::Accept(args) => forecast_cmd::run_accept(args).await,
            ForecastCommand::Dismiss(args) => forecast_cmd::run_dismiss(args).await,
            ForecastCommand::Scenario(args) => forecast_cmd::run_scenario(args).await,
        },
        Some(Commands::Scenario { command }) => scenario_cmd::run(command).await,
        Some(Commands::Rule { command }) => match command {
            RuleCommand::Upsert(args) => rule_upsert(args).await,
            RuleCommand::List(args) => rule_list(args).await,
            RuleCommand::Inspect(args) => rule_inspect(args).await,
        },
        Some(Commands::Account { command }) => match command {
            AccountCommand::Upsert(args) => account_upsert(args).await,
            AccountCommand::SetBillingCycle(args) => account_set_billing_cycle(args).await,
        },
        Some(Commands::Budget { command }) => match command {
            BudgetCommand::Upsert(args) => budget_upsert(args).await,
            BudgetCommand::List(args) => budget_list(args).await,
        },
        Some(Commands::Mcp) => mcp::run(),
        Some(Commands::Serve { port, command }) => match command {
            None => serve::run(port).await,
            Some(ServeCommand::Install(args)) => serve_install::install(args),
            Some(ServeCommand::Uninstall(args)) => serve_install::uninstall(args),
        },
        Some(Commands::Notify { command }) => match command {
            NotifyCommand::Whatsapp(args) => notify_whatsapp(args).await,
        },
    }
}

const WHATSAPP_WEBHOOK_URL_ENV: &str = "PHAI_WHATSAPP_WEBHOOK_URL";
const WHATSAPP_WEBHOOK_URL_ENV_LEGACY: &str = "FINANCE_OS_WHATSAPP_WEBHOOK_URL";
const WHATSAPP_WEBHOOK_TOKEN_ENV: &str = "PHAI_WHATSAPP_WEBHOOK_TOKEN";
const WHATSAPP_WEBHOOK_TOKEN_ENV_LEGACY: &str = "FINANCE_OS_WHATSAPP_WEBHOOK_TOKEN";

async fn notify_whatsapp(args: NotifyWhatsappArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let today = Utc::now().date_naive();
    let data = pulse::gather_pulse_data(store.as_ref(), today, args.days).await?;
    let plan = pulse::compute_closing_plan(&data);
    let body = pulse::render_pulse(&data, &plan, args.days);

    if args.echo || args.dry_run {
        println!("{body}");
    }

    if args.dry_run {
        return Ok(());
    }

    let url = phai_core::compat::env_var(WHATSAPP_WEBHOOK_URL_ENV, WHATSAPP_WEBHOOK_URL_ENV_LEGACY)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{WHATSAPP_WEBHOOK_URL_ENV} not set. Export the webhook URL or use --dry-run."
            )
        })?;
    let token = phai_core::compat::env_var(
        WHATSAPP_WEBHOOK_TOKEN_ENV,
        WHATSAPP_WEBHOOK_TOKEN_ENV_LEGACY,
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("failed to build HTTP client")?;
    let payload = json!({ "text": body });
    let mut req = client.post(&url).json(&payload);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let response = req
        .send()
        .await
        .with_context(|| format!("POST {url} failed"))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        anyhow::bail!("webhook returned {status}: {text}");
    }
    if args.echo {
        eprintln!("notify whatsapp: {status}");
    }
    Ok(())
}

async fn auth_setup(args: AuthSetupArgs) -> Result<()> {
    let paths = ConfigPaths::discover()?;
    paths.ensure()?;
    let mut config = AppConfig::load(&paths)?;
    config.backend = args.backend.into();
    config.actor_id = args.actor_id;
    config.project_id = args.project_id;
    config.dataset_id = args.dataset_id;
    config.service_account_path = args.service_account_path;
    config.local_db_path = match (&config.backend, &args.local_db_path) {
        // Explicit flag always wins.
        (_, Some(p)) => Some(p.clone()),
        // Switching to BigQuery: clear any residual local_db_path so it
        // doesn't diverge silently from the remote store.
        (phai_core::config::BackendKind::Bigquery, None) => {
            if config.local_db_path.is_some() {
                eprintln!("aviso: local_db_path removido — não necessário com backend bigquery");
            }
            None
        }
        // Local backend: keep whatever was set (or None for a fresh install).
        (phai_core::config::BackendKind::Local, None) => config.local_db_path.clone(),
    };
    config.pluggy_start_date = args.pluggy_start_date.or(config.pluggy_start_date.clone());
    config.save(&paths)?;

    println!("Configuração salva em {}", paths.config_file.display());
    println!("backend: {:?}", config.backend);
    println!("actor_id: {}", config.actor_id);
    if let Some(db_path) = &config.local_db_path {
        println!("local_db: {}", db_path.display());
    }
    Ok(())
}

async fn run_invite(command: InviteCommand) -> Result<()> {
    match command {
        InviteCommand::Create(args) => invite_create(args).await,
    }
}

async fn run_admin(command: AdminCommand) -> Result<()> {
    match command {
        AdminCommand::Migrate => admin_migrate().await,
        AdminCommand::ImportLegacy(args) => admin_import_legacy(args).await,
        AdminCommand::Reclassify(args) => admin_reclassify(args).await,
    }
}

async fn run_sync(command: SyncCommand) -> Result<()> {
    match command {
        SyncCommand::Pluggy(args) => sync_pluggy_command(args).await,
    }
}

async fn invite_create(args: InviteCreateArgs) -> Result<()> {
    let paths = ConfigPaths::discover()?;
    let owner_config = AppConfig::load(&paths).ok();

    let project_id = args
        .project_id
        .or_else(|| owner_config.as_ref().and_then(|c| c.project_id.clone()))
        .context("--project-id não informado e nenhum project configurado")?;
    let dataset_id = args
        .dataset_id
        .or_else(|| owner_config.as_ref().and_then(|c| c.dataset_id.clone()))
        .context("--dataset-id não informado e nenhum dataset configurado")?;

    let raw = fs::read_to_string(&args.service_account_path).with_context(|| {
        format!(
            "Falha ao ler service account em {}",
            args.service_account_path.display()
        )
    })?;
    let service_account: Value =
        serde_json::from_str(&raw).context("service account não é um JSON válido")?;
    if service_account.get("private_key").is_none() || service_account.get("client_email").is_none()
    {
        bail!("JSON não parece uma chave de service account (faltam private_key/client_email)");
    }

    let passphrase = read_invite_passphrase(args.passphrase_stdin)?;
    let invite = phai_core::Invite::new(
        project_id,
        dataset_id,
        args.actor_id,
        args.role,
        service_account,
        args.label,
    );
    let token = phai_core::seal_invite(&invite, &passphrase)?;

    // Token to stdout so it can be piped/saved; guidance to stderr.
    println!("{token}");
    eprintln!();
    eprintln!("Convite cifrado gerado. Envie a chave acima e a senha por canais separados.");
    eprintln!(
        "Quem tiver a chave + a senha tem acesso de escrita ao dataset — trate como segredo."
    );
    Ok(())
}

/// Collect the invite passphrase, either from the first line of stdin (for
/// scripting) or via a hidden double-entry prompt.
fn read_invite_passphrase(from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut line = String::new();
        io::stdin()
            .read_line(&mut line)
            .context("ler senha do stdin")?;
        let pass = line.trim_end_matches(['\n', '\r']).to_string();
        if pass.is_empty() {
            bail!("senha vazia recebida no stdin");
        }
        return Ok(pass);
    }
    let pass = rpassword::prompt_password("Senha do convite: ").context("ler senha")?;
    if pass.is_empty() {
        bail!("a senha não pode ser vazia");
    }
    let confirm = rpassword::prompt_password("Confirme a senha: ").context("ler confirmação")?;
    if pass != confirm {
        bail!("as senhas não conferem");
    }
    Ok(pass)
}

async fn admin_migrate() -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    let executed = run_migrations(store.as_ref(), &config).await?;
    if executed.is_empty() {
        println!("Nenhuma migration pendente.");
    } else {
        println!("Migrations aplicadas:");
        for version in executed {
            println!("- {version}");
        }
    }
    Ok(())
}

async fn admin_import_legacy(args: ImportLegacyArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let bundle = load_legacy_bundle(&args.finance_root, &config.actor_id)?;
    let mut audit = Vec::new();

    upsert_accounts_chunked(store.as_ref(), &bundle.accounts).await?;
    for row in &bundle.accounts {
        audit.push(AuditEvent::from_entity(
            "account",
            &row.account_id,
            "import_legacy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }

    upsert_categories_chunked(store.as_ref(), &bundle.categories).await?;
    for row in &bundle.categories {
        audit.push(AuditEvent::from_entity(
            "category",
            &row.category_id,
            "import_legacy",
            &config.actor_id,
            &format!("category:{}", row.category_id),
            serde_json::to_value(row)?,
        ));
    }

    upsert_rules_chunked(store.as_ref(), &bundle.rules).await?;
    for row in &bundle.rules {
        audit.push(AuditEvent::from_entity(
            "rule",
            &row.rule_id,
            "import_legacy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }

    upsert_forecasts_chunked(store.as_ref(), &bundle.forecasts).await?;
    for row in &bundle.forecasts {
        audit.push(AuditEvent::from_entity(
            "forecast",
            &row.forecast_id,
            "import_legacy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }

    upsert_transactions_chunked(store.as_ref(), &bundle.transactions).await?;
    for row in &bundle.transactions {
        audit.push(AuditEvent::from_entity(
            "transaction",
            &row.transaction_id,
            "import_legacy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }

    insert_audit_chunked(store.as_ref(), &audit).await?;

    println!("Import legado concluído:");
    println!("- accounts: {}", bundle.accounts.len());
    println!("- categories: {}", bundle.categories.len());
    println!("- rules: {}", bundle.rules.len());
    println!("- forecasts: {}", bundle.forecasts.len());
    println!("- transactions: {}", bundle.transactions.len());
    Ok(())
}

async fn admin_reclassify(args: ReclassifyArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let compiled_rules = compile_rules(&store.active_rules().await?)?;
    if compiled_rules.is_empty() {
        println!("Nenhuma regra ativa encontrada. Importe regras antes com `admin import-legacy`.");
        return Ok(());
    }

    let since =
        NaiveDate::from_ymd_opt(2020, 1, 1).context("invalid reclassify epoch (2020-01-01)")?;
    let today = Utc::now().date_naive();
    let items = store.transactions_in_date_range(None, since, today).await?;
    println!("Transações encontradas: {}", items.len());
    println!("Regras compiladas: {}", compiled_rules.len());

    let mut changed = 0u64;
    let mut unchanged = 0u64;
    let mut audit = Vec::new();

    for item in &items {
        let rule_application = apply_rules_with_facts(
            &item.raw_description,
            Some(item.amount),
            Some(item.transaction_id.as_str()),
            item.category_id.clone(),
            item.category_id.is_some(),
            &compiled_rules,
        );
        let new_category_id = rule_application.category_id;
        let new_source = rule_application.category_source;
        let new_context = rule_application.context;

        if new_source != "rule" {
            unchanged += 1;
            continue;
        }

        let cat_changed = new_category_id != item.category_id;
        if !cat_changed && new_context.is_none() {
            unchanged += 1;
            continue;
        }

        if args.dry_run {
            println!(
                "  [DRY-RUN] {} {:50} {:30} -> {}",
                item.transaction_date,
                human_format::truncate_with_ellipsis(item.display_description(), 50),
                item.category_id.as_deref().unwrap_or("(nenhuma)"),
                new_category_id.as_deref().unwrap_or("(nenhuma)")
            );
        } else {
            let idem_key = format!("reclassify:{}", item.transaction_id);
            store
                .annotate_transaction(
                    &item.transaction_id,
                    new_category_id.as_deref(),
                    Some(&new_source),
                    new_context.as_deref(),
                    &config.actor_id,
                    &idem_key,
                )
                .await?;
            audit.push(AuditEvent::from_entity(
                "transaction",
                &item.transaction_id,
                "reclassify",
                &config.actor_id,
                &idem_key,
                serde_json::json!({
                    "old_category_id": item.category_id,
                    "new_category_id": new_category_id,
                    "source": new_source,
                    "new_context": new_context,
                }),
            ));
        }
        changed += 1;
    }

    if !audit.is_empty() {
        insert_audit_chunked(store.as_ref(), &audit).await?;
    }

    let mode = if args.dry_run { " (dry-run)" } else { "" };
    println!("Reclassificação concluída{mode}:");
    println!("- alteradas: {changed}");
    println!("- sem alteração: {unchanged}");
    Ok(())
}

async fn sync_pluggy_command(args: SyncPluggyArgs) -> Result<()> {
    if args.json_summary && args.notify_summary {
        bail!("Use apenas uma saída de resumo: --json-summary ou --notify-summary");
    }

    // Always run on the latest binary before touching external data.
    // Skip when we just re-execed after an upgrade (PHAI_UPDATED) or when the
    // user opted out of auto-update (PHAI_NO_AUTO_UPDATE) — CI and e2e tests
    // rely on the latter to keep `target/debug/phai` stable across subprocess
    // invocations. Legacy FINANCE_OS_* names are still honored (ADR-0021).
    if phai_core::compat::env_var_os("PHAI_UPDATED", "FINANCE_OS_UPDATED").is_none()
        && phai_core::compat::env_var_os("PHAI_NO_AUTO_UPDATE", "FINANCE_OS_NO_AUTO_UPDATE")
            .is_none()
    {
        if let Ok(paths) = ConfigPaths::discover() {
            update::force_check(&paths.data_dir).await;
        }
    }

    let (_, config) = load_config().await?;
    let pluggy_config = args
        .pluggy_config
        .or_else(|| config.pluggy_config_path.clone())
        .unwrap_or_else(|| PathBuf::from("finance/pluggy-config.json"));
    let accounts_csv = args
        .accounts_csv
        .or_else(|| config.pluggy_accounts_csv_path.clone())
        .unwrap_or_else(|| PathBuf::from("finance/data/contas.csv"));
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let compiled_rules = compile_rules(&store.active_rules().await?)?;
    let internal_categories = store.internal_categories().await?;

    let effective_from = resolve_sync_from(
        args.from.as_deref(),
        config.pluggy_start_date.as_deref(),
        store.latest_pluggy_transaction_date().await?,
    )?;
    let to = args
        .to
        .unwrap_or_else(|| Utc::now().date_naive().format("%Y-%m-%d").to_string());
    let (mut accounts, transactions, rebinds) = sync_pluggy(SyncPluggyParams {
        actor_id: &config.actor_id,
        pluggy_config_path: &pluggy_config,
        accounts_csv_path: Some(&accounts_csv),
        fixture_path: args.fixture.as_deref(),
        from_override: Some(&effective_from),
        to_date: &to,
        rules: &compiled_rules,
        internal_categories: &internal_categories,
        api_base_url: None,
    })
    .await?;
    let existing_ids = store
        .existing_transaction_ids(
            &transactions
                .iter()
                .map(|row| row.transaction_id.clone())
                .collect::<Vec<_>>(),
        )
        .await?;
    // Merge billing metadata from existing accounts so that manually set
    // billing_closing_day / billing_due_day survive Pluggy re-syncs (which
    // always rebuild metadata_json from the API payload). Accounts missing
    // from the registry also keep their stored owner/type/bank/label instead
    // of falling back to Pluggy's generic values.
    let registry = phai_core::legacy::load_account_registry(&accounts_csv)?;
    if let Ok(existing_accounts) = store.get_accounts().await {
        let existing_map: BTreeMap<&str, &AccountRecord> = existing_accounts
            .iter()
            .map(|a| (a.account_id.as_str(), a))
            .collect();
        for acct in &mut accounts {
            if let Some(existing) = existing_map.get(acct.account_id.as_str()) {
                merge_billing_metadata(&mut acct.metadata_json, &existing.metadata_json);
                if !registry.contains_key(&acct.account_id) {
                    keep_stored_account_identity(acct, existing);
                }
            }
        }
    }

    let categories = build_category_records_from_transactions(&config.actor_id, &transactions);
    let mut audit = Vec::new();

    upsert_accounts_chunked(store.as_ref(), &accounts).await?;

    // Insert one snapshot per account for today's date — idempotent on repeated syncs.
    let today = Utc::now().date_naive();
    let snapshots: Vec<AccountSnapshotRecord> = accounts
        .iter()
        .map(|row| {
            let idempotency_key = format!(
                "snapshot:{}:{}:pluggy",
                row.account_id,
                today.format("%Y-%m-%d")
            );
            AccountSnapshotRecord {
                snapshot_id: Uuid::now_v7().to_string(),
                account_id: row.account_id.clone(),
                snapshot_date: today,
                balance: row.metadata_json.get("balance").and_then(|v| {
                    v.as_str()
                        .and_then(|s| s.parse::<Decimal>().ok())
                        .or_else(|| {
                            v.as_f64()
                                .and_then(|f| format!("{f:.2}").parse::<Decimal>().ok())
                        })
                }),
                credit_limit: None,
                currency_code: row
                    .metadata_json
                    .get("currency_code")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                source: "pluggy".to_string(),
                actor_id: config.actor_id.clone(),
                idempotency_key,
                metadata_json: serde_json::json!({}),
                created_at: Utc::now(),
            }
        })
        .collect();
    insert_snapshots_chunked(store.as_ref(), &snapshots).await?;

    upsert_categories_chunked(store.as_ref(), &categories).await?;
    let (transactions, mut dedup_audit) =
        dedup_pluggy_duplicates(store.as_ref(), &config.actor_id, transactions).await?;
    audit.append(&mut dedup_audit);
    upsert_transactions_chunked(store.as_ref(), &transactions).await?;

    for rebind in &rebinds {
        audit.push(AuditEvent::from_entity(
            "account",
            &rebind.internal_account_id,
            "rebind",
            &config.actor_id,
            &format!(
                "rebind:{}:{}",
                rebind.binding_id, rebind.to_pluggy_account_id
            ),
            json!({
                "from": rebind.from_pluggy_account_id,
                "to": rebind.to_pluggy_account_id,
                "pluggy_item_id": rebind.pluggy_item_id,
            }),
        ));
    }
    for row in &accounts {
        audit.push(AuditEvent::from_entity(
            "account",
            &row.account_id,
            "sync_pluggy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }
    for row in &transactions {
        audit.push(AuditEvent::from_entity(
            "transaction",
            &row.transaction_id,
            "sync_pluggy",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(row)?,
        ));
    }
    insert_audit_chunked(store.as_ref(), &audit).await?;

    let account_labels = accounts
        .iter()
        .map(|row| (row.account_id.clone(), row.label.clone()))
        .collect::<BTreeMap<_, _>>();
    let new_transactions = transactions
        .iter()
        .filter(|row| !existing_ids.contains(&row.transaction_id))
        .map(|row| SyncSummaryTransaction {
            transaction_id: row.transaction_id.clone(),
            transaction_date: row.transaction_date.format("%Y-%m-%d").to_string(),
            day_of_week: day_of_week_text(row.transaction_date),
            description: row.display_description().to_string(),
            amount: decimal_text(row.amount),
            tx_type: row.tx_type.clone(),
            category_id: row.category_id.clone(),
            category_source: row.category_source.clone(),
            context: row.context.clone(),
            account_id: row.account_id.clone(),
            account_label: row
                .account_id
                .as_ref()
                .and_then(|account_id| account_labels.get(account_id).cloned()),
            payment_status: row.payment_status.clone(),
            source: row.source.clone(),
            metadata_json: row.metadata_json.clone(),
        })
        .collect::<Vec<_>>();
    if args.json_summary || args.notify_summary {
        // Run the forecast orchestrator silently so the pending_summary
        // reflects fresh, reconciled forecast state. Failure is non-fatal —
        // the summary just sees yesterday's forecast view.
        if !args.no_forecast_refresh {
            if let Err(err) = forecast_cmd::refresh_all(store.as_ref(), &config, 12, 6, false).await
            {
                eprintln!("⚠️  Forecast refresh falhou (sync ainda OK): {err:#}");
            }
        }
        let pending_summary = load_sync_pending_summary(
            store.as_ref(),
            &transactions,
            &existing_ids,
            &account_labels,
        )
        .await;
        let summary = SyncSummaryOutput {
            actor_id: config.actor_id.clone(),
            backend: format!("{:?}", config.effective_backend()).to_lowercase(),
            generated_at: Utc::now().to_rfc3339(),
            summary_status: pending_summary.summary_status,
            warnings: pending_summary.warnings,
            new_transactions_count: new_transactions.len(),
            needs_context_count: pending_summary.needs_context_count,
            needs_context_returned_count: pending_summary.needs_context_returned_count,
            needs_context_truncated: pending_summary.needs_context_truncated,
            new_transactions,
            needs_context: pending_summary.needs_context,
        };
        if args.json_summary {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            // The human-friendly notify-summary message folds in the
            // current saldo per checking account so a phone-side reader
            // immediately sees the balance impact of the new transactions.
            let snapshots = store.latest_account_snapshots().await.unwrap_or_default();
            let stored_accounts = store.get_accounts().await.unwrap_or_default();
            println!(
                "{}",
                render_sync_notify_summary(&summary, &stored_accounts, &snapshots)
            );
        }
        return Ok(());
    }

    println!("Sync Pluggy concluído:");
    println!("- accounts: {}", accounts.len());
    println!("- transactions: {}", transactions.len());
    println!("- categories: {}", categories.len());
    println!("- actor: {}", config.actor_id);
    println!("- backend: {:?}", config.effective_backend());

    // Phase 5 — post-sync enrichment hook. Non-fatal: any failure
    // inside `enrich_after_sync` is logged via eprintln and the sync
    // result is preserved. The user can re-run `finance tx enrich`
    // manually whenever the LLM is back up.
    let new_tx_ids: Vec<String> = transactions
        .iter()
        .filter(|row| !existing_ids.contains(&row.transaction_id))
        .map(|row| row.transaction_id.clone())
        .collect();
    println!(
        "Sincronização concluída: {} transações novas",
        new_tx_ids.len()
    );

    if args.no_enrich {
        println!("Enrichment automático: pulado (--no-enrich).");
    } else if new_tx_ids.is_empty() {
        println!("Enrichment automático: sem transações novas para processar.");
    } else {
        // Non-TTY (CI, pipes, batch) → force auto_only so we never
        // print Suggest/AskUser prompts to a non-interactive stdout.
        let auto_only = !std::io::IsTerminal::is_terminal(&std::io::stdin());
        let summary =
            enrich::enrich_after_sync(&config, store.as_ref(), &new_tx_ids, auto_only).await;
        println!("{}", summary.format_summary());
        if summary.deferred > 0 {
            println!("Para revisar as adiadas: finance tx enrich --days 7");
        }
    }

    // Phase 6 — post-sync forecast orchestrator (ADR-0016).
    // Runs the full pipeline (installments → reconcile → materialise →
    // detect new candidates) so the chart and `scenario` reports always
    // see an up-to-date forecast table. Non-fatal: a failure is logged
    // and the sync result is preserved.
    if args.no_forecast_refresh {
        println!("Forecast refresh automático: pulado (--no-forecast-refresh).");
    } else {
        match forecast_cmd::refresh_all(store.as_ref(), &config, 12, 6, false).await {
            Ok(report) => {
                println!("{}", forecast_cmd::refresh_one_line(&report));
                if report.new_suggestions > 0 {
                    println!(
                        "Há {} candidato(s) recorrente(s) aguardando revisão — \
                         rode `fin forecast suggest` para ver e aceitar/descartar.",
                        report.new_suggestions
                    );
                }
            }
            Err(err) => {
                eprintln!("⚠️  Forecast refresh falhou (sync ainda OK): {err:#}");
            }
        }
    }
    Ok(())
}

fn resolve_sync_from(
    explicit_from: Option<&str>,
    configured_start: Option<&str>,
    latest_seen: Option<NaiveDate>,
) -> Result<String> {
    if let Some(value) = explicit_from {
        return Ok(value.to_string());
    }

    let configured_start_date = configured_start
        .map(|value| {
            NaiveDate::parse_from_str(value, "%Y-%m-%d")
                .with_context(|| format!("Falha ao parsear pluggy_start_date {value}"))
        })
        .transpose()?;

    let computed = latest_seen
        .and_then(|latest| latest.checked_sub_signed(Duration::days(DEFAULT_SYNC_LOOKBACK_DAYS)))
        .or(configured_start_date)
        .unwrap_or_else(|| Utc::now().date_naive() - Duration::days(DEFAULT_SYNC_LOOKBACK_DAYS));

    Ok(configured_start_date
        .map(|start| start.max(computed))
        .unwrap_or(computed)
        .format("%Y-%m-%d")
        .to_string())
}

async fn report_daily_pulse(args: DailyPulseArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    if format.structured_output() {
        // Backwards-compat: --raw still emits the flat list of items.
        let since = Utc::now()
            .date_naive()
            .checked_sub_signed(Duration::days(args.days.saturating_sub(1)))
            .context("Falha ao calcular janela do daily pulse")?;
        let items = store.daily_pulse(since).await?;
        print_report_output(format, &items)?;
        return Ok(());
    }

    let today = Utc::now().date_naive();
    let data = pulse::gather_pulse_data(store.as_ref(), today, args.days).await?;
    let plan = pulse::compute_closing_plan(&data);
    println!("{}", pulse::render_pulse(&data, &plan, args.days));
    Ok(())
}

async fn report_balances(_args: BalancesArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let accounts = store.get_accounts().await?;
    let snapshots = store.latest_account_snapshots().await?;

    // Index account → owner + label.
    let account_by_id: BTreeMap<&str, &AccountRecord> = accounts
        .iter()
        .filter(|a| !a.account_id.is_empty())
        .map(|a| (a.account_id.as_str(), a))
        .collect();

    // Only checking accounts have a meaningful "saldo em conta". Credit
    // cards expose debt via card_summary; mixing them here would mislead.
    let rows: Vec<(&AccountRecord, &phai_core::models::AccountSnapshotRecord)> = snapshots
        .iter()
        .filter_map(|s| {
            account_by_id
                .get(s.account_id.as_str())
                .filter(|a| is_checking_account_type(&a.account_type))
                .map(|a| (*a, s))
        })
        .collect();

    if format.structured_output() {
        let payload: Vec<_> = rows
            .iter()
            .map(|(a, s)| {
                serde_json::json!({
                    "account_id": a.account_id,
                    "owner": a.owner,
                    "label": a.label,
                    "balance": s.balance,
                    "currency_code": s.currency_code,
                    "snapshot_date": s.snapshot_date,
                })
            })
            .collect();
        print_report_output(format, &payload)?;
        return Ok(());
    }

    if rows.is_empty() {
        println!("{}", human_format::bold("Saldo em conta"));
        println!("  _(nenhum snapshot disponível — rode `finance sync pluggy` para atualizar)_");
        return Ok(());
    }

    // Group by owner so "Aline · Felipe · Total" reads naturally.
    let mut by_owner: BTreeMap<
        String,
        Vec<(&AccountRecord, &phai_core::models::AccountSnapshotRecord)>,
    > = BTreeMap::new();
    for (acc, snap) in &rows {
        by_owner
            .entry(acc.owner.clone())
            .or_default()
            .push((acc, snap));
    }

    println!("{}", human_format::bold("Saldo em conta"));
    let mut grand_total = Decimal::ZERO;
    for (owner, items) in &by_owner {
        let mut owner_total = Decimal::ZERO;
        for (acc, snap) in items {
            let balance = snap.balance.unwrap_or(Decimal::ZERO);
            owner_total += balance;
            grand_total += balance;
            let label = if acc.label.is_empty() {
                acc.account_id.clone()
            } else {
                acc.label.clone()
            };
            println!(
                "  💰 {} · {} ({})",
                label,
                human_format::brl_signed(balance),
                human_format::short_snapshot_ts(snap.created_at),
            );
        }
        if items.len() > 1 {
            println!(
                "  └ {}: {}",
                human_format::bold(&capitalize(owner)),
                human_format::brl_signed(owner_total),
            );
        }
    }
    println!(
        "  {}: {}",
        human_format::bold("Total"),
        human_format::brl_signed(grand_total),
    );
    Ok(())
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

async fn report_monthly_spend(args: MonthlySpendArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.monthly_spend(args.month.as_deref()).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    print_monthly_spend_human(&rows, args.month.as_deref());
    Ok(())
}

fn print_monthly_spend_human(rows: &[phai_core::models::MonthlySpendRow], month: Option<&str>) {
    use human_format::{
        bold, brl, category_emoji, category_family, family_label, month_label, pct,
    };
    use std::cmp::Reverse;
    use std::collections::HashMap;

    // Determine the month_ref label
    let month_display = month
        .map(month_label)
        .or_else(|| rows.first().map(|r| month_label(&r.month_ref)))
        .unwrap_or_else(|| "—".to_string());

    println!("📊 {}", bold(&format!("Gastos · {month_display}")));

    if rows.is_empty() {
        println!("- linhas: 0");
        return;
    }

    let internal_prefix = "financeiro-pagamento-recebido";

    // Filter out internal categories
    let visible: Vec<_> = rows
        .iter()
        .filter(|r| !r.category_id.starts_with(internal_prefix))
        .collect();

    // Group by family, accumulate spend per family
    let mut family_totals: HashMap<String, Decimal> = HashMap::new();
    // family → list of (category_id, spend)
    let mut family_rows: HashMap<String, Vec<(&str, Decimal)>> = HashMap::new();

    for row in &visible {
        let spend = -row.expenses; // expenses are stored negative
        let family =
            category_family(Some(&row.category_id)).unwrap_or_else(|| "outros".to_string());
        *family_totals.entry(family.clone()).or_insert(Decimal::ZERO) += spend;
        family_rows
            .entry(family)
            .or_default()
            .push((&row.category_id, spend));
    }

    // Sort families by spend descending
    let mut families: Vec<String> = family_totals.keys().cloned().collect();
    families.sort_by_key(|f| Reverse(family_totals[f]));

    let grand_total: Decimal = family_totals.values().copied().sum();

    println!();
    for family in &families {
        let family_spend = family_totals[family];
        let emoji = category_emoji(Some(family), None);
        let label = family_label(family);
        println!("{emoji} *{}*   {}", label, brl(family_spend));

        let sub_rows = &family_rows[family];
        // Only show sub-breakdown when there are multiple sub-categories
        if sub_rows.len() > 1 {
            let mut sorted_sub = sub_rows.clone();
            sorted_sub.sort_by_key(|&(_, v)| Reverse(v));
            for (cat_id, spend) in &sorted_sub {
                // Strip family prefix to show only the sub-category part
                let sub_label = cat_id
                    .strip_prefix(family)
                    .and_then(|s| s.strip_prefix(':').or_else(|| s.strip_prefix('-')))
                    .unwrap_or(cat_id)
                    .replace(':', " > ")
                    .replace('-', " ");
                println!("  _{}   {}_", sub_label, brl(*spend));
            }
        }
    }

    println!();
    println!("*Total*: {}", brl(grand_total));

    // Top-3 category share breakdown
    if grand_total > Decimal::ZERO && families.len() > 1 {
        println!();
        let top3 = families.iter().take(3);
        for family in top3 {
            let share = family_totals[family] / grand_total * Decimal::from(100u32);
            println!("  {} {}%", family_label(family), pct(share));
        }
    }
}

async fn report_cashflow_chart(args: CashflowChartArgs) -> Result<()> {
    cashflow_chart::report_cashflow_chart(args).await
}

fn month_bounds(month_ref: &str) -> Result<(NaiveDate, NaiveDate)> {
    let start = parse_month_ref(month_ref)?;
    let next = shift_month(start, 1)?;
    let end = next
        .checked_sub_signed(Duration::days(1))
        .context("Falha ao calcular fim do mês")?;
    Ok((start, end))
}

fn combine_cashflow_rows(month_ref: &str, left: &CashflowRow, right: &CashflowRow) -> CashflowRow {
    CashflowRow {
        month_ref: month_ref.to_string(),
        income: left.income + right.income,
        expenses: left.expenses + right.expenses,
        expense_reduction: left.expense_reduction + right.expense_reduction,
        net: left.net + right.net,
        opening_balance: left.opening_balance,
        closing_balance: left.closing_balance.map(|balance| balance + right.net),
    }
}

fn summarize_forecast_groups(month_ref: &str, forecast: &[CashflowCategoryGroup]) -> CashflowRow {
    let mut income = Decimal::ZERO;
    let mut expenses = Decimal::ZERO;
    for group in forecast {
        if group.subtotal >= Decimal::ZERO {
            income += group.subtotal;
        } else {
            expenses += -group.subtotal;
        }
    }
    CashflowRow {
        month_ref: month_ref.to_string(),
        income,
        expenses,
        expense_reduction: Decimal::ZERO,
        net: income - expenses,
        opening_balance: None,
        closing_balance: None,
    }
}

fn cashflow_category_id(category_id: Option<&str>) -> String {
    category_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("sem-categoria")
        .to_string()
}

fn cashflow_transaction_description(tx: &TransactionRecord) -> String {
    tx.display_description().trim().to_string()
}

fn add_cashflow_group_item(
    groups: &mut BTreeMap<String, CashflowCategoryGroup>,
    category_id: String,
    item: CashflowDetailItem,
) {
    let entry = groups
        .entry(category_id.clone())
        .or_insert_with(|| CashflowCategoryGroup {
            category_id,
            subtotal: Decimal::ZERO,
            transactions: 0,
            items: Vec::new(),
        });
    entry.subtotal += item.amount;
    entry.transactions += 1;
    entry.items.push(item);
}

fn sorted_cashflow_groups(
    groups: BTreeMap<String, CashflowCategoryGroup>,
) -> Vec<CashflowCategoryGroup> {
    let mut rows = groups.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.subtotal
            .abs()
            .cmp(&a.subtotal.abs())
            .then_with(|| a.category_id.cmp(&b.category_id))
    });
    for row in &mut rows {
        row.items.sort_by(|a, b| {
            b.amount
                .abs()
                .cmp(&a.amount.abs())
                .then_with(|| a.description.cmp(&b.description))
        });
    }
    rows
}

fn cashflow_item_from_transaction(tx: &TransactionRecord, source: &str) -> CashflowDetailItem {
    CashflowDetailItem {
        description: cashflow_transaction_description(tx),
        amount: tx.amount,
        category_id: tx.category_id.clone(),
        account_id: tx.account_id.clone(),
        transaction_id: Some(tx.transaction_id.clone()),
        transaction_date: Some(tx.transaction_date),
        source: source.to_string(),
    }
}

fn is_credit_card_payment_transaction(tx: &TransactionRecord) -> bool {
    if tx.amount >= Decimal::ZERO {
        return false;
    }
    if tx.category_id.as_deref().is_some_and(|category| {
        category.contains("credit-card-payment") || category.contains("pagamento-fatura")
    }) {
        return true;
    }
    let description = format!(
        "{} {} {}",
        tx.raw_description,
        tx.description.as_deref().unwrap_or_default(),
        tx.merchant_name.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase();
    description.contains("pagamento de fatura")
        || description.contains("pagamento cart")
        || description.contains("pagamento de cart")
        || description.contains("nubank pagamento")
}

fn card_cycle_ref_for(date: NaiveDate, closing_day: Option<u32>) -> Result<String> {
    match closing_day {
        Some(day) if date.day() >= day => Ok(month_ref_for(shift_month(
            NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
                .context("Falha ao calcular mês da transação")?,
            1,
        )?)),
        _ => Ok(month_ref_for(
            NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
                .context("Falha ao calcular mês da transação")?,
        )),
    }
}

fn card_row_from_transaction(tx: &TransactionRecord, month_ref: &str) -> CardClosedTransactionRow {
    CardClosedTransactionRow {
        month_ref: month_ref.to_string(),
        account_id: tx.account_id.clone().unwrap_or_default(),
        transaction_id: tx.transaction_id.clone(),
        transaction_date: tx.transaction_date,
        label: tx.raw_description.clone(),
        description: cashflow_transaction_description(tx),
        amount: tx.amount,
        category_id: tx.category_id.clone(),
        payment_status: tx.payment_status.clone(),
        metadata_json: tx.metadata_json.clone(),
    }
}

fn card_cashflow_item(row: &CardClosedTransactionRow, source: &str) -> CashflowDetailItem {
    CashflowDetailItem {
        description: row.description.clone(),
        amount: row.amount,
        category_id: row.category_id.clone(),
        account_id: Some(row.account_id.clone()),
        transaction_id: Some(row.transaction_id.clone()),
        transaction_date: Some(row.transaction_date),
        source: source.to_string(),
    }
}

fn is_card_side_bill_payment(tx: &TransactionRecord) -> bool {
    tx.amount > Decimal::ZERO
        && tx
            .raw_description
            .to_ascii_lowercase()
            .contains("pagamento recebido")
}

fn cashflow_progress(enabled: bool, message: &str) {
    if enabled && io::stderr().is_terminal() {
        let _ = writeln!(io::stderr(), "{} {message}", dim("cashflow"));
    }
}

fn ansi(code: &str, value: impl AsRef<str>) -> String {
    format!("\x1b[{code}m{}\x1b[0m", value.as_ref())
}

fn green(value: impl AsRef<str>) -> String {
    ansi("32", value)
}

fn red(value: impl AsRef<str>) -> String {
    ansi("31", value)
}

fn yellow(value: impl AsRef<str>) -> String {
    ansi("33", value)
}

fn cyan(value: impl AsRef<str>) -> String {
    ansi("36", value)
}

fn dim(value: impl AsRef<str>) -> String {
    ansi("2", value)
}

fn bold_terminal(value: impl AsRef<str>) -> String {
    ansi("1", value)
}

fn signed_money_color(value: Decimal) -> String {
    let formatted = human_format::brl_signed(value);
    if value < Decimal::ZERO {
        red(formatted)
    } else if value > Decimal::ZERO {
        green(formatted)
    } else {
        formatted
    }
}

fn unsigned_money_color(value: Decimal, positive: bool) -> String {
    let formatted = human_format::brl(value);
    if value == Decimal::ZERO {
        formatted
    } else if positive {
        green(formatted)
    } else {
        red(formatted)
    }
}

fn category_family_key(category_id: &str) -> String {
    human_format::category_family(Some(category_id)).unwrap_or_else(|| "sem-categoria".to_string())
}

fn category_subcategory_key(category_id: &str) -> String {
    category_id
        .split_once(':')
        .map(|(_, sub)| sub.to_string())
        .unwrap_or_else(|| {
            let family = category_family_key(category_id);
            if category_id == family {
                "geral".to_string()
            } else {
                category_id.to_string()
            }
        })
}

fn display_token(value: &str) -> String {
    if value == "geral" {
        "Geral".to_string()
    } else {
        value.replace([':', '-'], " ")
    }
}

fn is_income_category(category_id: Option<&str>) -> bool {
    let family = category_id.and_then(|id| human_format::category_family(Some(id)));
    matches!(family.as_deref(), Some("receitas" | "salario"))
        || category_id.is_some_and(|id| {
            let id = id.to_ascii_lowercase();
            id.contains("receita") || id.contains("income") || id.contains("salary")
        })
}

struct CashflowCardBillContext<'a> {
    store: &'a dyn FinanceStore,
    month_ref: &'a str,
    month_start: NaiveDate,
    month_end: NaiveDate,
    account_types: &'a BTreeMap<String, String>,
    accounts: &'a [AccountRecord],
    internal_categories: &'a BTreeSet<String>,
}

struct CashflowActualParts {
    income_groups: BTreeMap<String, CashflowCategoryGroup>,
    expense_groups: BTreeMap<String, CashflowCategoryGroup>,
    card_payments: Vec<TransactionRecord>,
}

fn collect_cashflow_actual_parts(
    rows: Vec<TransactionRecord>,
    account_types: &BTreeMap<String, String>,
    internal_categories: &BTreeSet<String>,
) -> CashflowActualParts {
    let mut income_groups = BTreeMap::<String, CashflowCategoryGroup>::new();
    let mut expense_groups = BTreeMap::<String, CashflowCategoryGroup>::new();
    let mut card_payments = Vec::<TransactionRecord>::new();

    for tx in rows {
        if is_credit_card_payment_transaction(&tx) {
            card_payments.push(tx);
            continue;
        }
        if skip_cashflow_actual_transaction(&tx, account_types, internal_categories) {
            continue;
        }

        add_cashflow_actual_transaction(&mut income_groups, &mut expense_groups, &tx);
    }

    CashflowActualParts {
        income_groups,
        expense_groups,
        card_payments,
    }
}

fn skip_cashflow_actual_transaction(
    tx: &TransactionRecord,
    account_types: &BTreeMap<String, String>,
    internal_categories: &BTreeSet<String>,
) -> bool {
    cashflow_account_type(tx, account_types) == Some("credit")
        || tx
            .category_id
            .as_deref()
            .is_some_and(|category| internal_categories.contains(category))
}

fn cashflow_account_type<'a>(
    tx: &TransactionRecord,
    account_types: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    tx.account_id
        .as_deref()
        .and_then(|id| account_types.get(id))
        .map(String::as_str)
}

fn add_cashflow_actual_transaction(
    income_groups: &mut BTreeMap<String, CashflowCategoryGroup>,
    expense_groups: &mut BTreeMap<String, CashflowCategoryGroup>,
    tx: &TransactionRecord,
) {
    let category = cashflow_category_id(tx.category_id.as_deref());
    let item = cashflow_item_from_transaction(tx, "transaction");
    if tx.amount >= Decimal::ZERO {
        add_cashflow_group_item(income_groups, category, item);
    } else {
        add_cashflow_group_item(expense_groups, category, item);
    }
}

async fn cashflow_card_bill_rows(
    context: &CashflowCardBillContext<'_>,
) -> Result<BTreeMap<(String, String), Vec<CardClosedTransactionRow>>> {
    let closing_days = cashflow_card_closing_days(context.accounts);
    let window_start = shift_month(context.month_start, -2)?;
    let credit_rows = context
        .store
        .effective_transactions_window(None, window_start, context.month_end)
        .await?;

    let mut bills = BTreeMap::<(String, String), Vec<CardClosedTransactionRow>>::new();
    for tx in credit_rows {
        let Some(account_id) = cashflow_bill_credit_account(context, &tx) else {
            continue;
        };
        let cycle_ref =
            card_cycle_ref_for(tx.transaction_date, closing_days.get(account_id).copied())?;
        if cycle_ref == context.month_ref {
            bills
                .entry((account_id.to_string(), cycle_ref.clone()))
                .or_default()
                .push(card_row_from_transaction(&tx, &cycle_ref));
        }
    }
    Ok(bills)
}

fn cashflow_card_closing_days(accounts: &[AccountRecord]) -> BTreeMap<String, u32> {
    accounts
        .iter()
        .filter_map(|account| {
            parse_closing_day(&account.metadata_json).map(|day| (account.account_id.clone(), day))
        })
        .collect()
}

fn cashflow_bill_credit_account<'a>(
    context: &CashflowCardBillContext<'_>,
    tx: &'a TransactionRecord,
) -> Option<&'a str> {
    let account_id = tx.account_id.as_deref()?;
    if context.account_types.get(account_id).map(String::as_str) != Some("credit") {
        return None;
    }
    if tx
        .category_id
        .as_deref()
        .is_some_and(|category| context.internal_categories.contains(category))
        || is_card_side_bill_payment(tx)
    {
        return None;
    }
    Some(account_id)
}

fn matched_cashflow_card_payment<'a>(
    bill_account_id: &str,
    bill_total: Decimal,
    month_start: NaiveDate,
    card_payments: &'a [TransactionRecord],
    used_payments: &BTreeSet<String>,
    account_owners: &BTreeMap<String, String>,
) -> Option<&'a TransactionRecord> {
    let bill_owner = account_owners
        .get(bill_account_id)
        .map(String::as_str)
        .unwrap_or_default();
    let tolerance = bill_total * Decimal::new(25, 2);
    card_payments
        .iter()
        .filter(|payment| !used_payments.contains(&payment.transaction_id))
        .filter(|payment| {
            let Some(payment_account_id) = payment.account_id.as_deref() else {
                return bill_owner.is_empty();
            };
            let payment_owner = account_owners
                .get(payment_account_id)
                .map(String::as_str)
                .unwrap_or_default();
            bill_owner.is_empty() || payment_owner.is_empty() || bill_owner == payment_owner
        })
        .filter(|payment| {
            let paid = payment.amount.abs();
            paid >= bill_total - tolerance && paid <= bill_total + tolerance
        })
        .min_by_key(|payment| {
            (payment.transaction_date - month_start)
                .num_days()
                .unsigned_abs()
        })
}

fn cashflow_card_bill_detail(
    account_id: String,
    rows: Vec<CardClosedTransactionRow>,
    payment: &TransactionRecord,
) -> CashflowCardBillDetail {
    let mut installment_items = Vec::new();
    let mut subscription_items = Vec::new();
    let mut other_groups = BTreeMap::<String, CashflowCategoryGroup>::new();

    for row in &rows {
        if detect_installment_marker(row).is_some() {
            installment_items.push(card_cashflow_item(row, "card_installment"));
        } else if is_subscription_row(row) {
            subscription_items.push(card_cashflow_item(row, "card_subscription"));
        } else {
            add_cashflow_group_item(
                &mut other_groups,
                cashflow_category_id(row.category_id.as_deref()),
                card_cashflow_item(row, "card_category"),
            );
        }
    }

    CashflowCardBillDetail {
        account_id,
        payment_transaction_id: Some(payment.transaction_id.clone()),
        payment_date: Some(payment.transaction_date),
        paid_amount: payment.amount.abs(),
        bill_total: -rows.iter().map(|row| row.amount).sum::<Decimal>(),
        installments: CashflowCategoryGroup {
            category_id: "compras-parceladas".to_string(),
            subtotal: installment_items.iter().map(|item| item.amount).sum(),
            transactions: installment_items.len(),
            items: installment_items,
        },
        subscriptions: CashflowCategoryGroup {
            category_id: "assinaturas".to_string(),
            subtotal: subscription_items.iter().map(|item| item.amount).sum(),
            transactions: subscription_items.len(),
            items: subscription_items,
        },
        other_categories: sorted_cashflow_groups(other_groups),
    }
}

async fn cashflow_card_bill_details(
    context: CashflowCardBillContext<'_>,
    card_payments: &[TransactionRecord],
) -> Result<Vec<CashflowCardBillDetail>> {
    if card_payments.is_empty() {
        return Ok(Vec::new());
    }

    let bills = cashflow_card_bill_rows(&context).await?;
    let account_owners = context
        .accounts
        .iter()
        .map(|account| (account.account_id.clone(), account.owner.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut used_payments = BTreeSet::<String>::new();
    let mut out = Vec::new();
    for ((account_id, _cycle), rows) in bills {
        let bill_total = -rows.iter().map(|row| row.amount).sum::<Decimal>();
        if bill_total <= Decimal::ZERO {
            continue;
        }
        let Some(payment) = matched_cashflow_card_payment(
            &account_id,
            bill_total,
            context.month_start,
            card_payments,
            &used_payments,
            &account_owners,
        ) else {
            continue;
        };
        used_payments.insert(payment.transaction_id.clone());
        out.push(cashflow_card_bill_detail(account_id, rows, payment));
    }

    out.sort_by(|a, b| {
        b.bill_total
            .cmp(&a.bill_total)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    Ok(out)
}

fn card_bill_expense_groups(card_bills: &[CashflowCardBillDetail]) -> Vec<CashflowCategoryGroup> {
    let mut groups = BTreeMap::<String, CashflowCategoryGroup>::new();
    for bill in card_bills {
        add_card_bill_itemized_groups(&mut groups, bill);
        add_card_bill_category_groups(&mut groups, bill);
    }
    sorted_cashflow_groups(groups)
}

fn add_card_bill_itemized_groups(
    groups: &mut BTreeMap<String, CashflowCategoryGroup>,
    bill: &CashflowCardBillDetail,
) {
    for group in [&bill.installments, &bill.subscriptions] {
        if group.transactions == 0 {
            continue;
        }
        for item in &group.items {
            add_cashflow_group_item(groups, group.category_id.clone(), item.clone());
        }
    }
}

fn add_card_bill_category_groups(
    groups: &mut BTreeMap<String, CashflowCategoryGroup>,
    bill: &CashflowCardBillDetail,
) {
    for group in &bill.other_categories {
        let entry =
            groups
                .entry(group.category_id.clone())
                .or_insert_with(|| CashflowCategoryGroup {
                    category_id: group.category_id.clone(),
                    subtotal: Decimal::ZERO,
                    transactions: 0,
                    items: Vec::new(),
                });
        entry.subtotal += group.subtotal;
        entry.transactions += group.transactions;
    }
}

fn installment_marker_is_final(marker: &str) -> bool {
    let Some((left, right)) = marker.split_once('/') else {
        return false;
    };
    let Ok(current) = left.parse::<u32>() else {
        return false;
    };
    let Ok(total) = right.parse::<u32>() else {
        return false;
    };
    current > 0 && current == total
}

fn cashflow_card_summary(card_bills: &[CashflowCardBillDetail]) -> CashflowCardSummary {
    CashflowCardSummary {
        total: cashflow_card_summary_total(card_bills),
        paid_on: cashflow_card_paid_dates(card_bills),
        installments_total: card_bills
            .iter()
            .map(|bill| bill.installments.subtotal.abs())
            .sum(),
        installment_transactions: card_bills
            .iter()
            .map(|bill| bill.installments.transactions)
            .sum(),
        installments_released_this_month: cashflow_installments_released_this_month(card_bills),
    }
}

fn cashflow_card_paid_dates(card_bills: &[CashflowCardBillDetail]) -> Vec<NaiveDate> {
    card_bills
        .iter()
        .filter_map(|bill| bill.payment_date)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn cashflow_installments_released_this_month(card_bills: &[CashflowCardBillDetail]) -> Decimal {
    card_bills
        .iter()
        .flat_map(|bill| bill.installments.items.iter())
        .filter(|item| cashflow_installment_item_is_final(item))
        .map(|item| item.amount.abs())
        .sum()
}

fn cashflow_installment_item_is_final(item: &CashflowDetailItem) -> bool {
    item.description.split_whitespace().any(|token| {
        installment_marker_is_final(token.trim_matches(|c: char| !c.is_ascii_digit() && c != '/'))
    })
}

fn cashflow_card_summary_total(card_bills: &[CashflowCardBillDetail]) -> Decimal {
    let paid_total: Decimal = card_bills
        .iter()
        .map(|bill| bill.paid_amount)
        .filter(|amount| *amount > Decimal::ZERO)
        .sum();
    if paid_total > Decimal::ZERO {
        paid_total
    } else {
        card_bills.iter().map(|bill| bill.bill_total).sum()
    }
}

fn cashflow_account_balances(
    accounts: &[AccountRecord],
    snapshots: &[phai_core::models::AccountSnapshotRecord],
) -> Vec<CashflowAccountBalance> {
    let account_by_id: BTreeMap<&str, &AccountRecord> = accounts
        .iter()
        .filter(|account| is_checking_account_type(&account.account_type))
        .map(|account| (account.account_id.as_str(), account))
        .collect();
    let mut balances = snapshots
        .iter()
        .filter_map(|snapshot| {
            let account = account_by_id.get(snapshot.account_id.as_str())?;
            Some(CashflowAccountBalance {
                account_id: account.account_id.clone(),
                owner: account.owner.clone(),
                label: if account.label.is_empty() {
                    account.account_id.clone()
                } else {
                    account.label.clone()
                },
                balance: snapshot.balance,
                snapshot_date: snapshot.snapshot_date,
            })
        })
        .collect::<Vec<_>>();
    balances.sort_by(|a, b| {
        a.owner
            .cmp(&b.owner)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| a.account_id.cmp(&b.account_id))
    });
    balances
}

fn empty_cashflow_card_summary() -> CashflowCardSummary {
    CashflowCardSummary {
        total: Decimal::ZERO,
        paid_on: Vec::new(),
        installments_total: Decimal::ZERO,
        installment_transactions: 0,
        installments_released_this_month: Decimal::ZERO,
    }
}

async fn cashflow_forecast_groups(
    store: &dyn FinanceStore,
    month_ref: &str,
    include_forecast: bool,
) -> Result<Vec<CashflowCategoryGroup>> {
    if !include_forecast {
        return Ok(Vec::new());
    }

    let mut groups = BTreeMap::<String, CashflowCategoryGroup>::new();
    for row in store.forecast_vs_actual(Some(month_ref)).await? {
        let Some((category, item)) = cashflow_forecast_group_item(row) else {
            continue;
        };
        add_cashflow_group_item(&mut groups, category, item);
    }
    Ok(sorted_cashflow_groups(groups))
}

fn cashflow_forecast_group_item(row: ForecastVsActualRow) -> Option<(String, CashflowDetailItem)> {
    if inactive_forecast_status(&row.status) {
        return None;
    }
    let remaining = row.forecast_amount.abs() - row.actual_amount.abs();
    if remaining <= Decimal::ZERO {
        return None;
    }
    let category = cashflow_category_id(row.category_id.as_deref());
    let amount = signed_forecast_remaining(row.category_id.as_deref(), remaining);
    Some((
        category,
        CashflowDetailItem {
            description: row.description,
            amount,
            category_id: row.category_id,
            account_id: row.account_id,
            transaction_id: Some(row.forecast_id),
            transaction_date: row.due_date,
            source: "forecast".to_string(),
        },
    ))
}

fn inactive_forecast_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_lowercase().as_str(),
        "cancelled" | "canceled" | "inactive" | "deleted"
    )
}

fn signed_forecast_remaining(category_id: Option<&str>, remaining: Decimal) -> Decimal {
    if is_income_category(category_id) {
        remaining
    } else {
        -remaining
    }
}

async fn build_cashflow_month_report(
    store: &dyn FinanceStore,
    month_ref: &str,
    include_details: bool,
    include_forecast: bool,
    actual_summary: CashflowRow,
) -> Result<CashflowMonthReport> {
    let accounts = store.get_accounts().await?;
    let snapshots = store.latest_account_snapshots().await?;
    let details =
        cashflow_details_for_report(store, month_ref, include_details, include_forecast).await?;
    let forecast_summary = cashflow_report_forecast_summary(month_ref, include_forecast, &details);
    let summary = cashflow_report_summary(month_ref, &actual_summary, forecast_summary.as_ref());
    let previous_details = cashflow_previous_details(store, month_ref, include_details).await?;
    let card_summary = details
        .as_ref()
        .map(|d| cashflow_card_summary(&d.card_bills))
        .unwrap_or_else(empty_cashflow_card_summary);

    Ok(CashflowMonthReport {
        month_ref: month_ref.to_string(),
        actual_summary,
        forecast_summary,
        summary,
        account_balances: cashflow_account_balances(&accounts, &snapshots),
        previous_details,
        card_summary,
        details: include_details.then_some(details).flatten(),
    })
}

async fn cashflow_details_for_report(
    store: &dyn FinanceStore,
    month_ref: &str,
    include_details: bool,
    include_forecast: bool,
) -> Result<Option<CashflowDetailSections>> {
    if include_details || include_forecast {
        Ok(Some(
            build_cashflow_detail_sections(store, month_ref, include_forecast).await?,
        ))
    } else {
        Ok(None)
    }
}

fn cashflow_report_forecast_summary(
    month_ref: &str,
    include_forecast: bool,
    details: &Option<CashflowDetailSections>,
) -> Option<CashflowRow> {
    let forecast = details
        .as_ref()
        .map(|d| d.forecast.as_slice())
        .unwrap_or(&[]);
    include_forecast.then(|| summarize_forecast_groups(month_ref, forecast))
}

fn cashflow_report_summary(
    month_ref: &str,
    actual_summary: &CashflowRow,
    forecast_summary: Option<&CashflowRow>,
) -> CashflowRow {
    forecast_summary
        .map(|forecast_row| combine_cashflow_rows(month_ref, actual_summary, forecast_row))
        .unwrap_or_else(|| actual_summary.clone())
}

async fn cashflow_previous_details(
    store: &dyn FinanceStore,
    month_ref: &str,
    include_details: bool,
) -> Result<Option<CashflowDetailSections>> {
    if !include_details {
        return Ok(None);
    }
    let previous_month = month_ref_for(shift_month(parse_month_ref(month_ref)?, -1)?);
    Ok(Some(
        build_cashflow_detail_sections(store, &previous_month, false).await?,
    ))
}

async fn build_cashflow_detail_sections(
    store: &dyn FinanceStore,
    month_ref: &str,
    include_forecast: bool,
) -> Result<CashflowDetailSections> {
    let (month_start, month_end) = month_bounds(month_ref)?;
    let accounts = store.get_accounts().await?;
    let account_types = accounts
        .iter()
        .map(|account| (account.account_id.clone(), account.account_type.clone()))
        .collect::<BTreeMap<_, _>>();
    let internal_categories = store.internal_categories().await?;
    let actual_rows = store
        .effective_transactions_window(None, month_start, month_end)
        .await?;
    let mut actual_parts =
        collect_cashflow_actual_parts(actual_rows, &account_types, &internal_categories);

    let card_bills = cashflow_card_bill_details(
        CashflowCardBillContext {
            store,
            month_ref,
            month_start,
            month_end,
            account_types: &account_types,
            accounts: &accounts,
            internal_categories: &internal_categories,
        },
        &actual_parts.card_payments,
    )
    .await?;
    merge_card_bill_expense_groups(&mut actual_parts, &card_bills);

    let income = sorted_cashflow_groups(actual_parts.income_groups);
    let expenses = sorted_cashflow_groups(actual_parts.expense_groups);
    let forecast = cashflow_forecast_groups(store, month_ref, include_forecast).await?;

    Ok(CashflowDetailSections {
        income,
        expenses,
        card_bills,
        forecast,
    })
}

fn merge_card_bill_expense_groups(
    actual_parts: &mut CashflowActualParts,
    card_bills: &[CashflowCardBillDetail],
) {
    for group in card_bill_expense_groups(card_bills) {
        let entry = actual_parts
            .expense_groups
            .entry(group.category_id.clone())
            .or_insert_with(|| CashflowCategoryGroup {
                category_id: group.category_id.clone(),
                subtotal: Decimal::ZERO,
                transactions: 0,
                items: Vec::new(),
            });
        entry.subtotal += group.subtotal;
        entry.transactions += group.transactions;
        entry.items.extend(group.items);
    }
}

async fn report_cashflow(args: CashflowArgs, format: ReportOutputFormat) -> Result<()> {
    let store = migrated_finance_store().await?;

    // Default to the current month so a bare `finance report cashflow`
    // answers "como tô agora?" without arguments.
    let today = chrono::Local::now().date_naive();
    let month_ref = cashflow_month_arg(&args, today)?;
    let effective_details = cashflow_effective_details(&args);
    let progress = cashflow_should_show_progress(format, &args, effective_details);

    let (row, accounts_considered, snapshot_anchor) =
        cashflow_basic_summary(store.as_ref(), &month_ref, today, progress).await?;

    if cashflow_uses_month_report(&args, effective_details) {
        return report_cashflow_detailed(
            store.as_ref(),
            &args,
            format,
            &month_ref,
            effective_details,
            progress,
            row,
        )
        .await;
    }

    render_basic_cashflow(format, &row, accounts_considered, snapshot_anchor)?;
    Ok(())
}

fn cashflow_effective_details(args: &CashflowArgs) -> bool {
    args.details
}

fn cashflow_uses_month_report(args: &CashflowArgs, effective_details: bool) -> bool {
    effective_details || args.forecast
}

fn cashflow_should_show_progress(
    format: ReportOutputFormat,
    args: &CashflowArgs,
    effective_details: bool,
) -> bool {
    !format.structured_output() && cashflow_uses_month_report(args, effective_details)
}

async fn migrated_finance_store() -> Result<Box<dyn FinanceStore>> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    Ok(store)
}

fn cashflow_month_arg(args: &CashflowArgs, today: NaiveDate) -> Result<String> {
    let month_ref = args.month.clone().unwrap_or_else(|| month_ref_for(today));
    parse_month_ref(&month_ref)?;
    Ok(month_ref)
}

fn render_basic_cashflow(
    format: ReportOutputFormat,
    row: &CashflowRow,
    accounts_considered: usize,
    snapshot_anchor: Option<NaiveDate>,
) -> Result<()> {
    if format.structured_output() {
        print_cashflow_summary_output(format, row, accounts_considered, snapshot_anchor)
    } else {
        print_cashflow_human(row, accounts_considered, snapshot_anchor);
        Ok(())
    }
}

async fn cashflow_basic_summary(
    store: &dyn FinanceStore,
    month_ref: &str,
    today: NaiveDate,
    progress: bool,
) -> Result<(CashflowRow, usize, Option<NaiveDate>)> {
    cashflow_progress(progress, "calculando resumo cash-basis");
    let row = store.cashflow_month(month_ref).await?;
    cashflow_progress(progress, "carregando saldos das contas correntes");
    let today_balance = store.checking_balance_at(today).await?;
    let snapshot_anchor = today_balance.as_ref().and_then(|b| b.snapshot_anchor_date);
    let accounts_considered = today_balance
        .as_ref()
        .map(|b| b.accounts_considered)
        .unwrap_or(0);

    Ok((row, accounts_considered, snapshot_anchor))
}

async fn report_cashflow_detailed(
    store: &dyn FinanceStore,
    args: &CashflowArgs,
    format: ReportOutputFormat,
    month_ref: &str,
    effective_details: bool,
    progress: bool,
    row: CashflowRow,
) -> Result<()> {
    cashflow_progress(progress, "agrupando conta corrente, cartões e forecast");
    let report =
        build_cashflow_month_report(store, month_ref, effective_details, args.forecast, row)
            .await?;

    if format.structured_output() {
        print_report_output(format, &report)?;
        return Ok(());
    }

    cashflow_progress(progress, "renderizando visão de terminal");
    print_cashflow_month_human(&report, effective_details, args.forecast);
    Ok(())
}

fn print_cashflow_summary_output(
    format: ReportOutputFormat,
    row: &CashflowRow,
    accounts_considered: usize,
    snapshot_anchor: Option<NaiveDate>,
) -> Result<()> {
    let payload = serde_json::json!({
        "month_ref": row.month_ref,
        "opening_balance": row.opening_balance.as_ref().map(|d| d.to_string()),
        "inflows": row.income.to_string(),
        "outflows": row.expenses.to_string(),
        "expense_reduction": row.expense_reduction.to_string(),
        "closing_balance": row.closing_balance.as_ref().map(|d| d.to_string()),
        "net": row.net.to_string(),
        "accounts_considered": accounts_considered,
        "snapshot_anchor_date": snapshot_anchor,
        "snapshot_complete": row.opening_balance.is_some() && row.closing_balance.is_some(),
    });
    print_report_output(format, &payload)
}

#[derive(Default)]
struct CashflowTerminalSubcategory {
    family: String,
    subcategory: String,
    actual: Decimal,
    forecast: Decimal,
    previous: Decimal,
    transactions: usize,
    forecast_names: BTreeSet<String>,
}

struct CashflowTerminalFamily {
    family: String,
    total: Decimal,
    forecast: Decimal,
    previous: Decimal,
    subcategories: Vec<CashflowTerminalSubcategory>,
}

fn add_terminal_group<'a>(
    map: &'a mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    group: &CashflowCategoryGroup,
) -> &'a mut CashflowTerminalSubcategory {
    let family = category_family_key(&group.category_id);
    let subcategory = category_subcategory_key(&group.category_id);
    map.entry((family.clone(), subcategory.clone()))
        .or_insert_with(|| CashflowTerminalSubcategory {
            family,
            subcategory,
            ..CashflowTerminalSubcategory::default()
        })
}

fn add_terminal_actual_group(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    group: &CashflowCategoryGroup,
) {
    let entry = add_terminal_group(map, group);
    entry.actual += group.subtotal;
    entry.transactions += group.transactions;
}

fn add_terminal_forecast_group(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    group: &CashflowCategoryGroup,
) {
    let entry = add_terminal_group(map, group);
    entry.forecast += group.subtotal;
    entry.forecast_names.extend(group.items.iter().map(|item| {
        human_format::truncate_with_ellipsis(
            &human_format::short_description(&item.description),
            24,
        )
    }));
}

fn add_terminal_previous_group(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    group: &CashflowCategoryGroup,
) {
    add_terminal_group(map, group).previous += group.subtotal;
}

fn terminal_families_for(
    details: &CashflowDetailSections,
    previous: Option<&CashflowDetailSections>,
    income: bool,
) -> Vec<CashflowTerminalFamily> {
    let map = terminal_subcategory_map(details, previous, income);
    let mut families = terminal_family_groups(map)
        .into_iter()
        .map(|(family, mut subcategories)| {
            sort_terminal_subcategories(&mut subcategories);
            cashflow_terminal_family(family, subcategories)
        })
        .collect::<Vec<_>>();
    families.sort_by(|a, b| {
        b.total
            .abs()
            .cmp(&a.total.abs())
            .then_with(|| a.family.cmp(&b.family))
    });
    families
}

fn terminal_subcategory_map(
    details: &CashflowDetailSections,
    previous: Option<&CashflowDetailSections>,
    income: bool,
) -> BTreeMap<(String, String), CashflowTerminalSubcategory> {
    let mut map = BTreeMap::<(String, String), CashflowTerminalSubcategory>::new();
    add_terminal_actual_groups(&mut map, details, income);
    add_terminal_forecast_groups(&mut map, details, income);
    add_terminal_previous_groups(&mut map, previous, income);
    map
}

fn terminal_actual_groups(
    details: &CashflowDetailSections,
    income: bool,
) -> &[CashflowCategoryGroup] {
    if income {
        details.income.as_slice()
    } else {
        details.expenses.as_slice()
    }
}

fn add_terminal_actual_groups(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    details: &CashflowDetailSections,
    income: bool,
) {
    for group in terminal_actual_groups(details, income) {
        add_terminal_actual_group(map, group);
    }
}

fn add_terminal_forecast_groups(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    details: &CashflowDetailSections,
    income: bool,
) {
    for group in details
        .forecast
        .iter()
        .filter(|group| (group.subtotal >= Decimal::ZERO) == income)
    {
        add_terminal_forecast_group(map, group);
    }
}

fn add_terminal_previous_groups(
    map: &mut BTreeMap<(String, String), CashflowTerminalSubcategory>,
    previous: Option<&CashflowDetailSections>,
    income: bool,
) {
    let Some(previous) = previous else {
        return;
    };
    for group in terminal_actual_groups(previous, income) {
        add_terminal_previous_group(map, group);
    }
}

fn terminal_family_groups(
    map: BTreeMap<(String, String), CashflowTerminalSubcategory>,
) -> BTreeMap<String, Vec<CashflowTerminalSubcategory>> {
    let mut by_family = BTreeMap::<String, Vec<CashflowTerminalSubcategory>>::new();
    for subcategory in map.into_values() {
        if terminal_subcategory_is_empty(&subcategory) {
            continue;
        }
        by_family
            .entry(subcategory.family.clone())
            .or_default()
            .push(subcategory);
    }
    by_family
}

fn terminal_subcategory_is_empty(subcategory: &CashflowTerminalSubcategory) -> bool {
    subcategory.actual == Decimal::ZERO
        && subcategory.forecast == Decimal::ZERO
        && subcategory.previous == Decimal::ZERO
}

fn sort_terminal_subcategories(subcategories: &mut [CashflowTerminalSubcategory]) {
    subcategories.sort_by(|a, b| {
        (b.actual + b.forecast)
            .abs()
            .cmp(&(a.actual + a.forecast).abs())
            .then_with(|| a.subcategory.cmp(&b.subcategory))
    });
}

fn cashflow_terminal_family(
    family: String,
    subcategories: Vec<CashflowTerminalSubcategory>,
) -> CashflowTerminalFamily {
    CashflowTerminalFamily {
        total: subcategories
            .iter()
            .map(|subcategory| subcategory.actual + subcategory.forecast)
            .sum(),
        forecast: subcategories
            .iter()
            .map(|subcategory| subcategory.forecast)
            .sum(),
        previous: subcategories
            .iter()
            .map(|subcategory| subcategory.previous)
            .sum(),
        family,
        subcategories,
    }
}

fn change_label(current: Decimal, previous: Decimal) -> String {
    let current_abs = current.abs();
    let previous_abs = previous.abs();
    if previous_abs == Decimal::ZERO {
        return if current_abs == Decimal::ZERO {
            dim("0%")
        } else {
            cyan("novo")
        };
    }
    let delta_pct = (current_abs - previous_abs) / previous_abs * Decimal::from(100u32);
    let rounded = delta_pct.round_dp(0);
    let label = format!("{rounded:+}%");
    if delta_pct > Decimal::ZERO {
        red(label)
    } else if delta_pct < Decimal::ZERO {
        green(label)
    } else {
        dim(label)
    }
}

fn share_pct(amount: Decimal, total: Decimal) -> i64 {
    if total == Decimal::ZERO {
        return 0;
    }
    (amount.abs() / total.abs() * Decimal::from(100u32))
        .round_dp(0)
        .to_string()
        .parse::<i64>()
        .unwrap_or(0)
}

fn print_terminal_family_section(title: &str, families: &[CashflowTerminalFamily]) {
    if families.is_empty() {
        return;
    }
    println!();
    println!("{}", bold_terminal(title));
    for family in families {
        print_terminal_family(family);
    }
}

fn print_terminal_family(family: &CashflowTerminalFamily) {
    let emoji = human_format::category_emoji(Some(&family.family), Some(family.total));
    println!(
        "\n{} {}  {}{}  {}",
        emoji,
        bold_terminal(human_format::family_label(&family.family)),
        signed_money_color(family.total),
        terminal_forecast_label(family.forecast),
        dim(format!(
            "vs mês ant. {}",
            change_label(family.total, family.previous)
        ))
    );
    for subcategory in &family.subcategories {
        print_terminal_subcategory(family, subcategory);
    }
}

fn terminal_forecast_label(amount: Decimal) -> String {
    if amount == Decimal::ZERO {
        String::new()
    } else {
        format!(
            " {}",
            yellow(format!("({})", human_format::brl_signed(amount)))
        )
    }
}

fn print_terminal_subcategory(
    family: &CashflowTerminalFamily,
    subcategory: &CashflowTerminalSubcategory,
) {
    let total = subcategory.actual + subcategory.forecast;
    println!(
        "  {:<24} {:>18}{}  {:>10}  {}",
        display_token(&subcategory.subcategory),
        signed_money_color(total),
        terminal_forecast_label(subcategory.forecast),
        change_label(total, subcategory.previous),
        human_format::progress_bar(share_pct(total, family.total))
    );
    print_terminal_forecast_names(subcategory);
}

fn print_terminal_forecast_names(subcategory: &CashflowTerminalSubcategory) {
    if subcategory.forecast_names.is_empty() {
        return;
    }
    let names = subcategory
        .forecast_names
        .iter()
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    println!("    {} {}", yellow("forecast:"), dim(names));
}

fn print_cashflow_month_human(
    report: &CashflowMonthReport,
    show_details: bool,
    include_forecast: bool,
) {
    print_cashflow_month_header(report, show_details, include_forecast);
    print_cashflow_account_balances(report);
    let Some(details) = &report.details else {
        return;
    };
    print_cashflow_detail_sections(report, details);
    print_cashflow_card_summary(report);
}

fn print_cashflow_month_header(
    report: &CashflowMonthReport,
    show_details: bool,
    include_forecast: bool,
) {
    use human_format::month_label;

    let suffix = cashflow_header_suffix(show_details, include_forecast);
    println!(
        "{}",
        bold_terminal(format!(
            "💵 Cashflow · {}{}",
            month_label(&report.month_ref),
            suffix
        ))
    );
    println!();

    let opening = optional_balance_label(report.summary.opening_balance);
    let closing = optional_balance_label(report.summary.closing_balance);
    println!(
        "  {:<15} {}    {:<15} {}",
        "Saldo inicial", opening, "Saldo final", closing
    );
    print_cashflow_actual_header(report);
    print_cashflow_forecast_header(report);
}

fn cashflow_header_suffix(show_details: bool, include_forecast: bool) -> &'static str {
    [
        (show_details && include_forecast, " · detalhes + forecast"),
        (show_details, " · detalhes"),
        (include_forecast, " · forecast"),
    ]
    .into_iter()
    .find_map(|(enabled, suffix)| enabled.then_some(suffix))
    .unwrap_or("")
}

fn optional_balance_label(amount: Option<Decimal>) -> String {
    amount
        .map(human_format::brl)
        .unwrap_or_else(|| "—".to_string())
}

fn print_cashflow_actual_header(report: &CashflowMonthReport) {
    println!(
        "  {:<15} {}    {:<15} {}    {:<15} {}",
        "Entradas",
        unsigned_money_color(report.actual_summary.income, true),
        "Saídas",
        unsigned_money_color(report.actual_summary.expenses, false),
        "Resultado",
        signed_money_color(report.actual_summary.net)
    );
}

fn print_cashflow_forecast_header(report: &CashflowMonthReport) {
    if let Some(forecast) = &report.forecast_summary {
        println!(
            "  {:<15} {}    {:<15} {}    {:<15} {}",
            "Forecast +",
            unsigned_money_color(forecast.income, true),
            "Forecast -",
            unsigned_money_color(forecast.expenses, false),
            "Projetado",
            signed_money_color(report.summary.net)
        );
    }
}

fn print_cashflow_account_balances(report: &CashflowMonthReport) {
    if report.account_balances.is_empty() {
        return;
    }
    println!();
    println!("{}", dim("Contas correntes sincronizadas"));
    for balance in &report.account_balances {
        let amount = balance
            .balance
            .map(signed_money_color)
            .unwrap_or_else(|| "—".to_string());
        println!(
            "  {}  {}  {}",
            balance.label,
            amount,
            dim(human_format::short_date(balance.snapshot_date))
        );
    }
}

fn print_cashflow_detail_sections(report: &CashflowMonthReport, details: &CashflowDetailSections) {
    let income_families = terminal_families_for(details, report.previous_details.as_ref(), true);
    let expense_families = terminal_families_for(details, report.previous_details.as_ref(), false);

    print_terminal_family_section("Entradas por categoria", &income_families);
    print_terminal_family_section("Saídas por categoria", &expense_families);
}

fn print_cashflow_card_summary(report: &CashflowMonthReport) {
    use human_format::brl;

    let paid = cashflow_card_paid_label(&report.card_summary);
    println!();
    println!("{}", bold_terminal("Cartões"));
    println!(
        "  total de cartão de crédito: {} {}",
        red(brl(report.card_summary.total)),
        dim(paid)
    );
    println!(
        "  total de compra parcelada: {} {}",
        red(brl(report.card_summary.installments_total)),
        dim(format!(
            "({} compras, liberou este mês {})",
            report.card_summary.installment_transactions,
            brl(report.card_summary.installments_released_this_month)
        ))
    );
}

fn cashflow_card_paid_label(summary: &CashflowCardSummary) -> String {
    if summary.total <= Decimal::ZERO {
        return "sem fatura paga no período".to_string();
    }
    if summary.paid_on.is_empty() {
        return "pagamento não encontrado".to_string();
    }
    format!(
        "pago em {}",
        summary
            .paid_on
            .iter()
            .map(|date| human_format::short_date(*date))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn print_cashflow_human(
    row: &phai_core::models::CashflowRow,
    accounts_considered: usize,
    snapshot_anchor: Option<chrono::NaiveDate>,
) {
    use human_format::{bold, brl, month_label};

    println!(
        "💵 {}",
        bold(&format!("Caixa · {}", month_label(&row.month_ref)))
    );
    println!();

    println!(
        "  Saldo inicial   {}",
        optional_balance_label(row.opening_balance)
    );
    println!("  Entradas      + {}", brl(row.income));
    println!("  Saídas        − {}", brl(row.expenses));
    print_cashflow_reduction(row.expense_reduction);
    println!("  ─────────────────────────────");

    print_cashflow_closing_balance(row);
    println!();
    print_cashflow_snapshot_label(accounts_considered, snapshot_anchor);
}

fn print_cashflow_reduction(expense_reduction: Decimal) {
    if expense_reduction > Decimal::ZERO {
        println!("  Cashback      + {}", human_format::brl(expense_reduction));
    }
}

fn print_cashflow_closing_balance(row: &phai_core::models::CashflowRow) {
    use human_format::{brl, brl_signed};

    match (row.opening_balance, row.closing_balance) {
        (Some(open), Some(close)) => {
            let delta = close - open;
            println!(
                "  Saldo final     {}   (Δ {})",
                brl(close),
                brl_signed(delta),
            );
        }
        _ => println!(
            "  Saldo final     {}",
            optional_balance_label(row.closing_balance)
        ),
    }
}

fn print_cashflow_snapshot_label(
    accounts_considered: usize,
    snapshot_anchor: Option<chrono::NaiveDate>,
) {
    let accounts_label = cashflow_accounts_label(accounts_considered);
    match snapshot_anchor {
        Some(date) => println!(
            "  _{accounts_label} · âncora: snapshot {}_",
            human_format::short_date(date)
        ),
        None => println!("  _{accounts_label} · snapshot incompleto: rode `finance sync pluggy`_"),
    }
}

fn cashflow_accounts_label(accounts_considered: usize) -> String {
    if accounts_considered == 1 {
        "1 conta corrente".to_string()
    } else {
        format!("{accounts_considered} contas correntes")
    }
}

async fn report_forecast_vs_actual(
    args: ForecastVsActualArgs,
    format: ReportOutputFormat,
) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.forecast_vs_actual(args.month.as_deref()).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    print_forecast_vs_actual_human(&rows, args.month.as_deref());
    Ok(())
}

fn print_forecast_vs_actual_human(
    rows: &[phai_core::models::ForecastVsActualRow],
    month: Option<&str>,
) {
    use human_format::bold;

    let month_display = forecast_month_display(rows, month);
    println!(
        "📊 {}",
        bold(&format!("Previsto vs Realizado · {month_display}"))
    );

    if rows.is_empty() {
        return;
    }

    let family_rows = forecast_rows_by_family(rows);
    let families = sorted_forecast_families(&family_rows);

    println!();
    for family in &families {
        print_forecast_family(family, &family_rows[family]);
    }

    print_forecast_totals(rows);
}

fn forecast_month_display(
    rows: &[phai_core::models::ForecastVsActualRow],
    month: Option<&str>,
) -> String {
    month
        .map(month_label)
        .or_else(|| rows.first().map(|r| month_label(&r.month_ref)))
        .unwrap_or_else(|| "—".to_string())
}

fn forecast_rows_by_family(
    rows: &[phai_core::models::ForecastVsActualRow],
) -> std::collections::HashMap<String, Vec<&phai_core::models::ForecastVsActualRow>> {
    let mut family_rows = std::collections::HashMap::new();
    for row in rows {
        let family = human_format::category_family(row.category_id.as_deref())
            .unwrap_or_else(|| "outros".to_string());
        family_rows.entry(family).or_insert_with(Vec::new).push(row);
    }
    family_rows
}

fn sorted_forecast_families(
    family_rows: &std::collections::HashMap<String, Vec<&phai_core::models::ForecastVsActualRow>>,
) -> Vec<String> {
    use std::cmp::Reverse;

    let mut families: Vec<String> = family_rows.keys().cloned().collect();
    families.sort_by_key(|f| {
        Reverse(
            family_rows[f]
                .iter()
                .map(|r| r.variance.abs())
                .fold(Decimal::ZERO, |acc, v| acc + v),
        )
    });
    families
}

fn print_forecast_family(family: &str, rows: &[&phai_core::models::ForecastVsActualRow]) {
    let emoji = category_emoji(Some(family), None);
    let label = human_format::family_label(family);
    println!("{emoji} *{}*", label);

    for row in sorted_forecast_family_rows(rows) {
        print_forecast_row(row);
    }
}

fn sorted_forecast_family_rows<'a>(
    rows: &'a [&phai_core::models::ForecastVsActualRow],
) -> Vec<&'a phai_core::models::ForecastVsActualRow> {
    use std::cmp::Reverse;

    let mut sorted = rows.to_vec();
    sorted.sort_by_key(|r| Reverse(r.variance.abs()));
    sorted
}

fn print_forecast_row(row: &phai_core::models::ForecastVsActualRow) {
    let forecast = -row.forecast_amount;
    let actual = -row.actual_amount;
    let variance = -row.variance;
    println!(
        "  {} {} {}  previsto {}  realizado {}  variação {}",
        human_format::short_description(&row.description),
        forecast_due_label(row.due_date),
        forecast_indicator(forecast, actual, variance),
        hf_brl(forecast),
        hf_brl(actual),
        human_format::brl_signed(variance),
    );
}

fn forecast_indicator(forecast: Decimal, actual: Decimal, variance: Decimal) -> &'static str {
    let near_zero = variance.abs() < Decimal::new(1, 2);
    if !near_zero && variance > Decimal::ZERO {
        "🔻"
    } else if near_zero {
        "✅"
    } else if actual.abs() > forecast.abs() * Decimal::from(80u32) / Decimal::from(100u32) {
        "⚠️"
    } else {
        "✅"
    }
}

fn forecast_due_label(due_date: Option<NaiveDate>) -> String {
    due_date
        .map(|date| format!("({})", date.format("%d/%m")))
        .unwrap_or_default()
}

fn print_forecast_totals(rows: &[phai_core::models::ForecastVsActualRow]) {
    let total_forecast: Decimal = rows.iter().map(|r| -r.forecast_amount).sum();
    let total_actual: Decimal = rows.iter().map(|r| -r.actual_amount).sum();
    let total_variance: Decimal = rows.iter().map(|r| -r.variance).sum();

    println!();
    println!(
        "*Total*  previsto {}  realizado {}  variação {}",
        hf_brl(total_forecast),
        hf_brl(total_actual),
        human_format::brl_signed(total_variance)
    );
}

async fn report_card_summary(args: CardSummaryArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.card_summary(args.month.as_deref()).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    let month_label_str = args
        .month
        .as_deref()
        .map(month_label)
        .unwrap_or_else(|| "—".to_string());
    println!("💳 {}", bold(&format!("Cartões · {month_label_str}")));
    println!();

    if rows.is_empty() {
        println!("Sem movimentos de cartão no período.");
        return Ok(());
    }

    let today = Utc::now().date_naive();
    let mut total_charges = Decimal::ZERO;

    for row in &rows {
        // Parse cycle window from month_ref (YYYY-MM → 13/prev → 12/curr)
        // We use the account_id as label (no technical UUID shown)
        let account_label = human_format::truncate_with_ellipsis(
            &human_format::short_description(&row.account_id),
            24,
        );
        let charged = -row.total_charges; // total_charges is stored negative
        let open = -row.open_amount;
        total_charges += charged;

        // Payment status emoji
        let status_emoji = if open <= Decimal::ZERO {
            "🟢"
        } else if open < charged {
            "🟡"
        } else {
            // fully open — check if month is past
            let month_ref_str = &row.month_ref;
            let is_past = NaiveDate::parse_from_str(&format!("{month_ref_str}-28"), "%Y-%m-%d")
                .map(|d| d < today)
                .unwrap_or(false);
            if is_past {
                "🔴"
            } else {
                "🟡"
            }
        };

        let paid_label = if open <= Decimal::ZERO {
            "pago".to_string()
        } else if open < charged {
            format!("aberto {}", hf_brl(open))
        } else {
            format!("em aberto {}", hf_brl(open))
        };

        println!(
            "{status_emoji} {} · {} · {}",
            bold(&account_label),
            hf_brl(charged),
            paid_label
        );
    }

    println!();
    println!("Total cobrado: {}", bold(&hf_brl(total_charges)));

    Ok(())
}

async fn report_card_closed_insights(
    args: CardClosedInsightsArgs,
    format: ReportOutputFormat,
) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let today = Utc::now().date_naive();
    let target_month = match args.month.as_deref() {
        Some(value) => {
            parse_month_ref(value)?;
            value.to_string()
        }
        None => default_closed_cards_month(today)?,
    };
    let target_month_date = parse_month_ref(&target_month)?;

    let card_rows = store.card_summary(Some(&target_month)).await?;
    let target_closed_rows = store.card_closed_transactions(Some(&target_month)).await?;
    let target_reportable_rows = store
        .card_reportable_transactions(Some(&target_month))
        .await?;

    let mut month_rows = BTreeMap::<String, Vec<CardClosedTransactionRow>>::new();
    month_rows.insert(target_month.clone(), target_closed_rows.clone());
    for delta in 1..=2 {
        let month = month_ref_for(shift_month(target_month_date, -delta)?);
        let rows = store.card_closed_transactions(Some(&month)).await?;
        month_rows.insert(month, rows);
    }

    let mut merchant_months = BTreeMap::<String, BTreeSet<String>>::new();
    for (month, rows) in &month_rows {
        for row in rows {
            merchant_months
                .entry(merchant_key_for_card_row(row))
                .or_default()
                .insert(month.clone());
        }
    }

    let mut closed_count_by_account = BTreeMap::<String, usize>::new();
    let mut categories = BTreeMap::<String, (Decimal, usize)>::new();
    let mut recurring = BTreeMap::<String, (Decimal, usize)>::new();
    let mut subscriptions = BTreeMap::<String, (Decimal, usize)>::new();
    let mut closed_installments = BTreeMap::<(String, String), (Decimal, usize)>::new();
    let mut open_installments = BTreeMap::<(String, String), (Decimal, usize)>::new();

    for row in &target_closed_rows {
        *closed_count_by_account
            .entry(row.account_id.clone())
            .or_insert(0) += 1;
        let category = row
            .category_id
            .clone()
            .unwrap_or_else(|| "sem-categoria".to_string());
        let category_entry = categories.entry(category).or_insert((Decimal::ZERO, 0));
        category_entry.0 += row.amount;
        category_entry.1 += 1;

        let merchant = merchant_key_for_card_row(row);
        if merchant_months
            .get(&merchant)
            .is_some_and(|months| months.len() >= 2)
        {
            let recurring_entry = recurring
                .entry(merchant.clone())
                .or_insert((Decimal::ZERO, 0));
            recurring_entry.0 += row.amount;
            recurring_entry.1 += 1;
        }

        if is_subscription_row(row) {
            let subscription_entry = subscriptions
                .entry(merchant.clone())
                .or_insert((Decimal::ZERO, 0));
            subscription_entry.0 += row.amount;
            subscription_entry.1 += 1;
        }

        if let Some(marker) = detect_installment_marker(row) {
            let installment_entry = closed_installments
                .entry((merchant, marker))
                .or_insert((Decimal::ZERO, 0));
            installment_entry.0 += row.amount;
            installment_entry.1 += 1;
        }
    }

    for row in &target_reportable_rows {
        if !is_open_card_payment_status(&row.payment_status) {
            continue;
        }
        if let Some(marker) = detect_installment_marker(row) {
            let merchant = merchant_key_for_card_row(row);
            let installment_entry = open_installments
                .entry((merchant, marker))
                .or_insert((Decimal::ZERO, 0));
            installment_entry.0 += row.amount;
            installment_entry.1 += 1;
        }
    }

    let mut accounts = card_rows
        .into_iter()
        .map(|row| CardClosedAccountInsight {
            account_id: row.account_id.clone(),
            total_charges: row.total_charges,
            open_amount: row.open_amount,
            closed_amount: row.total_charges - row.open_amount,
            closed_transactions: closed_count_by_account
                .get(&row.account_id)
                .copied()
                .unwrap_or(0),
        })
        .collect::<Vec<_>>();
    accounts.sort_by(|a, b| {
        b.closed_amount
            .cmp(&a.closed_amount)
            .then_with(|| a.account_id.cmp(&b.account_id))
    });

    let mut category_rows = categories
        .into_iter()
        .map(
            |(category_id, (amount, transactions))| CardClosedCategoryInsight {
                category_id,
                amount,
                transactions,
            },
        )
        .collect::<Vec<_>>();
    category_rows.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| a.category_id.cmp(&b.category_id))
    });

    let mut recurring_rows = recurring
        .into_iter()
        .map(
            |(merchant_key, (amount, transactions))| CardClosedRecurringInsight {
                months_detected: merchant_months
                    .get(&merchant_key)
                    .map(|months| months.len())
                    .unwrap_or(1),
                merchant_key,
                amount,
                transactions,
            },
        )
        .collect::<Vec<_>>();
    recurring_rows.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| a.merchant_key.cmp(&b.merchant_key))
    });

    let mut subscription_rows = subscriptions
        .into_iter()
        .map(
            |(merchant_key, (amount, transactions))| CardClosedSubscriptionInsight {
                merchant_key,
                amount,
                transactions,
            },
        )
        .collect::<Vec<_>>();
    subscription_rows.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| a.merchant_key.cmp(&b.merchant_key))
    });

    let mut closed_installment_rows = closed_installments
        .into_iter()
        .map(
            |((merchant_key, marker), (amount, transactions))| CardClosedInstallmentInsight {
                merchant_key,
                marker,
                amount,
                transactions,
            },
        )
        .collect::<Vec<_>>();
    closed_installment_rows.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| a.merchant_key.cmp(&b.merchant_key))
            .then_with(|| a.marker.cmp(&b.marker))
    });

    let mut open_installment_rows = open_installments
        .into_iter()
        .map(
            |((merchant_key, marker), (amount, transactions))| CardClosedInstallmentInsight {
                merchant_key,
                marker,
                amount,
                transactions,
            },
        )
        .collect::<Vec<_>>();
    open_installment_rows.sort_by(|a, b| {
        b.amount
            .cmp(&a.amount)
            .then_with(|| a.merchant_key.cmp(&b.merchant_key))
            .then_with(|| a.marker.cmp(&b.marker))
    });

    let output = CardClosedInsightsOutput {
        month_ref: target_month,
        accounts,
        categories: category_rows,
        recurring: recurring_rows,
        subscriptions: subscription_rows,
        closed_installments: closed_installment_rows,
        open_installments: open_installment_rows,
    };

    if format.structured_output() {
        print_report_output(format, &output)?;
        return Ok(());
    }

    println!(
        "🧾 {}",
        bold(&format!(
            "Fatura fechada · {}",
            month_label(&output.month_ref)
        ))
    );
    println!();

    if output.accounts.is_empty() {
        println!("Sem cartões com movimentos no mês.");
        return Ok(());
    }

    // ── Fechamento por cartão ──
    println!("{}", subsection_header("Fechamento por cartão"));
    for row in &output.accounts {
        let account_label = human_format::truncate_with_ellipsis(
            &human_format::short_description(&row.account_id),
            24,
        );
        let charged = -row.closed_amount;
        let open = -row.open_amount;
        let status = if open <= Decimal::ZERO {
            "🟢 pago".to_string()
        } else {
            format!("🟡 aberto {}", hf_brl(open))
        };
        println!(
            "• {} — {} · {}",
            bold(&account_label),
            hf_brl(charged),
            status
        );
    }

    // ── Top categorias ──
    println!();
    println!("{}", subsection_header("Top categorias"));
    if output.categories.is_empty() {
        println!("Sem categorias detectadas.");
    } else {
        for row in output.categories.iter().take(3) {
            let emoji = category_emoji(Some(&row.category_id), None);
            let label = row.category_id.replace(':', " › ").replace('-', " ");
            println!(
                "• {} {} — {} ({} txn)",
                emoji,
                label,
                hf_brl(-row.amount),
                row.transactions
            );
        }
    }

    // ── Assinaturas ──
    println!();
    println!("{}", subsection_header("Assinaturas"));
    if output.subscriptions.is_empty() {
        println!("Nenhuma assinatura detectada.");
    } else {
        for row in output.subscriptions.iter().take(8) {
            println!(
                "• {} — {}/mês",
                human_format::truncate_with_ellipsis(
                    &human_format::short_description(&row.merchant_key),
                    28
                ),
                hf_brl(-row.amount)
            );
        }
    }

    // ── Parcelamentos ──
    println!();
    println!("{}", subsection_header("Parcelamentos"));
    let total_open_installments = output
        .open_installments
        .iter()
        .fold(Decimal::ZERO, |acc, r| acc + (-r.amount));
    let n_open = output.open_installments.len();
    if n_open == 0 {
        println!("Sem parcelas em aberto.");
    } else {
        println!(
            "{} parcela{} ativa{}, {} restantes",
            n_open,
            if n_open == 1 { "" } else { "s" },
            if n_open == 1 { "" } else { "s" },
            hf_brl(total_open_installments)
        );
        for row in output.open_installments.iter().take(5) {
            println!(
                "• {} ({}) — {}",
                human_format::truncate_with_ellipsis(
                    &human_format::short_description(&row.merchant_key),
                    24
                ),
                row.marker,
                hf_brl(-row.amount)
            );
        }
    }
    if !output.closed_installments.is_empty() {
        println!();
        println!("{}", bold("Fechadas nesta fatura:"));
        for row in output.closed_installments.iter().take(5) {
            println!(
                "• {} ({}) — {}",
                human_format::truncate_with_ellipsis(
                    &human_format::short_description(&row.merchant_key),
                    24
                ),
                row.marker,
                hf_brl(-row.amount)
            );
        }
    }

    // ── Recorrentes ──
    println!();
    println!("{}", subsection_header("Recorrentes"));
    if output.recurring.is_empty() {
        println!("Nenhum recorrente detectado.");
    } else {
        for row in output.recurring.iter().take(6) {
            println!(
                "• {} — {} ({} meses)",
                human_format::truncate_with_ellipsis(
                    &human_format::short_description(&row.merchant_key),
                    28
                ),
                hf_brl(-row.amount),
                row.months_detected
            );
        }
    }

    Ok(())
}

async fn report_installments(args: InstallmentsArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let today = Utc::now().date_naive();
    let from = shift_month(
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
            .context("Falha ao calcular mês atual")?,
        -(args.lookback_months as i32),
    )?;
    let to = today;

    let transactions: Vec<_> = store
        .transactions_in_date_range(args.account_id.as_deref(), from, to)
        .await?
        .into_iter()
        .map(|mut tx| {
            tx.raw_description =
                enrich_description_from_metadata(&tx.raw_description, &tx.metadata_json);
            tx
        })
        .collect();

    let all_chains = group_into_chains(&transactions);
    // Report only active chains (remaining > 0)
    let active_chains: Vec<&InstallmentChain> = all_chains
        .iter()
        .filter(|chain| chain.remaining > 0)
        .collect();

    if format.structured_output() {
        if args.verbose {
            let active = all_chains
                .iter()
                .filter(|c| c.remaining > 0)
                .collect::<Vec<_>>();
            print_report_output(format, &active)?;
        } else {
            let views: Vec<InstallmentChainView> = active_chains
                .iter()
                .map(|chain| InstallmentChainView::from_chain(chain))
                .collect();
            print_report_output(format, &views)?;
        }
        return Ok(());
    }

    println!("📦 *Parcelas ativas*");
    println!();

    if active_chains.is_empty() {
        println!("Nenhuma parcela ativa encontrada.");
        return Ok(());
    }

    let releasing: Vec<&&InstallmentChain> = active_chains
        .iter()
        .filter(|c| c.released_next_month)
        .collect();
    let ongoing: Vec<&&InstallmentChain> = active_chains
        .iter()
        .filter(|c| !c.released_next_month && c.remaining > 1)
        .collect();

    if !releasing.is_empty() {
        println!("🔔 *Liberam no próximo mês*");
        for chain in &releasing {
            println!(
                "{} · {}/{} · {} · termina {}",
                normalize_inline_text(&chain.base_description),
                chain.current,
                chain.total,
                brl_signed(chain.total_amount),
                short_date_pt(chain.projected_end),
            );
        }
        println!();
    }

    if !ongoing.is_empty() {
        let mut sorted_ongoing = ongoing.clone();
        sorted_ongoing.sort_by_key(|c| c.projected_end);
        println!("📅 *Em andamento*");
        for chain in &sorted_ongoing {
            println!(
                "{} · {}/{} · {} · termina {}",
                normalize_inline_text(&chain.base_description),
                chain.current,
                chain.total,
                brl_signed(chain.total_amount),
                short_date_pt(chain.projected_end),
            );
        }
        println!();
    }

    // Footer: total monthly commitment (next installment per chain = total_amount / total)
    let monthly_commitment: rust_decimal::Decimal = active_chains
        .iter()
        .map(|c| {
            if c.total > 0 {
                c.total_amount / rust_decimal::Decimal::from(c.total)
            } else {
                rust_decimal::Decimal::ZERO
            }
        })
        .fold(rust_decimal::Decimal::ZERO, |acc, v| acc + v);
    println!(
        "Compromisso mensal estimado: {}",
        brl_abs(monthly_commitment.abs())
    );

    Ok(())
}

async fn report_ofx_consistency(
    args: OfxConsistencyArgs,
    format: ReportOutputFormat,
) -> Result<()> {
    if args.date_tolerance_days < 0 {
        bail!("date_tolerance_days deve ser >= 0");
    }

    let ofx_raw = fs::read_to_string(&args.ofx)
        .with_context(|| format!("Falha ao ler OFX {}", args.ofx.display()))?;
    let mut ofx_rows = parse_ofx_transactions(&ofx_raw)?;
    if ofx_rows.is_empty() {
        bail!("Nenhuma transação <STMTTRN> encontrada no OFX");
    }
    ofx_rows.sort_by(|a, b| {
        a.transaction_date
            .cmp(&b.transaction_date)
            .then_with(|| a.amount.cmp(&b.amount))
            .then_with(|| a.description.cmp(&b.description))
    });

    let range_start = ofx_rows
        .iter()
        .map(|row| row.transaction_date)
        .min()
        .context("OFX sem data mínima")?;
    let range_end = ofx_rows
        .iter()
        .map(|row| row.transaction_date)
        .max()
        .context("OFX sem data máxima")?;
    let query_since = range_start
        .checked_sub_signed(Duration::days(args.date_tolerance_days))
        .context("Falha ao calcular início da janela de busca")?;
    let query_until = range_end
        .checked_add_signed(Duration::days(args.date_tolerance_days))
        .context("Falha ao calcular fim da janela de busca")?;

    let ofx_account_ids = parse_ofx_tag_values(&ofx_raw, "ACCTID")
        .into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect::<BTreeSet<_>>();

    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let mut db_rows = store
        .effective_transactions_window(None, query_since, query_until)
        .await?;

    let inferred_account_id = infer_account_id_from_fit_ids(&ofx_rows, &db_rows)
        .or_else(|| infer_account_id_from_overlap(&ofx_rows, &db_rows, args.date_tolerance_days));
    let account_filter = if let Some(account_id) = args.account_id.as_deref() {
        db_rows.retain(|row| row.account_id.as_deref() == Some(account_id));
        Some(format!("account_id:{account_id}"))
    } else if let Some(account_id) = inferred_account_id.as_deref() {
        db_rows.retain(|row| row.account_id.as_deref() == Some(account_id));
        Some(format!("inferred_account_id:{account_id}"))
    } else if !ofx_account_ids.is_empty() {
        let filtered = db_rows
            .iter()
            .filter(|row| {
                metadata_pluggy_account_id(row)
                    .is_some_and(|pluggy_id| ofx_account_ids.contains(pluggy_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        if !filtered.is_empty() {
            db_rows = filtered;
            Some(format!(
                "pluggy_account_id:{}",
                ofx_account_ids
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(",")
            ))
        } else {
            None
        }
    } else {
        None
    };

    db_rows.sort_by(|a, b| {
        a.transaction_date
            .cmp(&b.transaction_date)
            .then_with(|| a.amount.cmp(&b.amount))
            .then_with(|| a.transaction_id.cmp(&b.transaction_id))
    });

    let mut used_rows = vec![false; db_rows.len()];
    let mut matches = Vec::with_capacity(ofx_rows.len());
    let mut matched = 0usize;
    let mut missing = 0usize;

    for ofx_row in &ofx_rows {
        if let Some((best_idx, amount_diff, date_diff, desc_distance)) =
            find_best_ofx_match(ofx_row, &db_rows, &used_rows, args.date_tolerance_days)
        {
            used_rows[best_idx] = true;
            matched += 1;
            let db_row = &db_rows[best_idx];
            let status = if amount_diff == Decimal::ZERO && date_diff == 0 && desc_distance <= 1 {
                "matched"
            } else {
                "matched-near"
            };
            matches.push(OfxConsistencyMatch {
                status: status.to_string(),
                ofx_fit_id: ofx_row.fit_id.clone(),
                ofx_date: ofx_row.transaction_date.format("%Y-%m-%d").to_string(),
                ofx_amount: ofx_row.amount,
                ofx_description: ofx_row.description.clone(),
                transaction_id: Some(db_row.transaction_id.clone()),
                account_id: db_row.account_id.clone(),
                transaction_date: Some(db_row.transaction_date.format("%Y-%m-%d").to_string()),
                transaction_amount: Some(db_row.amount),
                transaction_description: Some(normalize_inline_text(db_row.display_description())),
                date_diff_days: Some(date_diff),
            });
        } else {
            missing += 1;
            matches.push(OfxConsistencyMatch {
                status: "missing-in-finance".to_string(),
                ofx_fit_id: ofx_row.fit_id.clone(),
                ofx_date: ofx_row.transaction_date.format("%Y-%m-%d").to_string(),
                ofx_amount: ofx_row.amount,
                ofx_description: ofx_row.description.clone(),
                transaction_id: None,
                account_id: None,
                transaction_date: None,
                transaction_amount: None,
                transaction_description: None,
                date_diff_days: None,
            });
        }
    }

    let extra_transactions = db_rows
        .iter()
        .zip(used_rows.iter())
        .filter_map(|(row, used)| {
            if *used {
                None
            } else {
                Some(OfxConsistencyDbOnly {
                    transaction_id: row.transaction_id.clone(),
                    transaction_date: row.transaction_date.format("%Y-%m-%d").to_string(),
                    amount: row.amount,
                    description: normalize_inline_text(row.display_description()),
                    account_id: row.account_id.clone(),
                })
            }
        })
        .collect::<Vec<_>>();

    let output = OfxConsistencyOutput {
        ofx_path: args.ofx.display().to_string(),
        account_filter,
        range_start: range_start.format("%Y-%m-%d").to_string(),
        range_end: range_end.format("%Y-%m-%d").to_string(),
        date_tolerance_days: args.date_tolerance_days,
        ofx_transactions: ofx_rows.len(),
        db_candidates: db_rows.len(),
        matched,
        missing_in_finance: missing,
        extra_in_finance: extra_transactions.len(),
        consistent: missing == 0 && extra_transactions.is_empty(),
        matches,
        extra_transactions,
    };

    if format.structured_output() {
        print_report_output(format, &output)?;
        return Ok(());
    }

    println!("🧪 OFX consistency check");
    println!("- arquivo: {}", output.ofx_path);
    println!(
        "- período OFX: {} -> {}",
        output.range_start, output.range_end
    );
    if let Some(filter) = &output.account_filter {
        println!("- filtro de conta: {filter}");
    }
    println!("- tolerância de data: {} dias", output.date_tolerance_days);
    println!("- transações OFX: {}", output.ofx_transactions);
    println!("- transações base (janela): {}", output.db_candidates);
    println!("- casadas: {}", output.matched);
    println!("- faltando na base: {}", output.missing_in_finance);
    println!("- sobrando na base: {}", output.extra_in_finance);
    println!(
        "- consistente: {}",
        if output.consistent { "sim" } else { "não" }
    );

    println!();
    println!("Comparação transação a transação:");
    for row in &output.matches {
        match row.status.as_str() {
            "matched" => {
                println!(
                    "✅ {} | {} | {} | {}",
                    row.ofx_date,
                    brl(row.ofx_amount),
                    row.ofx_description,
                    row.transaction_id.as_deref().unwrap_or("sem-id")
                );
            }
            "matched-near" => {
                println!(
                    "⚠️ {} | {} | {} | match aproximado {} | delta-data {}d",
                    row.ofx_date,
                    brl(row.ofx_amount),
                    row.ofx_description,
                    row.transaction_id.as_deref().unwrap_or("sem-id"),
                    row.date_diff_days.unwrap_or(0)
                );
            }
            _ => {
                println!(
                    "❌ {} | {} | {} | sem match",
                    row.ofx_date,
                    brl(row.ofx_amount),
                    row.ofx_description
                );
            }
        }
    }

    println!();
    println!("Extras na base (sem par no OFX):");
    if output.extra_transactions.is_empty() {
        println!("Nenhum extra detectado.");
    } else {
        for row in &output.extra_transactions {
            println!(
                "{} | {} | {} | {}",
                row.transaction_date,
                brl(row.amount),
                row.description,
                row.transaction_id
            );
        }
    }

    Ok(())
}

async fn report_uncategorized(args: UncategorizedArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.uncategorized(args.limit).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    let total_count = rows.len();
    let total_amount: rust_decimal::Decimal = rows
        .iter()
        .fold(rust_decimal::Decimal::ZERO, |acc, r| acc + r.amount);

    // Group by normalized description prefix (first 3 tokens, case-insensitive key)
    // Value: (display_label, total_amount, count, latest_date)
    let mut groups: std::collections::BTreeMap<
        String,
        (String, rust_decimal::Decimal, usize, chrono::NaiveDate),
    > = std::collections::BTreeMap::new();
    for row in &rows {
        let prefix: Vec<&str> = row.description.split_whitespace().take(3).collect();
        let key = prefix.join(" ").to_ascii_uppercase();
        let display = normalize_inline_text(&row.description);
        let entry = groups.entry(key).or_insert((
            display,
            rust_decimal::Decimal::ZERO,
            0,
            row.transaction_date,
        ));
        entry.1 += row.amount;
        entry.2 += 1;
        if row.transaction_date > entry.3 {
            entry.3 = row.transaction_date;
        }
    }

    // Sort by absolute total descending
    let mut sorted_groups: Vec<(String, rust_decimal::Decimal, usize, chrono::NaiveDate)> = groups
        .into_iter()
        .map(|(_, (label, amount, count, latest))| (label, amount, count, latest))
        .collect();
    sorted_groups.sort_by_key(|(_, amount, _, _)| std::cmp::Reverse(amount.abs()));

    println!("❓ *Sem categoria · top {}*", args.limit);
    println!();

    for (desc, amount, count, latest) in &sorted_groups {
        println!(
            "{} ({}×) · {} · mais recente {}",
            desc,
            count,
            brl_abs(*amount),
            short_date_pt(*latest),
        );
    }

    println!();
    println!(
        "Total: {} transações · {}",
        total_count,
        brl_abs(total_amount.abs())
    );

    Ok(())
}

async fn report_duplicates(_args: DuplicatesArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let groups = store.audit_duplicate_transactions().await?;

    if format.structured_output() {
        print_report_output(format, &groups)?;
        return Ok(());
    }

    if groups.is_empty() {
        println!("✅ Nenhuma transação duplicada encontrada.");
        return Ok(());
    }

    // Inflation = the amount the extra copies add on top of one canonical row.
    let extra_count: i64 = groups.iter().map(|g| g.count - 1).sum();
    let inflated: rust_decimal::Decimal = groups
        .iter()
        .map(|g| g.amount.abs() * rust_decimal::Decimal::from(g.count - 1))
        .sum();

    println!("⚠️ *Transações duplicadas · {} grupos*", groups.len());
    println!();
    for g in &groups {
        println!(
            "{}× · {} · {} · {} [{}]",
            g.count,
            brl_abs(g.amount.abs()),
            short_date_pt(g.transaction_date),
            normalize_inline_text(&g.description),
            g.sources.join(","),
        );
    }
    println!();
    println!(
        "Total: {} cópias extras inflando {} (mantendo 1 por grupo).",
        extra_count,
        brl_abs(inflated)
    );
    println!("Auditoria somente-leitura — a limpeza é um passo separado (ADR-0025).");

    Ok(())
}

async fn report_split_candidates(
    args: SplitCandidatesArgs,
    format: ReportOutputFormat,
) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let since = Utc::now()
        .date_naive()
        .checked_sub_signed(Duration::days(args.days.saturating_sub(1)))
        .context("Falha ao calcular janela de split candidates")?;
    let rows = store.split_candidates(since).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    println!("Split candidates desde {}", since.format("%Y-%m-%d"));
    println!("- linhas: {}", rows.len());
    println!();
    for row in rows {
        println!(
            "{} | {} | {} | {} | policy {} ({})",
            row.transaction_date.format("%Y-%m-%d"),
            brl(row.amount),
            row.transaction_id,
            normalize_inline_text(&row.description),
            row.policy_id,
            row.match_type
        );
    }
    Ok(())
}

async fn report_item_prices(args: ItemPricesArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let since = args
        .since
        .as_deref()
        .map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
        .transpose()
        .context("since inválido; use YYYY-MM-DD")?;
    let rows = store.item_prices(&args.query, since).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    println!("Item prices {}", args.query);
    println!("- linhas: {}", rows.len());
    println!();
    for row in rows {
        println!(
            "{} | {} | qty {} {} | unit {} | total {} | {}",
            row.transaction_date.format("%Y-%m-%d"),
            normalize_inline_text(&row.description),
            row.quantity
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string()),
            row.unit.unwrap_or_default(),
            row.unit_price
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string()),
            row.total_price
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string()),
            row.store_name.unwrap_or_else(|| "sem-loja".to_string())
        );
    }
    Ok(())
}

async fn report_data_health(args: DataHealthArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let today = Utc::now().date_naive();
    let window_start = today
        .checked_sub_signed(Duration::days(args.days.saturating_sub(1)))
        .context("Falha ao calcular janela do data-health")?;
    let items = store.daily_pulse(window_start).await?;
    let latest_pluggy_date = store.latest_pluggy_transaction_date().await?;
    let total_transactions = store.count_rows("transactions").await?;
    let transactions_with_context = store.count_transactions_with_context().await?;
    let uncategorized_count = store.count_uncategorized().await?;
    let active_rules_count = store.active_rules().await?.len();

    let mut flat_counts = BTreeMap::<String, usize>::new();
    let mut overlap_groups = BTreeMap::<(Option<String>, String), Vec<_>>::new();
    let mut window_pluggy_rows = 0usize;
    let mut window_legacy_rows = 0usize;
    let mut categorized_rows = 0usize;

    for item in &items {
        match item.source.as_str() {
            "pluggy" => window_pluggy_rows += 1,
            "legacy" => window_legacy_rows += 1,
            _ => {}
        }
        if let Some(category_id) = item.category_id.as_deref() {
            categorized_rows += 1;
            if is_flat_category(category_id) {
                *flat_counts.entry(category_id.to_string()).or_insert(0) += 1;
            }
        }
        overlap_groups
            .entry((item.account_id.clone(), decimal_text(item.amount.abs())))
            .or_default()
            .push(item);
    }

    let flat_category_rows = flat_counts.values().copied().sum::<usize>();
    let mut flat_categories = flat_counts
        .into_iter()
        .map(|(category_id, count)| FlatCategoryCount { category_id, count })
        .collect::<Vec<_>>();
    flat_categories.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.category_id.cmp(&b.category_id))
    });
    let window_other_rows = items
        .len()
        .saturating_sub(window_pluggy_rows + window_legacy_rows);
    let context_coverage_ratio = if total_transactions > 0 {
        transactions_with_context as f64 / total_transactions as f64
    } else {
        0.0
    };
    let mut overlap_candidates = Vec::new();
    for ((account_id, amount), rows) in overlap_groups {
        let legacy_rows = rows
            .iter()
            .filter(|item| item.source == "legacy" && item.amount.is_sign_negative())
            .copied()
            .collect::<Vec<_>>();
        let pluggy_rows = rows
            .iter()
            .filter(|item| item.source == "pluggy" && item.amount.is_sign_negative())
            .copied()
            .collect::<Vec<_>>();
        for legacy in legacy_rows {
            if let Some(pluggy) = pluggy_rows.iter().copied().find(|pluggy| {
                (legacy.transaction_date - pluggy.transaction_date)
                    .num_days()
                    .abs()
                    <= 7
                    && descriptions_look_related(&legacy.description, &pluggy.description)
            }) {
                overlap_candidates.push(OverlapCandidate {
                    account_id: account_id.clone(),
                    amount: amount.clone(),
                    legacy_transaction_id: legacy.transaction_id.clone(),
                    legacy_date: legacy.transaction_date.format("%Y-%m-%d").to_string(),
                    legacy_description: legacy.description.clone(),
                    pluggy_transaction_id: pluggy.transaction_id.clone(),
                    pluggy_date: pluggy.transaction_date.format("%Y-%m-%d").to_string(),
                    pluggy_description: pluggy.description.clone(),
                });
            }
        }
    }
    overlap_candidates.sort_by(|a, b| {
        b.legacy_date
            .cmp(&a.legacy_date)
            .then_with(|| a.account_id.cmp(&b.account_id))
            .then_with(|| a.amount.cmp(&b.amount))
    });
    let overlap_candidates_count = overlap_candidates.len();
    overlap_candidates.truncate(10);

    let output = DataHealthOutput {
        actor_id: config.actor_id.clone(),
        backend: format!("{:?}", config.effective_backend()).to_lowercase(),
        generated_at: Utc::now().to_rfc3339(),
        window_start: window_start.format("%Y-%m-%d").to_string(),
        window_days: args.days,
        latest_pluggy_transaction_date: latest_pluggy_date
            .map(|value| value.format("%Y-%m-%d").to_string()),
        pluggy_lag_days: latest_pluggy_date.map(|value| (today - value).num_days()),
        total_transactions,
        transactions_with_context,
        context_coverage_ratio,
        uncategorized_count,
        active_rules_count,
        window_rows: items.len(),
        window_pluggy_rows,
        window_legacy_rows,
        window_other_rows,
        categorized_rows,
        flat_category_rows,
        overlap_candidates_count,
        overlap_candidates,
        flat_categories,
    };

    if format.structured_output() {
        print_report_output(format, &output)?;
        return Ok(());
    }

    // Build check items: (severity 0=fail 1=warn 2=ok, emoji, description)
    let mut checks: Vec<(u8, &str, String)> = Vec::new();

    // Pluggy lag
    match output.pluggy_lag_days {
        Some(days) if days > 3 => {
            checks.push((0, "🔻", format!("Pluggy desatualizado · lag {} dias", days)))
        }
        Some(days) if days > 1 => checks.push((
            1,
            "🟡",
            format!("Pluggy levemente atrasado · lag {} dias", days),
        )),
        Some(_) => checks.push((2, "✅", "Pluggy atualizado".to_string())),
        None => checks.push((1, "🟡", "Sem data Pluggy registrada".to_string())),
    }

    // Uncategorized
    if output.uncategorized_count > 10 {
        checks.push((
            0,
            "🔻",
            format!("Sem categoria · {} transações", output.uncategorized_count),
        ));
    } else if output.uncategorized_count > 0 {
        checks.push((
            1,
            "🟡",
            format!("Sem categoria · {} transações", output.uncategorized_count),
        ));
    } else {
        checks.push((2, "✅", "Todas as transações categorizadas".to_string()));
    }

    // Flat categories
    if output.flat_category_rows > 0 {
        checks.push((
            1,
            "🟡",
            format!(
                "Categorias planas (sem hierarquia) · {} linhas",
                output.flat_category_rows
            ),
        ));
    } else {
        checks.push((2, "✅", "Sem categorias planas".to_string()));
    }

    // Overlaps
    if output.overlap_candidates_count > 0 {
        checks.push((
            0,
            "🔻",
            format!(
                "Sobreposições legacy×pluggy · {} candidatos",
                output.overlap_candidates_count
            ),
        ));
    } else {
        checks.push((2, "✅", "Sem sobreposições detectadas".to_string()));
    }

    // Context coverage
    let coverage_pct = output.context_coverage_ratio * 100.0;
    if coverage_pct < 50.0 {
        checks.push((
            1,
            "🟡",
            format!("Cobertura de contexto baixa · {:.0}%", coverage_pct),
        ));
    } else {
        checks.push((
            2,
            "✅",
            format!("Cobertura de contexto · {:.0}%", coverage_pct),
        ));
    }

    // Sort: failures first, then warnings, then ok
    checks.sort_by_key(|(severity, _, _)| *severity);

    let total_checks = checks.len();
    let problems = checks.iter().filter(|(s, _, _)| *s < 2).count();

    println!("🩹 *Saúde dos dados*");
    println!();

    if problems == 0 {
        println!("✅ Tudo certo · {} verificações.", total_checks);
    } else {
        for (_, emoji, desc) in &checks {
            println!("{} {}", emoji, desc);
        }

        if !output.overlap_candidates.is_empty() {
            println!();
            println!("*Sobreposições detectadas:*");
            for row in &output.overlap_candidates {
                println!(
                    "  {} · legacy {} · pluggy {}",
                    row.account_id.as_deref().unwrap_or("sem-conta"),
                    row.legacy_date,
                    row.pluggy_date,
                );
            }
        }

        println!();
        println!("{} problema(s) em {} verificações.", problems, total_checks);
    }

    Ok(())
}

async fn report_scenario(args: ScenarioArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let today = Utc::now().date_naive();
    let current_month = month_ref_for(
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
            .context("Falha ao calcular o mês atual")?,
    );
    let target_month = match args.month.as_deref() {
        Some(value) => {
            parse_month_ref(value)?;
            value.to_string()
        }
        None => default_scenario_month(today)?,
    };
    let history_months = args.history_months.max(1);
    let extra_expense = decimal_from_str(&args.extra_expense)?;
    let extra_income = decimal_from_str(&args.extra_income)?;

    let mut cashflow_rows = store.cashflow(history_months.saturating_add(12)).await?;
    cashflow_rows.sort_by(|a, b| b.month_ref.cmp(&a.month_ref));
    let baseline_rows = cashflow_rows
        .into_iter()
        .filter(|row| row.month_ref < target_month)
        .filter(|row| !(target_month > current_month && row.month_ref == current_month))
        .take(history_months)
        .collect::<Vec<_>>();
    if baseline_rows.is_empty() {
        bail!(
            "Não há meses históricos suficientes antes de {} para calcular cenário",
            target_month
        );
    }

    let divisor = Decimal::from(baseline_rows.len() as i64);
    let avg_income = baseline_rows
        .iter()
        .fold(Decimal::ZERO, |acc, row| acc + row.income)
        / divisor;
    let avg_expenses = baseline_rows
        .iter()
        .fold(Decimal::ZERO, |acc, row| acc + row.expenses)
        / divisor;
    let avg_net = baseline_rows
        .iter()
        .fold(Decimal::ZERO, |acc, row| acc + row.net)
        / divisor;

    let forecast_rows = store.forecast_vs_actual(Some(&target_month)).await?;
    let known_forecast_expenses = forecast_rows
        .iter()
        .fold(Decimal::ZERO, |acc, row| acc + row.forecast_amount);
    let planning_expenses = if known_forecast_expenses > avg_expenses {
        known_forecast_expenses
    } else {
        avg_expenses
    };

    let carryover_open_card_month =
        month_ref_for(shift_month(parse_month_ref(&target_month)?, -1)?);
    let carryover_open_card_amount = store
        .card_summary(Some(&carryover_open_card_month))
        .await?
        .into_iter()
        .fold(Decimal::ZERO, |acc, row| acc + row.open_amount);

    let projected_net = avg_income - planning_expenses + extra_income - extra_expense;
    let projected_cash_after_card_carry = projected_net - carryover_open_card_amount;

    let output = ScenarioOutput {
        target_month,
        baseline_months: baseline_rows
            .iter()
            .map(|row| row.month_ref.clone())
            .collect(),
        avg_income,
        avg_expenses,
        avg_net,
        planning_expenses,
        known_forecast_expenses,
        known_forecast_count: forecast_rows.len(),
        carryover_open_card_month,
        carryover_open_card_amount,
        extra_income,
        extra_expense,
        projected_net,
        projected_cash_after_card_carry,
        looks_ok_after_card_carry: projected_cash_after_card_carry >= Decimal::ZERO,
    };

    if format.structured_output() {
        print_report_output(format, &output)?;
        return Ok(());
    }

    println!("Scenario {}", output.target_month);
    println!("- baseline meses: {}", output.baseline_months.join(", "));
    println!("- média entradas: {}", brl(output.avg_income));
    println!("- média saídas: {}", brl(-output.avg_expenses));
    println!("- média líquida: {}", brl(output.avg_net));
    println!(
        "- despesas de planejamento: {}",
        brl(-output.planning_expenses)
    );
    println!(
        "- forecast conhecido: {} em {} itens",
        brl(-output.known_forecast_expenses),
        output.known_forecast_count
    );
    println!(
        "- faturas em aberto carregadas de {}: {}",
        output.carryover_open_card_month,
        brl(-output.carryover_open_card_amount)
    );
    println!("- ajuste manual de receita: {}", brl(output.extra_income));
    println!("- ajuste manual de despesa: {}", brl(-output.extra_expense));
    println!("- líquido projetado: {}", brl(output.projected_net));
    println!(
        "- caixa após carregar cartões em aberto: {}",
        brl(output.projected_cash_after_card_carry)
    );
    println!(
        "- parecer: {}",
        if output.looks_ok_after_card_carry {
            "fica positivo"
        } else {
            "fica pressionado / negativo"
        }
    );
    Ok(())
}

async fn tx_upsert_manual(args: ManualTransactionArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let tx_id = args
        .transaction_id
        .unwrap_or_else(|| format!("manual_{}", Uuid::now_v7()));
    let transaction_date =
        NaiveDate::parse_from_str(&args.date, "%Y-%m-%d").context("date inválida")?;
    let category_key = args
        .category
        .as_deref()
        .map(|value| category_key_from_input(value, args.subcategory.as_deref()));
    let now = Utc::now();
    let mut tx = TransactionRecord {
        transaction_id: tx_id.clone(),
        account_id: args.account_id.clone(),
        transaction_date,
        raw_description: args.description.clone(),
        description: Some(args.description),
        merchant_name: None,
        purpose: None,
        amount: decimal_from_str(&args.amount)?,
        tx_type: if args.amount.trim_start().starts_with('-') {
            "debit".to_string()
        } else {
            "credit".to_string()
        },
        category_id: category_key,
        category_source: if args.category.is_some() {
            "manual".to_string()
        } else {
            "uncategorized".to_string()
        },
        context: None,
        classifier_trace: args.context,
        payment_status: args.payment_status,
        source: "manual".to_string(),
        actor_id: config.actor_id.clone(),
        idempotency_key: manual_transaction_idempotency(&config.actor_id),
        metadata_json: json!({"origin": "phai-cli"}),
        created_at: now,
        updated_at: now,
        enrichment_attempted_at: None,
        amount_cents: None,
    };
    ensure_transaction_idempotency(&mut tx);
    store.upsert_transactions(&[tx.clone()]).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "transaction",
            &tx.transaction_id,
            "upsert_manual",
            &config.actor_id,
            &tx.idempotency_key,
            serde_json::to_value(&tx)?,
        )])
        .await?;
    println!("Transação manual salva: {}", tx.transaction_id);
    Ok(())
}

async fn tx_enrich(args: enrich::EnrichArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    enrich::run(args, &config, store.as_ref()).await
}

async fn tx_replicate_anatomy(args: ReplicateAnatomyArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let candidates = store
        .replicable_anatomy_candidates(args.limit)
        .await
        .context("replicable_anatomy_candidates falhou")?;

    let total = candidates.len();
    let mut replicated = 0usize;
    let mut no_donor = 0usize;
    let mut already_complete = 0usize;
    let mut errors = 0usize;

    for tx in &candidates {
        let outcome = match find_and_replicate(store.as_ref(), tx).await {
            Ok(o) => o,
            Err(err) => {
                eprintln!(
                    "aviso: replicação falhou para {} ({}): {err:#}",
                    tx.transaction_id, tx.raw_description
                );
                errors += 1;
                continue;
            }
        };
        match outcome {
            ReplicationOutcome::Replicated(rep) => {
                replicated += 1;
                println!(
                    "  replicate  {}  ←  {}{}{}{}",
                    tx.transaction_id,
                    rep.donor_id,
                    rep.description
                        .as_deref()
                        .map(|d| format!("  desc={d:?}"))
                        .unwrap_or_default(),
                    rep.purpose
                        .as_deref()
                        .map(|p| format!("  purpose={p:?}"))
                        .unwrap_or_default(),
                    rep.category_id
                        .as_deref()
                        .map(|c| format!("  category={c:?}"))
                        .unwrap_or_default(),
                );
                if !args.dry_run {
                    let idempotency_key =
                        format!("anatomy_rep:{}:{}", tx.transaction_id, Uuid::now_v7());
                    if rep.description.is_some() || rep.purpose.is_some() {
                        store
                            .update_transaction_anatomy(
                                &tx.transaction_id,
                                TransactionAnatomyPatch {
                                    description: rep.description.as_deref(),
                                    purpose: rep.purpose.as_deref(),
                                    ..TransactionAnatomyPatch::default()
                                },
                                &config.actor_id,
                                &idempotency_key,
                            )
                            .await
                            .with_context(|| {
                                format!("update_transaction_anatomy falhou: {}", tx.transaction_id)
                            })?;
                    }
                    if let Some(category_id) = rep.category_id.as_deref() {
                        store
                            .annotate_transaction(
                                &tx.transaction_id,
                                Some(category_id),
                                Some("replicated:human"),
                                None,
                                &config.actor_id,
                                &idempotency_key,
                            )
                            .await
                            .with_context(|| {
                                format!("annotate_transaction falhou: {}", tx.transaction_id)
                            })?;
                    }
                    let audit = AuditEvent::from_entity(
                        "transaction",
                        &tx.transaction_id,
                        "anatomy_replicated",
                        &config.actor_id,
                        &idempotency_key,
                        serde_json::json!({
                            "donor_id": rep.donor_id,
                            "description_replicated": rep.description.is_some(),
                            "purpose_replicated": rep.purpose.is_some(),
                            "category_id": rep.category_id,
                        }),
                    );
                    store.insert_audit_events(&[audit]).await?;
                }
            }
            ReplicationOutcome::NoDonor | ReplicationOutcome::NoMerchant => no_donor += 1,
            ReplicationOutcome::AlreadyComplete => already_complete += 1,
        }
    }

    println!(
        "\nTotal: {total}  replicado: {replicated}  sem donor: {no_donor}  já completo: {already_complete}  erros: {errors}{}",
        if args.dry_run { "  (dry-run — nenhuma alteração gravada)" } else { "" },
    );
    Ok(())
}

async fn tx_categorize(args: CategorizeTransactionArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let category_key = category_key_from_input(&args.category, args.subcategory.as_deref());
    let idempotency_key = format!("annotate:{}:{}", args.transaction_id, Uuid::now_v7());
    store
        .annotate_transaction(
            &args.transaction_id,
            Some(&category_key),
            Some("manual"),
            args.context.as_deref(),
            &config.actor_id,
            &idempotency_key,
        )
        .await?;
    let audit = AuditEvent::from_entity(
        "transaction",
        &args.transaction_id,
        "categorize",
        &config.actor_id,
        &idempotency_key,
        json!({
            "category_id": category_key,
            "context": args.context,
        }),
    );
    store.insert_audit_events(&[audit]).await?;
    println!("Categoria atualizada para {}", args.transaction_id);
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionMergeCopied {
    description: Option<String>,
    merchant_name: Option<String>,
    purpose: Option<String>,
    category_id: Option<String>,
    classifier_trace: Option<String>,
    context: Option<String>,
}

impl TransactionMergeCopied {
    fn has_anatomy(&self) -> bool {
        self.description.is_some()
            || self.merchant_name.is_some()
            || self.purpose.is_some()
            || self.classifier_trace.is_some()
            || self.context.is_some()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TransactionMergePlan {
    from: String,
    into_transaction_id: String,
    from_source: String,
    into_source: String,
    amount_diff: Decimal,
    date_diff_days: i64,
    copied: TransactionMergeCopied,
    dry_run: bool,
}

async fn tx_merge(args: MergeTransactionsArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let plan = merge_transactions(store.as_ref(), &config, &args).await?;
    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!(
            "Transações mescladas: {} -> {} (diferença {})",
            plan.from,
            plan.into_transaction_id,
            brl(plan.amount_diff)
        );
    }
    Ok(())
}

async fn merge_transactions(
    store: &dyn FinanceStore,
    config: &AppConfig,
    args: &MergeTransactionsArgs,
) -> Result<TransactionMergePlan> {
    if args.from == args.into_transaction_id {
        bail!("--from e --into precisam ser transações diferentes");
    }
    if args.max_days < 0 {
        bail!("--max-days não pode ser negativo");
    }
    let max_amount_diff = decimal_from_str(&args.max_amount_diff).with_context(|| {
        format!(
            "valor inválido em --max-amount-diff: {}",
            args.max_amount_diff
        )
    })?;
    if max_amount_diff < Decimal::ZERO {
        bail!("--max-amount-diff não pode ser negativo");
    }

    let duplicate = store
        .transaction_by_id(&args.from)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Transação duplicada {} não encontrada", args.from))?;
    let canonical = store
        .transaction_by_id(&args.into_transaction_id)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Transação canônica {} não encontrada",
                args.into_transaction_id
            )
        })?;

    validate_transaction_merge(&duplicate, &canonical, args, max_amount_diff)?;
    let copied = merge_copied_fields(&duplicate, &canonical);
    let amount_diff = decimal_abs(duplicate.amount - canonical.amount);
    let date_diff_days = duplicate
        .transaction_date
        .signed_duration_since(canonical.transaction_date)
        .num_days()
        .abs();
    let plan = TransactionMergePlan {
        from: duplicate.transaction_id.clone(),
        into_transaction_id: canonical.transaction_id.clone(),
        from_source: duplicate.source.clone(),
        into_source: canonical.source.clone(),
        amount_diff,
        date_diff_days,
        copied,
        dry_run: args.dry_run,
    };

    if args.dry_run {
        return Ok(plan);
    }

    let idempotency_key = format!(
        "merge:{}:{}:{}",
        duplicate.transaction_id,
        canonical.transaction_id,
        Uuid::now_v7()
    );
    if plan.copied.has_anatomy() {
        store
            .update_transaction_anatomy(
                &canonical.transaction_id,
                TransactionAnatomyPatch {
                    description: plan.copied.description.as_deref(),
                    merchant_name: plan.copied.merchant_name.as_deref(),
                    purpose: plan.copied.purpose.as_deref(),
                    classifier_trace: plan.copied.classifier_trace.as_deref(),
                    context: plan.copied.context.as_deref(),
                },
                &config.actor_id,
                &idempotency_key,
            )
            .await?;
    }
    if let Some(category_id) = plan.copied.category_id.as_deref() {
        store
            .annotate_transaction(
                &canonical.transaction_id,
                Some(category_id),
                Some("manual"),
                plan.copied.classifier_trace.as_deref(),
                &config.actor_id,
                &idempotency_key,
            )
            .await?;
    }
    store.delete_transaction(&duplicate.transaction_id).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "transaction",
            &canonical.transaction_id,
            "merge_duplicate",
            &config.actor_id,
            &idempotency_key,
            json!({
                "from": duplicate,
                "into_before": canonical,
                "copied": &plan.copied,
                "amount_diff": amount_diff,
                "date_diff_days": date_diff_days,
            }),
        )])
        .await?;

    Ok(plan)
}

fn validate_transaction_merge(
    duplicate: &TransactionRecord,
    canonical: &TransactionRecord,
    args: &MergeTransactionsArgs,
    max_amount_diff: Decimal,
) -> Result<()> {
    if !args.allow_any_source && !is_merge_duplicate_source(duplicate) {
        bail!(
            "A transação --from tem source='{}'; use --allow-any-source para mesclar mesmo assim",
            duplicate.source
        );
    }
    if !args.allow_non_pluggy_target && canonical.source != "pluggy" {
        bail!(
            "A transação --into tem source='{}'; use --allow-non-pluggy-target para manter essa linha",
            canonical.source
        );
    }
    if duplicate.account_id != canonical.account_id {
        bail!(
            "As transações têm contas diferentes: {:?} vs {:?}",
            duplicate.account_id,
            canonical.account_id
        );
    }
    let date_diff_days = duplicate
        .transaction_date
        .signed_duration_since(canonical.transaction_date)
        .num_days()
        .abs();
    if date_diff_days > args.max_days {
        bail!(
            "As datas diferem em {date_diff_days} dias, acima de --max-days {}",
            args.max_days
        );
    }
    if amount_sign(duplicate.amount) != amount_sign(canonical.amount) {
        bail!(
            "As transações têm sinais diferentes: {} vs {}",
            duplicate.amount,
            canonical.amount
        );
    }
    let amount_diff = decimal_abs(duplicate.amount - canonical.amount);
    if amount_diff > max_amount_diff {
        bail!(
            "A diferença de valor ({}) excede --max-amount-diff {}",
            amount_diff,
            max_amount_diff
        );
    }
    Ok(())
}

fn merge_copied_fields(
    duplicate: &TransactionRecord,
    canonical: &TransactionRecord,
) -> TransactionMergeCopied {
    TransactionMergeCopied {
        description: clean_merge_text(duplicate.description.as_deref()),
        merchant_name: clean_merge_text(duplicate.merchant_name.as_deref()),
        purpose: clean_merge_text(duplicate.purpose.as_deref()),
        category_id: human_category(duplicate, canonical),
        classifier_trace: clean_merge_text(duplicate.classifier_trace.as_deref()),
        context: clean_merge_text(duplicate.context.as_deref()),
    }
}

fn human_category(duplicate: &TransactionRecord, canonical: &TransactionRecord) -> Option<String> {
    if canonical.category_source == "manual" {
        return None;
    }
    matches!(
        duplicate.category_source.as_str(),
        "manual" | "enriched:user"
    )
    .then(|| duplicate.category_id.as_deref())
    .flatten()
    .and_then(|category| clean_merge_text(Some(category)))
}

fn clean_merge_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn is_merge_duplicate_source(row: &TransactionRecord) -> bool {
    matches!(row.source.as_str(), "manual" | "legacy") || row.transaction_id.starts_with("manual_")
}

fn amount_sign(value: Decimal) -> i8 {
    if value > Decimal::ZERO {
        1
    } else if value < Decimal::ZERO {
        -1
    } else {
        0
    }
}

fn decimal_abs(value: Decimal) -> Decimal {
    if value < Decimal::ZERO {
        -value
    } else {
        value
    }
}

async fn tx_set_anatomy(args: SetAnatomyArgs) -> Result<()> {
    if args.description.is_none()
        && args.merchant_name.is_none()
        && args.purpose.is_none()
        && args.classifier_trace.is_none()
    {
        bail!("Informe ao menos um campo: --description, --merchant-name, --purpose ou --classifier-trace");
    }
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let idempotency_key = format!("anatomy:{}:{}", args.transaction_id, Uuid::now_v7());
    store
        .update_transaction_anatomy(
            &args.transaction_id,
            TransactionAnatomyPatch {
                description: args.description.as_deref(),
                merchant_name: args.merchant_name.as_deref(),
                purpose: args.purpose.as_deref(),
                classifier_trace: args.classifier_trace.as_deref(),
                context: None,
            },
            &config.actor_id,
            &idempotency_key,
        )
        .await?;
    let audit = AuditEvent::from_entity(
        "transaction",
        &args.transaction_id,
        "set_anatomy",
        &config.actor_id,
        &idempotency_key,
        json!({
            "description": args.description,
            "merchant_name": args.merchant_name,
            "purpose": args.purpose,
            "classifier_trace": args.classifier_trace,
        }),
    );
    store.insert_audit_events(&[audit]).await?;
    println!("Anatomia atualizada para {}", args.transaction_id);
    Ok(())
}

async fn tx_set_context(args: SetContextArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let idempotency_key = format!("context:{}:{}", args.transaction_id, Uuid::now_v7());
    store
        .update_transaction_anatomy(
            &args.transaction_id,
            TransactionAnatomyPatch {
                context: Some(&args.context),
                ..TransactionAnatomyPatch::default()
            },
            &config.actor_id,
            &idempotency_key,
        )
        .await?;
    let audit = AuditEvent::from_entity(
        "transaction",
        &args.transaction_id,
        "set_context",
        &config.actor_id,
        &idempotency_key,
        json!({ "context": args.context }),
    );
    store.insert_audit_events(&[audit]).await?;
    println!("Contexto atualizado para {}", args.transaction_id);
    Ok(())
}

async fn tx_list_context(args: ListContextArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.transactions_with_context(args.limit).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("Transactions with context");
    println!("- linhas: {}", rows.len());
    println!();

    for row in rows {
        let account = row
            .account_label
            .or(row.account_id)
            .unwrap_or_else(|| "sem-conta".to_string());
        let category = row
            .category_id
            .unwrap_or_else(|| "sem-categoria".to_string());
        println!(
            "{} | {} | {} | {} | {} | {} | {}",
            row.transaction_id,
            row.transaction_date.format("%Y-%m-%d"),
            brl(row.amount),
            row.description,
            account,
            category,
            row.context
        );
    }
    Ok(())
}

fn print_transaction_row(row: &TransactionRecord) {
    let account = row.account_id.as_deref().unwrap_or("sem-conta");
    println!(
        "{} | {} | {} | {} | {} | {:?}",
        row.transaction_id,
        row.transaction_date.format("%Y-%m-%d"),
        decimal_text(row.amount),
        row.display_description(),
        account,
        row.context,
    );
}

async fn tx_find(args: TxFindArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store
        .find_transactions_by_description(&args.query, args.limit)
        .await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("Transactions matching {:?}", args.query);
    println!("- linhas: {}", rows.len());
    println!();
    for row in &rows {
        print_transaction_row(row);
    }
    Ok(())
}

async fn tx_pending(args: TxPendingArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.latest_uncategorized_transactions(args.limit).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("Pending (no context) transactions");
    println!("- linhas: {}", rows.len());
    println!();
    for row in &rows {
        print_transaction_row(row);
    }
    Ok(())
}

async fn tx_pending_human(args: PendingHumanArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = match args.kind {
        PendingHumanKind::Description => store.pending_human_descriptions(args.limit).await?,
        PendingHumanKind::Merchant => store.pending_merchants(args.limit).await?,
        PendingHumanKind::Purpose => {
            let threshold = decimal_from_str(&args.min_abs_amount)?;
            store.pending_purposes(threshold, args.limit).await?
        }
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("Pending human {:?} transactions", args.kind);
    println!("- linhas: {}", rows.len());
    println!();
    for row in &rows {
        print_transaction_row(row);
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewHumanQueueItem {
    transaction_id: String,
    account_id: Option<String>,
    transaction_date: String,
    amount: String,
    raw_description: String,
    display_description: String,
    description: Option<String>,
    merchant_name: Option<String>,
    purpose: Option<String>,
    category_id: Option<String>,
    category_source: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReviewHumanApplyResult {
    transaction_id: String,
    updated_description: bool,
    updated_merchant_name: bool,
    updated_purpose: bool,
    updated_category: bool,
    category_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewHumanSummary {
    uncategorized_count: i64,
    missing_description_count: i64,
    missing_merchant_count: i64,
    missing_purpose_count: i64,
    min_purpose_amount: String,
    total_attention_count: i64,
    suggested_next_command: String,
}

fn review_queue_item(row: &TransactionRecord) -> ReviewHumanQueueItem {
    ReviewHumanQueueItem {
        transaction_id: row.transaction_id.clone(),
        account_id: row.account_id.clone(),
        transaction_date: row.transaction_date.format("%Y-%m-%d").to_string(),
        amount: decimal_text(row.amount),
        raw_description: row.raw_description.clone(),
        display_description: row.display_description().to_string(),
        description: row.description.clone(),
        merchant_name: row.merchant_name.clone(),
        purpose: row.purpose.clone(),
        category_id: row.category_id.clone(),
        category_source: row.category_source.clone(),
    }
}

async fn review_human_summary(
    store: &dyn FinanceStore,
    min_abs_amount: Decimal,
    limit: usize,
) -> Result<ReviewHumanSummary> {
    let uncategorized_count = store.count_uncategorized().await?;
    let missing_description_count = store.count_pending_human_descriptions().await?;
    let missing_merchant_count = store.count_pending_merchants().await?;
    let missing_purpose_count = store.count_pending_purposes(min_abs_amount).await?;
    Ok(ReviewHumanSummary {
        uncategorized_count,
        missing_description_count,
        missing_merchant_count,
        missing_purpose_count,
        min_purpose_amount: decimal_text(min_abs_amount),
        total_attention_count: uncategorized_count
            + missing_description_count
            + missing_merchant_count
            + missing_purpose_count,
        suggested_next_command: format!("tx review-human --kind all --limit {limit} --json"),
    })
}

fn print_review_human_summary(summary: &ReviewHumanSummary) {
    println!("📌 Categorizações e descrições");
    println!("- sem categoria: {}", summary.uncategorized_count);
    println!(
        "- sem descrição humana: {}",
        summary.missing_description_count
    );
    println!("- sem estabelecimento: {}", summary.missing_merchant_count);
    println!(
        "- sem propósito acima de R$ {}: {}",
        summary.min_purpose_amount.replace('.', ","),
        summary.missing_purpose_count
    );
    println!();
    if summary.total_attention_count == 0 {
        println!("Tudo em dia por aqui.");
    } else {
        println!("Quer brincar de categorizar algumas agora? Responda \"sim\" que eu te mando a primeira leva.");
    }
}

fn effective_review_human_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_REVIEW_LIMIT)
}

pub(crate) async fn review_human_rows(
    store: &dyn FinanceStore,
    kind: ReviewHumanKind,
    limit: usize,
    min_abs_amount: Decimal,
    filters: &ReviewFilters,
) -> Result<Vec<TransactionRecord>> {
    let fetch_limit = review_human_fetch_limit(limit, filters);
    let mut rows = match kind {
        ReviewHumanKind::Description => store.pending_human_descriptions(fetch_limit).await?,
        ReviewHumanKind::Merchant => store.pending_merchants(fetch_limit).await?,
        ReviewHumanKind::Purpose => store.pending_purposes(min_abs_amount, fetch_limit).await?,
        ReviewHumanKind::All => review_human_rows_all(store, fetch_limit, min_abs_amount).await?,
    };
    rows.retain(|row| filters.matches(row));
    sort_review_human_rows(&mut rows);
    rows.truncate(limit);
    Ok(rows)
}

fn sort_review_human_rows(rows: &mut [TransactionRecord]) {
    rows.sort_by(|left, right| {
        right
            .transaction_date
            .cmp(&left.transaction_date)
            .then_with(|| right.amount.abs().cmp(&left.amount.abs()))
            .then_with(|| left.transaction_id.cmp(&right.transaction_id))
    });
}

fn review_human_fetch_limit(limit: usize, filters: &ReviewFilters) -> usize {
    if filters.is_empty() {
        return limit;
    }
    limit.saturating_mul(50).clamp(limit, 5_000)
}

async fn review_human_rows_all(
    store: &dyn FinanceStore,
    limit: usize,
    min_abs_amount: Decimal,
) -> Result<Vec<TransactionRecord>> {
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    for batch in [
        store.pending_human_descriptions(limit).await?,
        store.pending_merchants(limit).await?,
        store.pending_purposes(min_abs_amount, limit).await?,
    ] {
        for row in batch {
            if seen.insert(row.transaction_id.clone()) {
                rows.push(row);
            }
            if rows.len() >= limit {
                return Ok(rows);
            }
        }
    }
    Ok(rows)
}

fn print_review_queue(rows: &[TransactionRecord]) {
    println!("Pendências de anatomia humana");
    println!("- linhas: {}", rows.len());
    println!();
    for (index, row) in rows.iter().enumerate() {
        let category = row.category_id.as_deref().unwrap_or("sem-categoria");
        println!(
            "{}. {} | {} | {} | {}",
            index + 1,
            row.transaction_date.format("%Y-%m-%d"),
            brl(row.amount),
            category,
            row.display_description()
        );
        println!("   id: {}", row.transaction_id);
        println!("   raw: {}", row.raw_description);
        if let Some(merchant) = row.merchant_name.as_deref() {
            println!("   merchant: {merchant}");
        }
        if let Some(description) = row.description.as_deref() {
            println!("   description: {description}");
        }
        if let Some(purpose) = row.purpose.as_deref() {
            println!("   purpose: {purpose}");
        }
        println!();
    }
}

enum PromptValue {
    Keep,
    Set(String),
    Skip,
    Quit,
}

fn prompt_value(label: &str, current: Option<&str>) -> Result<PromptValue> {
    match current {
        Some(value) if !value.trim().is_empty() => print!("{label} [{value}]: "),
        _ => print!("{label}: "),
    }
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    match value {
        "" => Ok(PromptValue::Keep),
        "q" | "quit" | "sair" => Ok(PromptValue::Quit),
        "s" | "skip" | "pular" => Ok(PromptValue::Skip),
        _ => Ok(PromptValue::Set(value.to_string())),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HumanReviewPatch {
    pub(crate) description: Option<String>,
    pub(crate) merchant_name: Option<String>,
    pub(crate) purpose: Option<String>,
    pub(crate) category_id: Option<String>,
    /// Commitment-tier override (ADR-0032). `Some("")` clears it.
    pub(crate) commitment_tier: Option<String>,
}

impl HumanReviewPatch {
    fn has_changes(&self) -> bool {
        self.description.is_some()
            || self.merchant_name.is_some()
            || self.purpose.is_some()
            || self.category_id.is_some()
            || self.commitment_tier.is_some()
    }
}

pub(crate) async fn apply_human_review(
    store: &dyn FinanceStore,
    config: &AppConfig,
    transaction_id: &str,
    patch: HumanReviewPatch,
) -> Result<ReviewHumanApplyResult> {
    if !patch.has_changes() {
        bail!("Informe ao menos um campo humano ou categoria para salvar");
    }
    let updated_description = patch.description.is_some();
    let updated_merchant_name = patch.merchant_name.is_some();
    let updated_purpose = patch.purpose.is_some();
    let updated_category = patch.category_id.is_some();
    let existing = store
        .transaction_by_id(transaction_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Transação {transaction_id} não encontrada"))?;
    let idempotency_key = format!("review-human:{transaction_id}:{}", Uuid::now_v7());

    if patch.description.is_some() || patch.merchant_name.is_some() || patch.purpose.is_some() {
        store
            .update_transaction_anatomy(
                transaction_id,
                TransactionAnatomyPatch {
                    description: patch.description.as_deref(),
                    merchant_name: patch.merchant_name.as_deref(),
                    purpose: patch.purpose.as_deref(),
                    classifier_trace: None,
                    context: None,
                },
                &config.actor_id,
                &idempotency_key,
            )
            .await?;
    }

    if let Some(category_id) = patch.category_id.as_deref() {
        store
            .annotate_transaction(
                transaction_id,
                Some(category_id),
                Some("manual"),
                None,
                &config.actor_id,
                &idempotency_key,
            )
            .await?;
    }

    // Commitment-tier override (ADR-0032): an empty string clears it.
    if let Some(tier) = patch.commitment_tier.as_deref() {
        let tier = if tier.is_empty() { None } else { Some(tier) };
        store
            .set_commitment_tier(transaction_id, tier, &config.actor_id, &idempotency_key)
            .await?;
    }

    let audit = AuditEvent::from_entity(
        "transaction",
        transaction_id,
        "review_human",
        &config.actor_id,
        &idempotency_key,
        json!({
            "old": {
                "description": existing.description,
                "merchant_name": existing.merchant_name,
                "purpose": existing.purpose,
                "category_id": existing.category_id,
            },
            "new": {
                "description": patch.description.clone(),
                "merchant_name": patch.merchant_name.clone(),
                "purpose": patch.purpose.clone(),
                "category_id": patch.category_id.clone(),
                "commitment_tier": patch.commitment_tier.clone(),
            },
        }),
    );
    store.insert_audit_events(&[audit]).await?;

    Ok(ReviewHumanApplyResult {
        transaction_id: transaction_id.to_string(),
        updated_description,
        updated_merchant_name,
        updated_purpose,
        updated_category,
        category_id: patch.category_id,
    })
}

/// Fetch every transaction matching the filters, regardless of review status.
/// Used by the "ver todas" toggle (Ctrl+R) so the user can navigate through
/// already-curated rows alongside the pending ones.
pub(crate) async fn all_transactions_for_review(
    store: &dyn FinanceStore,
    limit: usize,
    min_abs_amount: Decimal,
    filters: &ReviewFilters,
) -> Result<Vec<TransactionRecord>> {
    use chrono::Datelike;
    // Determine date window from the month filter; fall back to last 90 days
    // when the user has cleared the month filter.
    let today = chrono::Utc::now().date_naive();
    let (from, to) = match filters.month.as_deref() {
        Some(month) => {
            // Parse YYYY-MM
            let parts: Vec<&str> = month.split('-').collect();
            if parts.len() == 2 {
                if let (Ok(y), Ok(m)) = (parts[0].parse::<i32>(), parts[1].parse::<u32>()) {
                    let from = chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(today);
                    // last day of month: next-month-1
                    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                    let to = chrono::NaiveDate::from_ymd_opt(ny, nm, 1)
                        .map(|d| d.pred_opt().unwrap_or(d))
                        .unwrap_or(today);
                    (from, to)
                } else {
                    (today - chrono::Duration::days(90), today)
                }
            } else {
                (today - chrono::Duration::days(90), today)
            }
        }
        None => {
            // Default window: current month minus 2 months → today
            let y = today.year();
            let m = today.month();
            let (sy, sm) = if m <= 2 { (y - 1, m + 10) } else { (y, m - 2) };
            let from = chrono::NaiveDate::from_ymd_opt(sy, sm, 1).unwrap_or(today);
            (from, today)
        }
    };
    let mut rows = store
        .effective_transactions_window(filters.account_id.as_deref(), from, to)
        .await?;
    // Apply remaining filters + min-abs-amount.
    rows.retain(|row| row.amount.abs() >= min_abs_amount && filters.matches(row));
    sort_review_human_rows(&mut rows);
    rows.truncate(limit);
    Ok(rows)
}

async fn tx_review_human(args: ReviewHumanArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let min_abs_amount = decimal_from_str(&args.min_abs_amount)?;
    let limit = effective_review_human_limit(args.limit);
    let mut filters = ReviewFilters::from_review_args(&args);
    // Resolve --owner to the set of account_ids owned by that name. This is
    // what makes Ford/OpenClaw able to scope the queue per-person without
    // having to enumerate accounts on every call.
    if let Some(owner_name) = filters.owner.clone() {
        let accounts = store.get_accounts().await?;
        let owned: BTreeSet<String> = accounts
            .into_iter()
            .filter(|a| a.owner == owner_name)
            .map(|a| a.account_id)
            .collect();
        if owned.is_empty() {
            anyhow::bail!(
                "Owner '{}' não bate com nenhuma conta. Use `fin --help` para listar opções (ou cheque accounts.owner).",
                owner_name
            );
        }
        filters.owner_accounts = Some(owned);
    }

    if args.summary {
        let summary = review_human_summary(store.as_ref(), min_abs_amount, limit).await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            print_review_human_summary(&summary);
        }
        return Ok(());
    }

    if let Some(transaction_id) = args.transaction_id.as_deref() {
        let category_id = args
            .category
            .as_deref()
            .map(|value| category_key_from_input(value, args.subcategory.as_deref()));
        let result = apply_human_review(
            store.as_ref(),
            &config,
            transaction_id,
            HumanReviewPatch {
                description: args.description,
                merchant_name: args.merchant_name,
                purpose: args.purpose,
                category_id,
                commitment_tier: None,
            },
        )
        .await?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&vec![result])?);
        } else {
            println!("Revisão salva");
        }
        return Ok(());
    }

    let rows =
        review_human_rows(store.as_ref(), args.kind, limit, min_abs_amount, &filters).await?;
    if args.json {
        let items = rows.iter().map(review_queue_item).collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&items)?);
        return Ok(());
    }

    if !io::stdin().is_terminal() {
        print_review_queue(&rows);
        println!(
            "stdin não é interativo; use --json ou --transaction-id para salvar via OpenClaw."
        );
        return Ok(());
    }

    println!("Comandos: Enter mantém, 's' pula a transação, 'q' sai.");
    println!();
    for (index, row) in rows.iter().enumerate() {
        println!(
            "[{}/{}] {} | {} | {}",
            index + 1,
            rows.len(),
            row.transaction_date.format("%Y-%m-%d"),
            brl(row.amount),
            row.category_id.as_deref().unwrap_or("sem-categoria")
        );
        println!("id: {}", row.transaction_id);
        println!("raw: {}", row.raw_description);
        println!("atual: {}", row.display_description());

        let mut patch = HumanReviewPatch {
            description: None,
            merchant_name: None,
            purpose: None,
            category_id: None,
            commitment_tier: None,
        };

        let ask_merchant = matches!(args.kind, ReviewHumanKind::Merchant)
            || (matches!(args.kind, ReviewHumanKind::All) && row.merchant_name.is_none());
        if ask_merchant {
            match prompt_value("merchant", row.merchant_name.as_deref())? {
                PromptValue::Set(value) => patch.merchant_name = Some(value),
                PromptValue::Skip => {
                    println!("pulada\n");
                    continue;
                }
                PromptValue::Quit => break,
                PromptValue::Keep => {}
            }
        }

        let ask_description = matches!(args.kind, ReviewHumanKind::Description)
            || (matches!(args.kind, ReviewHumanKind::All) && row.description.is_none());
        if ask_description {
            match prompt_value("description", row.description.as_deref())? {
                PromptValue::Set(value) => patch.description = Some(value),
                PromptValue::Skip => {
                    println!("pulada\n");
                    continue;
                }
                PromptValue::Quit => break,
                PromptValue::Keep => {}
            }
        }

        let ask_purpose = matches!(args.kind, ReviewHumanKind::Purpose)
            || (matches!(args.kind, ReviewHumanKind::All) && row.purpose.is_none());
        if ask_purpose {
            match prompt_value("purpose", row.purpose.as_deref())? {
                PromptValue::Set(value) => patch.purpose = Some(value),
                PromptValue::Skip => {
                    println!("pulada\n");
                    continue;
                }
                PromptValue::Quit => break,
                PromptValue::Keep => {}
            }
        }

        match prompt_value("category", row.category_id.as_deref())? {
            PromptValue::Set(value) => {
                patch.category_id = Some(category_key_from_input(&value, None))
            }
            PromptValue::Skip => {
                println!("pulada\n");
                continue;
            }
            PromptValue::Quit => break,
            PromptValue::Keep => {}
        }

        if patch.has_changes() {
            let result =
                apply_human_review(store.as_ref(), &config, &row.transaction_id, patch).await?;
            println!("salvo: {}\n", result.transaction_id);
        } else {
            println!("sem alterações\n");
        }
    }
    Ok(())
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SetContextByDescResult {
    transaction_id: String,
    description: String,
    old_context: Option<String>,
    new_context: String,
}

async fn tx_set_context_by_desc(args: SetContextByDescArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store
        .find_transactions_by_description(&args.query, 100)
        .await?;

    let results: Vec<SetContextByDescResult> = rows
        .iter()
        .map(|row| SetContextByDescResult {
            transaction_id: row.transaction_id.clone(),
            description: row.display_description().to_string(),
            old_context: row.context.clone(),
            new_context: args.context.clone(),
        })
        .collect();

    if args.dry_run {
        if args.json {
            println!("{}", serde_json::to_string_pretty(&results)?);
        } else {
            println!("Dry-run: {} transaction(s) would be updated", results.len());
            for r in &results {
                println!(
                    "{} | {} | {:?} -> {:?}",
                    r.transaction_id, r.description, r.old_context, r.new_context
                );
            }
        }
        return Ok(());
    }

    // Real run: apply context to each matching transaction
    let mut audit = Vec::new();
    for row in &rows {
        let idempotency_key = format!(
            "context-by-desc:{}:{}:{}",
            row.transaction_id,
            args.context,
            Uuid::now_v7()
        );
        store
            .update_transaction_anatomy(
                &row.transaction_id,
                TransactionAnatomyPatch {
                    context: Some(&args.context),
                    ..TransactionAnatomyPatch::default()
                },
                &config.actor_id,
                &idempotency_key,
            )
            .await?;
        audit.push(AuditEvent::from_entity(
            "transaction",
            &row.transaction_id,
            "set_context_by_desc",
            &config.actor_id,
            &idempotency_key,
            json!({
                "query": args.query,
                "context": args.context,
                "old_context": row.context,
            }),
        ));
    }
    insert_audit_chunked(store.as_ref(), &audit).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        println!("Contexto atualizado para {} transação(ões)", results.len());
        for r in &results {
            println!(
                "{} | {} | {:?} -> {:?}",
                r.transaction_id, r.description, r.old_context, r.new_context
            );
        }
    }
    Ok(())
}

async fn tx_split_preview(args: TxSplitPreviewArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let (parent, _payload, preview) =
        load_split_payload_preview(store.as_ref(), &args.transaction_id, &args.payload).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&preview)?);
        return Ok(());
    }

    println!("Split preview {}", preview.parent_transaction_id);
    println!(
        "- pai: {} | {}",
        brl(parent.amount),
        parent.display_description()
    );
    println!("- linhas: {}", preview.lines.len());
    println!("- itens de recibo: {}", preview.items.len());
    println!("- total split: {}", brl(preview.split_total));
    println!("- diferença: {}", brl(preview.difference));
    println!("- payload_hash: {}", preview.payload_hash);
    println!();
    for line in &preview.lines {
        println!(
            "{} | {} | {} | {}",
            line.line_index,
            brl(line.amount),
            line.category_id.as_deref().unwrap_or("sem-categoria"),
            line.description
        );
    }
    Ok(())
}

async fn tx_split_apply(args: TxSplitApplyArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let (_parent, payload, preview) =
        load_split_payload_preview(store.as_ref(), &args.transaction_id, &args.payload).await?;
    let now = Utc::now();
    let (split, lines, items) = build_split_records(
        &args.transaction_id,
        &config.actor_id,
        payload.source.as_deref(),
        payload.notes.as_deref(),
        payload.metadata.clone(),
        &preview,
        now,
    )?;
    store
        .apply_transaction_split(&split, &lines, &items)
        .await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "transaction_split",
            &split.split_id,
            "apply",
            &config.actor_id,
            &split.idempotency_key,
            json!({
                "parent_transaction_id": args.transaction_id,
                "payload_hash": split.payload_hash,
                "line_count": lines.len(),
                "receipt_item_count": items.len(),
            }),
        )])
        .await?;
    println!(
        "Split aplicado: {} ({} linhas, {} itens)",
        split.split_id,
        lines.len(),
        items.len()
    );
    Ok(())
}

async fn tx_split_show(args: TxSplitShowArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let detail = store
        .transaction_split_detail(&args.transaction_id)
        .await?
        .with_context(|| format!("Transação {} não encontrada", args.transaction_id))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&detail)?);
        return Ok(());
    }

    println!("Split {}", detail.parent.transaction_id);
    println!(
        "- pai: {} | {}",
        brl(detail.parent.amount),
        detail.parent.display_description()
    );
    match &detail.split {
        Some(split) => {
            println!("- split_id: {}", split.split_id);
            println!("- status: {}", split.status);
            println!("- payload_hash: {}", split.payload_hash);
        }
        None => {
            println!("- sem split ativo");
            return Ok(());
        }
    }
    println!("- linhas: {}", detail.lines.len());
    for line in &detail.lines {
        println!(
            "{} | {} | {} | {}",
            line.line_index,
            brl(line.amount),
            line.category_id.as_deref().unwrap_or("sem-categoria"),
            line.description
        );
    }
    println!("- itens de recibo: {}", detail.items.len());
    for item in detail.items.iter().take(20) {
        println!(
            "{} | {} | {} | unit {} | total {}",
            item.item_index,
            item.description,
            item.quantity
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string()),
            item.unit_price
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string()),
            item.total_price
                .map(decimal_text)
                .unwrap_or_else(|| "-".to_string())
        );
    }
    Ok(())
}

async fn tx_split_clear(args: TxSplitClearArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    ensure_bigquery_split_backend(&config)?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let parent = store
        .transaction_by_id(&args.transaction_id)
        .await?
        .with_context(|| format!("Transação {} não encontrada", args.transaction_id))?;
    let idempotency_key = format!("split-clear:{}:{}", args.transaction_id, Uuid::now_v7());
    store
        .clear_transaction_split(
            &args.transaction_id,
            &config.actor_id,
            &idempotency_key,
            args.reason.as_deref(),
        )
        .await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "transaction_split",
            &args.transaction_id,
            "clear",
            &config.actor_id,
            &idempotency_key,
            json!({
                "parent_transaction_id": parent.transaction_id,
                "reason": args.reason,
            }),
        )])
        .await?;
    println!("Split limpo para {}", args.transaction_id);
    Ok(())
}

async fn forecast_upsert(args: ForecastUpsertArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let due_date = args
        .date
        .as_deref()
        .map(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d"))
        .transpose()
        .context("date inválida para forecast")?;
    let mut row = ForecastRecord {
        forecast_id: args
            .forecast_id
            .unwrap_or_else(|| format!("forecast_{}", Uuid::now_v7())),
        due_date,
        description: args.description,
        amount: decimal_from_str(&args.amount)?,
        category_id: args
            .category
            .as_deref()
            .map(|value| category_key_from_input(value, args.subcategory.as_deref())),
        account_id: args.account_id,
        status: args.status,
        recurrence: args.recurrence,
        actor_id: config.actor_id.clone(),
        idempotency_key: String::new(),
        metadata_json: json!({"origin": "phai-cli"}),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        template_id: None,
        realized_transaction_id: None,
        realized_at: None,
    };
    ensure_forecast_idempotency(&mut row)?;
    store.upsert_forecasts(&[row.clone()]).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "forecast",
            &row.forecast_id,
            "upsert",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(&row)?,
        )])
        .await?;
    println!("Forecast salvo: {}", row.forecast_id);
    Ok(())
}

async fn rule_upsert(args: RuleUpsertArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let now = Utc::now();
    let mut row = RuleRecord {
        rule_id: args.rule_id,
        body: args.body,
        status: args.status,
        actor_id: config.actor_id.clone(),
        idempotency_key: String::new(),
        created_at: now,
        updated_at: now,
    };
    ensure_rule_idempotency(&mut row);
    store.upsert_rules(&[row.clone()]).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "rule",
            &row.rule_id,
            "upsert",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(&row)?,
        )])
        .await?;
    println!("Rule salva: {}", row.rule_id);
    Ok(())
}

fn rule_matches_status(rule: &RuleRecord, status: RuleStatusFilter) -> bool {
    match status {
        RuleStatusFilter::All => true,
        RuleStatusFilter::Active => rule.status.eq_ignore_ascii_case("active"),
        RuleStatusFilter::Disabled => rule.status.eq_ignore_ascii_case("disabled"),
    }
}

async fn rule_list(args: RuleListArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store
        .all_rules()
        .await?
        .into_iter()
        .filter(|row| rule_matches_status(row, args.status))
        .collect::<Vec<_>>();

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!("Rules status={}", args.status.as_str());
    println!("- linhas: {}", rows.len());
    println!();

    for row in rows {
        println!(
            "{} | {} | {} | {}",
            row.rule_id,
            row.status,
            row.updated_at.format("%Y-%m-%d"),
            row.body
        );
    }
    Ok(())
}

async fn rule_inspect(args: RuleInspectArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let row = store
        .all_rules()
        .await?
        .into_iter()
        .find(|row| row.rule_id == args.rule_id)
        .with_context(|| format!("Rule {} não encontrada", args.rule_id))?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&row)?);
        return Ok(());
    }

    println!("Rule {}", row.rule_id);
    println!("- status: {}", row.status);
    println!("- actor_id: {}", row.actor_id);
    println!("- created_at: {}", row.created_at.to_rfc3339());
    println!("- updated_at: {}", row.updated_at.to_rfc3339());
    println!("- idempotency_key: {}", row.idempotency_key);
    println!();
    println!("{}", row.body);
    Ok(())
}

async fn account_upsert(args: AccountUpsertArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let now = Utc::now();
    let mut row = AccountRecord {
        account_id: args.account_id,
        owner: args.owner,
        account_type: args.account_type,
        bank: args.bank,
        label: args.label,
        pluggy_account_id: args.pluggy_account_id,
        pluggy_item_id: args.pluggy_item_id,
        status: args.status,
        actor_id: config.actor_id.clone(),
        idempotency_key: String::new(),
        metadata_json: json!({"origin": "phai-cli"}),
        created_at: now,
        updated_at: now,
    };
    ensure_account_idempotency(&mut row);
    store.upsert_accounts(&[row.clone()]).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "account",
            &row.account_id,
            "upsert",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(&row)?,
        )])
        .await?;
    println!("Conta salva: {}", row.account_id);
    Ok(())
}

async fn account_set_billing_cycle(args: AccountBillingCycleArgs) -> Result<()> {
    if !(1..=28).contains(&args.closing_day) || !(1..=28).contains(&args.due_day) {
        anyhow::bail!("closing-day e due-day devem estar entre 1 e 28");
    }
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let mut accounts = store.get_accounts().await?;
    let row = accounts
        .iter_mut()
        .find(|a| a.account_id == args.account_id)
        .with_context(|| format!("conta '{}' não encontrada", args.account_id))?;

    if !row.metadata_json.is_object() {
        row.metadata_json = json!({});
    }
    if let Some(obj) = row.metadata_json.as_object_mut() {
        obj.insert(
            "billing_closing_day".into(),
            json!(args.closing_day.to_string()),
        );
        obj.insert("billing_due_day".into(), json!(args.due_day.to_string()));
    }
    row.updated_at = Utc::now();
    let row = row.clone();

    store.upsert_accounts(std::slice::from_ref(&row)).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "account",
            &row.account_id,
            "set_billing_cycle",
            &config.actor_id,
            &row.idempotency_key,
            serde_json::to_value(&row)?,
        )])
        .await?;
    println!(
        "Ciclo de fatura salvo: {} (fecha dia {}, vence dia {})",
        row.account_id, args.closing_day, args.due_day
    );
    Ok(())
}

async fn budget_upsert(args: BudgetUpsertArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    if let Some(ref month) = args.month {
        parse_month_ref(month)?;
    }

    let amount = decimal_from_str(&args.amount)?;
    let now = Utc::now();
    let budget_id = format!("budget_{}", uuid::Uuid::now_v7());
    let idempotency_key = format!(
        "budget:{}:{}:{}",
        args.category_id,
        args.subcategory_id.as_deref().unwrap_or(""),
        args.month.as_deref().unwrap_or("_default"),
    );
    let record = CategoryBudgetRecord {
        budget_id: budget_id.clone(),
        category_id: args.category_id.clone(),
        subcategory_id: args.subcategory_id.clone(),
        month_ref: args.month.clone(),
        amount,
        alert_threshold_pct: args.alert_threshold_pct,
        actor_id: config.actor_id.clone(),
        idempotency_key: idempotency_key.clone(),
        created_at: now,
        updated_at: now,
    };
    store.upsert_category_budget(&record).await?;
    store
        .insert_audit_events(&[AuditEvent::from_entity(
            "category_budget",
            &budget_id,
            "upsert",
            &config.actor_id,
            &idempotency_key,
            serde_json::to_value(&record)?,
        )])
        .await?;
    println!(
        "Budget salvo: {} (category: {}, month: {})",
        budget_id,
        args.category_id,
        args.month.as_deref().unwrap_or("_default"),
    );
    Ok(())
}

async fn budget_list(args: BudgetListArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let rows = store.list_category_budgets(args.month.as_deref()).await?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }

    println!(
        "Budgets{}",
        args.month
            .as_deref()
            .map(|m| format!(" {m}"))
            .unwrap_or_default()
    );
    println!("- linhas: {}", rows.len());
    println!();

    for row in rows {
        let subcat = row.subcategory_id.as_deref().unwrap_or("-");
        let month = row.month_ref.as_deref().unwrap_or("_default");
        println!(
            "{} | sub: {} | month: {} | budget: {} | threshold: {}%",
            row.category_id,
            subcat,
            month,
            brl(row.amount),
            row.alert_threshold_pct,
        );
    }
    Ok(())
}

async fn report_budget_status(args: BudgetStatusArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    parse_month_ref(&args.month)?;
    let rows: Vec<BudgetStatusRow> = store.budget_status_for_month(&args.month).await?;

    if format.structured_output() {
        print_report_output(format, &rows)?;
        return Ok(());
    }

    println!(
        "🎯 {}",
        bold(&format!("Orçamentos · {}", month_label(&args.month)))
    );
    println!();

    if rows.is_empty() {
        println!("Nenhum orçamento configurado para o período.");
        return Ok(());
    }

    // Sort: over budget first, then alert, then by usage_pct descending
    let mut sorted = rows.clone();
    sorted.sort_by_key(|r| {
        let tier: i32 = if r.actual_amount > r.budget_amount {
            0 // over
        } else if r.alert {
            1 // near threshold
        } else {
            2 // ok
        };
        let usage_inv = -(r.usage_pct.to_string().parse::<f64>().unwrap_or(0.0) as i64);
        (tier, usage_inv)
    });

    let mut total_budget = Decimal::ZERO;
    let mut total_actual = Decimal::ZERO;

    for row in &sorted {
        let usage_i64 = row
            .usage_pct
            .to_string()
            .parse::<f64>()
            .map(|v| v as i64)
            .unwrap_or(0);
        let bar = progress_bar(usage_i64);
        let status_emoji = if row.actual_amount > row.budget_amount {
            "🔻"
        } else if row.alert {
            "🟡"
        } else {
            "✅"
        };
        let label = row.category_id.replace(':', " › ").replace('-', " ");
        println!(
            "{} {} — {} / {} ({}%)",
            status_emoji,
            bold(&label),
            hf_brl(row.actual_amount),
            hf_brl(row.budget_amount),
            usage_i64
        );
        println!("   {bar}");
        total_budget += row.budget_amount;
        total_actual += row.actual_amount;
    }

    println!();
    println!(
        "Total: {} de {} orçados",
        bold(&hf_brl(total_actual)),
        hf_brl(total_budget)
    );

    Ok(())
}

// ─── report cards ──────────────────────────────────────────────────────────
//
// Unified credit-card report. Supersedes the deprecated `card-summary` and
// `card-closed-insights` reports.
//
// Sections (default human output):
//   1. Visão geral — per-card totals + payment status (paid / open / overdue)
//   2. Parceladas — active installment chains: total commitment, what's new
//      this month, what frees up next month
//   3. Gastos por categoria — every charge in the month grouped by family,
//      then by category, sorted by spend descending
//
// Payment status is inferred by searching checking accounts for an outgoing
// debit categorized as `credit-card-payment` (or whatever internal category
// the user's classifier rules assign to bill payments) within a ±15-day
// window of the bill close date, matching the bill total within ±5%.

#[derive(Debug, Clone, serde::Serialize)]
struct CardsBillStatus {
    /// `paid`, `open`, `overdue`, or `unknown`
    state: &'static str,
    /// Date the bill closes (start of the next cycle, ~billing_closing_day).
    /// Inferred as the last day of the report month when we don't know better.
    close_date: NaiveDate,
    /// Date the bill is due. Inferred as close_date + 7 days when we don't
    /// have a `billing_due_day` for the account.
    due_date: NaiveDate,
    /// When `state = paid`, the date we believe the user paid the bill.
    paid_on: Option<NaiveDate>,
    /// Total of the bill in the report month.
    total: Decimal,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CardsAccountReport {
    account_id: String,
    transaction_count: usize,
    status: CardsBillStatus,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CardsReport {
    month_ref: String,
    accounts: Vec<CardsAccountReport>,
    grand_total: Decimal,
}

/// Extract Pluggy's `creditCardMetadata.billId` from a transaction metadata blob.
fn extract_bill_id(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("raw")
        .and_then(|v| v.get("creditCardMetadata"))
        .and_then(|v| v.get("billId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Compute a synthetic bill identifier from a transaction date and the
/// account's billing closing day. Brazilian credit cards conventionally
/// attribute transactions posted ON the closing day to the *next* cycle
/// (Nubank's OFX statements confirm: DTSTART = closing_day_prev_month means
/// transactions on that date are the first in the new cycle). So:
/// transactions on day < closing_day belong to the bill closing this month;
/// transactions on day >= closing_day belong to next month's bill.
fn compute_bill_id(date: NaiveDate, closing_day: u32) -> String {
    let (year, month, day) = (date.year(), date.month(), date.day());
    let (bill_year, bill_month) = if day < closing_day {
        (year, month)
    } else if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    format!("cycle-{}-{:02}-{:02}", bill_year, bill_month, closing_day)
}

/// Parse `billing_closing_day` from account metadata JSON (stored as a string
/// or integer for forward compat). When only the due day is known, infer the
/// closing day as seven days before the due date, matching Nubank's observed
/// cycle shape. Only values in 1..=28 are accepted.
fn parse_closing_day(metadata: &serde_json::Value) -> Option<u32> {
    metadata_billing_day(metadata.get("billing_closing_day"))
        .or_else(|| metadata_due_day(metadata).and_then(closing_day_from_due_day))
}

fn metadata_billing_day(value: Option<&serde_json::Value>) -> Option<u32> {
    let value = value?;
    match value {
        serde_json::Value::String(s) => s.parse::<u32>().ok(),
        serde_json::Value::Number(n) => n.as_u64().map(|d| d as u32),
        _ => None,
    }
    .filter(|d| (1..=28).contains(d))
}

fn metadata_due_day(metadata: &serde_json::Value) -> Option<u32> {
    metadata_billing_day(metadata.get("billing_due_day")).or_else(|| {
        metadata
            .pointer("/raw/creditData/balanceDueDate")
            .and_then(|value| value.as_str())
            .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok())
            .map(|date| date.day())
            .filter(|day| (1..=28).contains(day))
    })
}

fn closing_day_from_due_day(due_day: u32) -> Option<u32> {
    if !(1..=28).contains(&due_day) {
        return None;
    }
    Some(if due_day > 7 {
        due_day - 7
    } else {
        due_day + 21
    })
}

/// Infer the account's billing closing day by inspecting Pluggy-supplied
/// billId clusters in the transaction data. Pluggy tags some (~20%) of
/// credit-card transactions with `creditCardMetadata.billId`. Each unique
/// billId is a real bill; its `max(transaction_date)` is the last day of
/// the cycle, and the bill itself closes 1–2 days later (Pluggy stops
/// posting once the cycle ends).
///
/// Returns the *mode* of (max_date.day + 2), clamped to 1..=28. Returns
/// None when fewer than 2 Pluggy-tagged bills are observed (not enough
/// signal to commit to a value).
fn infer_closing_day_from_pluggy(rows: &[CardClosedTransactionRow]) -> Option<u32> {
    use std::collections::HashMap;
    let mut bill_max: HashMap<String, NaiveDate> = HashMap::new();
    for row in rows {
        if let Some(bill_id) = extract_bill_id(&row.metadata_json) {
            let entry = bill_max.entry(bill_id).or_insert(row.transaction_date);
            if row.transaction_date > *entry {
                *entry = row.transaction_date;
            }
        }
    }
    if bill_max.len() < 2 {
        return None;
    }
    let mut day_counts: HashMap<u32, u32> = HashMap::new();
    for max_date in bill_max.values() {
        let close_day = (max_date.day() + 2).min(28);
        *day_counts.entry(close_day).or_insert(0) += 1;
    }
    day_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(day, _)| day)
}

fn group_card_rows_by_account(
    rows: &[CardClosedTransactionRow],
) -> BTreeMap<String, Vec<CardClosedTransactionRow>> {
    let mut grouped = BTreeMap::new();
    for row in rows {
        grouped
            .entry(row.account_id.clone())
            .or_insert_with(Vec::new)
            .push(row.clone());
    }
    grouped
}

fn card_close_date(
    selected_close_dates: &BTreeMap<String, Option<NaiveDate>>,
    account_id: &str,
    txs: &[CardClosedTransactionRow],
    today: NaiveDate,
) -> NaiveDate {
    selected_close_dates
        .get(account_id)
        .copied()
        .flatten()
        .unwrap_or_else(|| {
            txs.iter()
                .map(|t| t.transaction_date)
                .max()
                .unwrap_or(today)
        })
}

fn matched_card_payment(
    payment_candidates: &[TransactionRecord],
    close_date: NaiveDate,
    full_total: Decimal,
) -> Option<&TransactionRecord> {
    let tolerance = full_total * Decimal::new(10, 2);
    let lower = full_total - tolerance;
    let upper = full_total + tolerance;
    payment_candidates
        .iter()
        .filter(|t| t.transaction_date >= close_date.saturating_sub_signed_unsafe())
        .filter(|t| {
            let abs = t.amount.abs();
            abs >= lower && abs <= upper
        })
        .min_by_key(|t| (t.transaction_date - close_date).num_days().unsigned_abs())
}

fn card_bill_status(
    mode: CardsMode,
    today: NaiveDate,
    close_date: NaiveDate,
    total: Decimal,
    full_total: Decimal,
    payment_candidates: &[TransactionRecord],
) -> CardsBillStatus {
    let due_date = close_date
        .checked_add_signed(Duration::days(7))
        .unwrap_or(close_date);

    if mode == CardsMode::Next {
        return CardsBillStatus {
            state: "partial",
            close_date,
            due_date,
            paid_on: None,
            total,
        };
    }
    if today < close_date {
        return CardsBillStatus {
            state: "open",
            close_date,
            due_date,
            paid_on: None,
            total,
        };
    }
    match matched_card_payment(payment_candidates, close_date, full_total) {
        Some(tx) => CardsBillStatus {
            state: "paid",
            close_date,
            due_date,
            paid_on: Some(tx.transaction_date),
            total,
        },
        None if today > due_date => CardsBillStatus {
            state: "overdue",
            close_date,
            due_date,
            paid_on: None,
            total,
        },
        None => CardsBillStatus {
            state: "open",
            close_date,
            due_date,
            paid_on: None,
            total,
        },
    }
}

fn build_cards_account_reports(
    by_account_full: &BTreeMap<String, Vec<CardClosedTransactionRow>>,
    by_account_display: &BTreeMap<String, Vec<CardClosedTransactionRow>>,
    selected_close_dates: &BTreeMap<String, Option<NaiveDate>>,
    payment_candidates: &[TransactionRecord],
    mode: CardsMode,
    today: NaiveDate,
    installments_only: bool,
) -> (Vec<CardsAccountReport>, Decimal) {
    let mut accounts_report = Vec::with_capacity(by_account_full.len());
    let mut grand_total = Decimal::ZERO;
    for (account_id, full_txs) in by_account_full {
        let display_txs = by_account_display
            .get(account_id)
            .map_or(&[][..], Vec::as_slice);
        if installments_only && display_txs.is_empty() {
            continue;
        }

        let full_total: Decimal = full_txs.iter().map(|t| t.amount).sum::<Decimal>().abs();
        let total: Decimal = display_txs.iter().map(|t| t.amount).sum::<Decimal>().abs();
        grand_total += total;

        let close_date = card_close_date(selected_close_dates, account_id, full_txs, today);
        let status = card_bill_status(
            mode,
            today,
            close_date,
            total,
            full_total,
            payment_candidates,
        );

        accounts_report.push(CardsAccountReport {
            account_id: account_id.clone(),
            transaction_count: display_txs.len(),
            status,
        });
    }
    (accounts_report, grand_total)
}

async fn report_cards(args: CardsArgs, format: ReportOutputFormat) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let accounts = store.get_accounts().await?;
    let mut closing_days: BTreeMap<String, u32> = accounts
        .iter()
        .filter_map(|a| parse_closing_day(&a.metadata_json).map(|d| (a.account_id.clone(), d)))
        .collect();

    let today = Utc::now().date_naive();
    let (mode, target_month) = match (&args.month, args.closed, args.next) {
        (Some(value), _, _) => {
            parse_month_ref(value)?;
            (CardsMode::Specific, value.to_string())
        }
        (None, _, true) => (CardsMode::Next, format!("{}", today.format("%Y-%m"))),
        _ => (
            CardsMode::Closed,
            format!("{}", today.format("%Y-%m")), // placeholder; we'll resolve later from billId clusters
        ),
    };

    // Fetch a wide window so we capture full bill cycles regardless of where
    // they straddle calendar months. 90 days covers the most-recent-closed
    // bill (~30d) plus the cycle before it as fallback. A `--month` request
    // anchors the window on that cycle (through the following month, where
    // its payment lands) so older bills are not cut off by today's window.
    let window_end = match mode {
        CardsMode::Specific => shift_month(parse_month_ref(&target_month)?, 2)? - Duration::days(1),
        CardsMode::Closed | CardsMode::Next => today,
    };
    let window_start = window_end
        .checked_sub_signed(Duration::days(90))
        .unwrap_or(window_end);
    let mut all_rows: Vec<CardClosedTransactionRow> = Vec::new();
    // Existing storage API is month-scoped; iterate over months in the window.
    let mut cursor = window_start;
    while cursor <= window_end {
        let m = format!("{}", cursor.format("%Y-%m"));
        let txs = store.card_closed_transactions(Some(&m)).await?;
        all_rows.extend(txs);
        cursor = shift_month(cursor, 1).unwrap_or(window_end + Duration::days(1));
    }

    // Optional --account-id filter
    if let Some(id) = args.account_id.as_deref() {
        all_rows.retain(|r| r.account_id == id);
    }

    // Note: `--installments-only` is intentionally NOT applied here.
    // Filtering at this stage would shrink the per-bill totals and break
    // payment matching downstream (the matcher compares the *full* bill
    // total against checking-account "Pagamento de fatura" debits within
    // a ±10% tolerance). Instead, status is computed from the full bill
    // and the filter is applied later to the displayed transactions,
    // counts, and totals only.

    // For each account that doesn't have a configured `billing_closing_day`
    // in metadata, infer one from the Pluggy billId clusters in the data.
    // This makes the report work out of the box on accounts that were never
    // explicitly configured, falling back to the explicit value when it
    // exists.
    let mut rows_by_account: BTreeMap<String, Vec<&CardClosedTransactionRow>> = BTreeMap::new();
    for row in &all_rows {
        rows_by_account
            .entry(row.account_id.clone())
            .or_default()
            .push(row);
    }
    for (acct, rows_ref) in &rows_by_account {
        if closing_days.contains_key(acct) {
            continue;
        }
        let owned: Vec<CardClosedTransactionRow> = rows_ref.iter().map(|r| (*r).clone()).collect();
        if let Some(d) = infer_closing_day_from_pluggy(&owned) {
            closing_days.insert(acct.clone(), d);
        }
    }

    // Group by (account_id, bill_id).  Resolution order:
    //   1. Account-level billing_closing_day (explicit or inferred) →
    //      synthetic cycle bill_id (best, because it consolidates the
    //      ~80% of Pluggy txs without a billId into the same bill as
    //      the ~20% that do have one).
    //   2. Pluggy's creditCardMetadata.billId (kept as a fallback for
    //      accounts where we couldn't infer a closing day).
    //   3. Calendar-month fallback (no-bill-YYYY-MM).
    type BillKey = (String, String);
    type BillBucket = Vec<CardClosedTransactionRow>;
    let mut bills: BTreeMap<BillKey, BillBucket> = BTreeMap::new();
    for row in &all_rows {
        let bill_id = closing_days
            .get(&row.account_id)
            .map(|d| compute_bill_id(row.transaction_date, *d))
            .or_else(|| extract_bill_id(&row.metadata_json))
            .unwrap_or_else(|| format!("no-bill-{}", row.transaction_date.format("%Y-%m")));
        bills
            .entry((row.account_id.clone(), bill_id))
            .or_default()
            .push(row.clone());
    }

    // Per account, list of bill candidates with the date we'll use to order
    // and classify them.
    struct BillCandidate {
        account_id: String,
        /// Last transaction date seen in this bill. Used for display ("closed
        /// on...") and as a fallback ordering key.
        max_date: NaiveDate,
        /// Authoritative cycle close date, derived from the synthetic
        /// bill_id format `cycle-YYYY-MM-DD` when available. When None we
        /// fall back to `max_date` for ordering.
        derived_close_date: Option<NaiveDate>,
        txs: Vec<CardClosedTransactionRow>,
    }
    /// Parse a synthetic bill_id of the form `cycle-YYYY-MM-DD` into the
    /// underlying close date. Returns None for Pluggy-real billIds or the
    /// no-bill fallback.
    fn parse_synthetic_close_date(bill_id: &str) -> Option<NaiveDate> {
        let rest = bill_id.strip_prefix("cycle-")?;
        NaiveDate::parse_from_str(rest, "%Y-%m-%d").ok()
    }
    let candidates: Vec<BillCandidate> = bills
        .into_iter()
        .map(|((account_id, bill_id), txs)| {
            let max_date = txs
                .iter()
                .map(|t| t.transaction_date)
                .max()
                .unwrap_or(today);
            let derived_close_date = parse_synthetic_close_date(&bill_id);
            BillCandidate {
                account_id,
                max_date,
                derived_close_date,
                txs,
            }
        })
        .collect();

    // Pick the right bill per card based on mode.
    let mut per_account_unique: BTreeMap<String, Vec<&BillCandidate>> = BTreeMap::new();
    for c in &candidates {
        per_account_unique
            .entry(c.account_id.clone())
            .or_default()
            .push(c);
    }
    let mut selected: Vec<&BillCandidate> = Vec::new();
    for bills_for_account in per_account_unique.values() {
        // Bills with a synthetic close_date split cleanly by today's date:
        // close_date <= today → closed; close_date > today → open. This is
        // the authoritative signal when `billing_closing_day` is set on the
        // account. For accounts/bills without a synthetic close_date (e.g.
        // Pluggy real billIds or the no-bill fallback) we fall back to
        // max_date ordering as before.
        let pick: Option<&BillCandidate> = match mode {
            CardsMode::Closed => {
                // Most recent bill whose close_date <= today.
                let with_close: Vec<&&BillCandidate> = bills_for_account
                    .iter()
                    .filter(|b| b.derived_close_date.is_some_and(|d| d <= today))
                    .collect();
                if !with_close.is_empty() {
                    with_close
                        .into_iter()
                        .max_by_key(|b| b.derived_close_date.unwrap_or(b.max_date))
                        .copied()
                } else {
                    // Fallback: sort by max_date desc and skip the latest
                    // (assumed to be the currently open cycle). If only one
                    // bucket exists (e.g. a new card), use it rather than
                    // silently omitting the account from the report.
                    let mut sorted: Vec<&BillCandidate> = bills_for_account.to_vec();
                    sorted.sort_by_key(|b| std::cmp::Reverse(b.max_date));
                    sorted.get(1).or_else(|| sorted.first()).copied()
                }
            }
            CardsMode::Next => {
                // Most recent bill whose close_date > today (the cycle still
                // accruing charges). Fall back to "bill with the latest
                // max_date" if no synthetic close dates are available.
                let with_close: Vec<&&BillCandidate> = bills_for_account
                    .iter()
                    .filter(|b| b.derived_close_date.is_some_and(|d| d > today))
                    .collect();
                if !with_close.is_empty() {
                    with_close
                        .into_iter()
                        .min_by_key(|b| b.derived_close_date.unwrap_or(b.max_date))
                        .copied()
                } else {
                    bills_for_account.iter().copied().max_by_key(|b| b.max_date)
                }
            }
            CardsMode::Specific => {
                // Pick the bill whose close_date (or max_date fallback) falls
                // in the target month.
                bills_for_account.iter().copied().find(|b| {
                    let key = b.derived_close_date.unwrap_or(b.max_date);
                    key.format("%Y-%m").to_string() == target_month
                })
            }
        };
        if let Some(b) = pick {
            selected.push(b);
        }
    }

    // Re-derive `target_month` from selected bills for display: use the
    // most recent max_date among the selected.
    let display_month = selected
        .iter()
        .map(|b| b.max_date)
        .max()
        .map(|d| d.format("%Y-%m").to_string())
        .unwrap_or_else(|| target_month.clone());

    // Collect rows from the selected bills. `full_rows` is the unfiltered
    // bill content used for payment matching and status inference; `rows`
    // is what we actually display (filtered to installments when the user
    // passed `--installments-only`).
    let full_rows: Vec<CardClosedTransactionRow> = selected
        .iter()
        .flat_map(|b| b.txs.iter().cloned())
        .collect();
    let rows: Vec<CardClosedTransactionRow> = if args.installments_only {
        full_rows
            .iter()
            .filter(|r| detect_installment_marker(r).is_some())
            .cloned()
            .collect()
    } else {
        full_rows.clone()
    };

    // Group rows by account_id for the visão geral section. Status uses the
    // full bill; counts/totals reflect the displayed (possibly filtered) set.
    let by_account_full = group_card_rows_by_account(&full_rows);
    let by_account = group_card_rows_by_account(&rows);

    // Map account_id → derived_close_date of the bill we selected for that
    // account, so the status section can use the *real* cycle close date
    // rather than approximating from the last-seen transaction.
    let selected_close_dates: BTreeMap<String, Option<NaiveDate>> = selected
        .iter()
        .map(|b| (b.account_id.clone(), b.derived_close_date))
        .collect();

    // For payment inference: gather all checking-account debits classified
    // as bill payments in a window covering the cycle close dates onward.
    let pay_window_start = window_start;
    let pay_window_end = today + Duration::days(20);
    // Identify bill payments by either:
    //  (a) category_id resembles `credit-card-payment` / `pagamento-fatura`, or
    //  (b) description starts with "Pagamento de fatura" (Pluggy's canonical
    //      text for an outgoing CC payment from checking). Many setups don't
    //      classify these (category_id stays NULL), so description match is
    //      the most robust signal.
    let payment_candidates: Vec<TransactionRecord> = store
        .transactions_in_date_range(None, pay_window_start, pay_window_end)
        .await?
        .into_iter()
        .filter(|tx| {
            if tx.amount >= Decimal::ZERO {
                return false;
            }
            if tx.category_id.as_deref().is_some_and(|c| {
                c.contains("credit-card-payment") || c.contains("pagamento-fatura")
            }) {
                return true;
            }
            let desc_lower = tx.raw_description.to_lowercase();
            desc_lower.contains("pagamento de fatura")
                || desc_lower.contains("pagamento cart")
                || desc_lower.contains("pagamento de cart")
                || desc_lower.contains("nubank pagamento")
        })
        .collect();

    // Build per-account report. We iterate `by_account_full` so that the
    // status (paid/open/overdue) reflects the real bill — even when the
    // user passed `--installments-only`, which only shrinks the displayed
    // totals, not the underlying bill.
    let (accounts_report, grand_total) = build_cards_account_reports(
        &by_account_full,
        &by_account,
        &selected_close_dates,
        &payment_candidates,
        mode,
        today,
        args.installments_only,
    );

    let report = CardsReport {
        month_ref: display_month.clone(),
        accounts: accounts_report,
        grand_total,
    };

    if format.structured_output() {
        // Raw output: include both summary and the full transaction list so
        // agents have everything they need.
        let payload = serde_json::json!({
            "summary": report,
            "transactions": rows,
            // Debug: expose every candidate bill so a tooling caller can
            // verify the per-card grouping logic, not just the one we
            // selected. Will be removed once the report is stable.
            "all_candidates": candidates.iter().map(|b| {
                serde_json::json!({
                    "account_id": b.account_id,
                    "min_date": b.txs.iter().map(|t| t.transaction_date).min().map(|d| d.to_string()),
                    "max_date": b.max_date.to_string(),
                    "derived_close_date": b.derived_close_date.map(|d| d.to_string()),
                    "transaction_count": b.txs.len(),
                    "total": b.txs.iter().map(|t| t.amount.abs()).sum::<Decimal>().to_string(),
                    "bill_id_sample": b.txs.first().and_then(|t| extract_bill_id(&t.metadata_json)),
                })
            }).collect::<Vec<_>>(),
        });
        print_report_output(format, &payload)?;
        return Ok(());
    }

    // ─── Human output ──────────────────────────────────────────────────────
    let mode_suffix = match mode {
        CardsMode::Closed => " (fechado)",
        CardsMode::Next => " (em curso)",
        CardsMode::Specific => "",
    };
    let installments_suffix = if args.installments_only {
        " · só parceladas"
    } else {
        ""
    };
    println!(
        "💳 {}",
        bold(&format!(
            "Cartões · {}{}{}",
            month_label(&display_month),
            mode_suffix,
            installments_suffix
        ))
    );
    println!();

    if rows.is_empty() {
        println!("_(sem lançamentos de cartão no período)_");
        return Ok(());
    }

    // Visão geral
    println!("{}", subsection_header("Visão geral"));
    for acct in &report.accounts {
        let s = &acct.status;
        let (emoji, status_text) = match s.state {
            "paid" => (
                "🟢",
                format!(
                    "pago {}",
                    s.paid_on
                        .map(human_format::short_date)
                        .unwrap_or_else(|| "—".to_string())
                ),
            ),
            "open" => (
                "🟡",
                format!("em aberto · vence {}", human_format::short_date(s.due_date)),
            ),
            "overdue" => (
                "🔴",
                format!("ATRASADO · vencia {}", human_format::short_date(s.due_date)),
            ),
            "partial" => {
                let days_to_close = (s.close_date - today).num_days();
                let when = if days_to_close > 0 {
                    format!("fecha em ~{days_to_close}d")
                } else {
                    "fechando agora".to_string()
                };
                ("🟡", format!("em curso · {when}"))
            }
            _ => ("⚪", "status desconhecido".to_string()),
        };
        println!(
            "{} {} · {} ({} lanç) · {}",
            emoji,
            bold(&acct.account_id),
            human_format::brl_signed(-s.total),
            acct.transaction_count,
            status_text,
        );
    }
    println!(
        "{}: {}",
        bold("Total"),
        human_format::brl_signed(-report.grand_total)
    );
    println!();

    // Gastos por categoria — family → subcategory → transactions
    println!("{}", subsection_header("Gastos por categoria"));
    let mut by_family: BTreeMap<String, Vec<&CardClosedTransactionRow>> = BTreeMap::new();
    for row in &rows {
        let family = human_format::category_family(row.category_id.as_deref())
            .unwrap_or_else(|| "sem-categoria".to_string());
        by_family.entry(family).or_default().push(row);
    }
    let mut family_totals: Vec<(String, Decimal, Vec<&CardClosedTransactionRow>)> = by_family
        .into_iter()
        .map(|(f, list)| {
            // Signed sum: refunds (positive) net against charges (negative)
            // so the category total reflects the actual cost the user paid
            // in that category for the cycle.
            let net: Decimal = list.iter().map(|r| r.amount).sum();
            (f, net, list)
        })
        .collect();
    // Sort by absolute net descending — categories with the biggest
    // movement (positive or negative) come first.
    family_totals.sort_by_key(|(_, net, _)| std::cmp::Reverse(net.abs()));

    const TX_LIMIT: usize = 5;

    for (family, family_net, list) in &family_totals {
        let repr_cat = list.first().and_then(|r| r.category_id.as_deref());
        let emoji = human_format::category_emoji(repr_cat, Some(*family_net));
        let label = human_format::family_label(family);
        println!(
            "{} {} · {} ({} lanç)",
            emoji,
            bold(&label),
            human_format::brl_signed(*family_net),
            list.len(),
        );

        // Group this family's rows by subcategory. Subcategory comes from
        // the part after the first `:` in category_id. Pluggy-default ids
        // (no `:`) bucket under "—".
        let mut by_sub: BTreeMap<String, Vec<&&CardClosedTransactionRow>> = BTreeMap::new();
        for row in list {
            let sub = row
                .category_id
                .as_deref()
                .and_then(|c| c.split_once(':').map(|(_, s)| s.to_string()))
                .unwrap_or_else(|| "—".to_string());
            by_sub.entry(sub).or_default().push(row);
        }

        // If there's only one subcategory bucket (or only "—"), skip the
        // intermediate header and list transactions directly under the
        // family. Otherwise render each subcategory as a nested section.
        let render_subs = by_sub.len() > 1 || !by_sub.contains_key("—");

        if render_subs {
            let mut sub_buckets: Vec<(String, Decimal, Vec<&&CardClosedTransactionRow>)> = by_sub
                .into_iter()
                .map(|(name, items)| {
                    let net: Decimal = items.iter().map(|r| r.amount).sum();
                    (name, net, items)
                })
                .collect();
            sub_buckets.sort_by_key(|(_, net, _)| std::cmp::Reverse(net.abs()));

            for (sub_name, sub_net, sub_items) in sub_buckets {
                let sub_label = if sub_name == "—" {
                    "sem subcategoria".to_string()
                } else {
                    sub_name.replace('-', " ")
                };
                println!(
                    "  ↳ {} · {} ({} lanç)",
                    bold(&sub_label),
                    human_format::brl_signed(sub_net),
                    sub_items.len(),
                );
                let mut sorted_sub = sub_items.clone();
                sorted_sub.sort_by_key(|r| std::cmp::Reverse(r.amount.abs()));
                let take_n = if args.all { sorted_sub.len() } else { TX_LIMIT };
                for tx in sorted_sub.iter().take(take_n) {
                    println!(
                        "      • {} · {} ({})",
                        human_format::short_description(&tx.description),
                        human_format::brl_signed(tx.amount),
                        human_format::short_date(tx.transaction_date),
                    );
                }
                if !args.all && sorted_sub.len() > TX_LIMIT {
                    println!(
                        "      _… mais {} {}_",
                        sorted_sub.len() - TX_LIMIT,
                        if sorted_sub.len() - TX_LIMIT == 1 {
                            "lançamento"
                        } else {
                            "lançamentos"
                        }
                    );
                }
            }
        } else {
            // Single bucket (only "—") — list flat under family.
            let mut sorted_list: Vec<&&CardClosedTransactionRow> = list.iter().collect();
            sorted_list.sort_by_key(|r| std::cmp::Reverse(r.amount.abs()));
            let take_n = if args.all {
                sorted_list.len()
            } else {
                TX_LIMIT
            };
            for tx in sorted_list.iter().take(take_n) {
                println!(
                    "  • {} · {} ({})",
                    human_format::short_description(&tx.description),
                    human_format::brl_signed(tx.amount),
                    human_format::short_date(tx.transaction_date),
                );
            }
            if !args.all && sorted_list.len() > TX_LIMIT {
                println!(
                    "  _… mais {} {}_",
                    sorted_list.len() - TX_LIMIT,
                    if sorted_list.len() - TX_LIMIT == 1 {
                        "lançamento"
                    } else {
                        "lançamentos"
                    }
                );
            }
        }
        println!();
    }

    // Surface Pluggy-default classified transactions so the user can build
    // rules for them. Heuristic: any category_id that doesn't contain `:`
    // is a bare word that came from Pluggy's default classifier (the user's
    // own rules consistently produce `family:subcategory` ids). They're
    // already remapped onto PT families for display by `category_family`,
    // but their volume is worth highlighting so the user can close the loop.
    let pluggy_default: Vec<&CardClosedTransactionRow> = rows
        .iter()
        .filter(|r| {
            r.category_id
                .as_deref()
                .is_some_and(|c| !c.is_empty() && !c.contains(':'))
        })
        .collect();
    if !pluggy_default.is_empty() {
        let pluggy_net: Decimal = pluggy_default.iter().map(|r| r.amount).sum();
        println!(
            "💡 {} lanç auto-classificados pela Pluggy ({}) — criar regra pra consolidar",
            pluggy_default.len(),
            human_format::brl_signed(pluggy_net)
        );
        println!();
    }

    Ok(())
}

// Small helpers used only by report_cards.

trait NaiveDateSat {
    fn saturating_sub_signed_unsafe(self) -> NaiveDate;
}
impl NaiveDateSat for NaiveDate {
    fn saturating_sub_signed_unsafe(self) -> NaiveDate {
        // Subtract 7 days as a heuristic "start of payment-search window".
        self.checked_sub_signed(Duration::days(7)).unwrap_or(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_human_review, effective_review_human_limit, merge_transactions, parse_closing_day,
        report_output_format, resolve_sync_from, write_csv_from_value, Cli, Commands,
        HumanReviewPatch, MergeTransactionsArgs, ReportCommand, ReportOutputFormat,
    };
    use chrono::NaiveDate;
    use clap::Parser;
    use phai_core::models::TransactionRecord;
    use phai_core::storage::FinanceStore;
    use rust_decimal::Decimal;
    use serde_json::json;

    #[test]
    fn resolve_sync_from_prefers_explicit_date() {
        let latest = NaiveDate::from_ymd_opt(2026, 3, 20);
        assert_eq!(
            resolve_sync_from(Some("2026-03-01"), Some("2025-12-01"), latest).unwrap(),
            "2026-03-01"
        );
    }

    #[test]
    fn resolve_sync_from_uses_latest_seen_with_lookback() {
        let latest = NaiveDate::from_ymd_opt(2026, 3, 27);
        assert_eq!(
            resolve_sync_from(None, Some("2025-12-01"), latest).unwrap(),
            "2026-03-13"
        );
    }

    #[test]
    fn resolve_sync_from_never_goes_before_configured_start() {
        let latest = NaiveDate::from_ymd_opt(2025, 12, 10);
        assert_eq!(
            resolve_sync_from(None, Some("2025-12-01"), latest).unwrap(),
            "2025-12-01"
        );
    }

    #[test]
    fn effective_review_human_limit_defaults_and_override() {
        assert_eq!(effective_review_human_limit(None), 10);
        assert_eq!(effective_review_human_limit(Some(42)), 42);
    }

    #[test]
    fn parse_closing_day_derives_from_due_date_metadata() {
        assert_eq!(
            parse_closing_day(&json!({
                "raw": {
                    "creditData": {
                        "balanceDueDate": "2026-05-18"
                    }
                }
            })),
            Some(11)
        );
        assert_eq!(
            parse_closing_day(&json!({
                "billing_due_day": 5
            })),
            Some(26)
        );
        assert_eq!(
            parse_closing_day(&json!({
                "billing_closing_day": "10",
                "billing_due_day": 18
            })),
            Some(10)
        );
    }

    #[test]
    fn report_csv_flag_selects_csv_output() {
        let cli = Cli::try_parse_from(["phai", "report", "monthly-spend", "--csv"]).unwrap();
        let Some(Commands::Report {
            csv,
            command: ReportCommand::MonthlySpend(args),
        }) = cli.command
        else {
            panic!("expected report monthly-spend command");
        };
        let format = report_output_format(csv, args.structured_output()).unwrap();
        assert_eq!(format, ReportOutputFormat::Csv);
        assert!(format.structured_output());
    }

    #[test]
    fn csv_writer_escapes_commas_quotes_and_nulls() {
        let rows = json!([
            {
                "description": "A, B",
                "amount": "-10.00",
            },
            {
                "description": "Quote \"x\"",
                "amount": "2.50",
                "category": null,
            }
        ]);
        let mut out = Vec::new();
        write_csv_from_value(&rows, &mut out).unwrap();
        let rendered = String::from_utf8(out).unwrap();

        assert_eq!(
            rendered,
            "amount,category,description\n-10.00,,\"A, B\"\n2.50,,\"Quote \"\"x\"\"\"\n"
        );
    }

    fn sample_review_row(category_id: &str) -> TransactionRecord {
        let now = chrono::Utc::now();
        TransactionRecord {
            transaction_id: "tx-1".to_string(),
            account_id: None,
            transaction_date: NaiveDate::from_ymd_opt(2026, 5, 1).unwrap(),
            raw_description: "COMPRA MERCADO".to_string(),
            description: None,
            merchant_name: None,
            purpose: None,
            amount: Decimal::new(-4200, 2),
            amount_cents: None,
            tx_type: "debit".to_string(),
            category_id: Some(category_id.to_string()),
            category_source: "manual".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: "manual".to_string(),
            actor_id: "test".to_string(),
            idempotency_key: "tx-1".to_string(),
            metadata_json: json!({}),
            created_at: now,
            updated_at: now,
            enrichment_attempted_at: None,
        }
    }

    // ── Review save flow tests (headless) ─────────────────────────

    #[tokio::test]
    async fn local_sqlite_save_upserts_and_updates_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let config = phai_core::AppConfig {
            local_db_path: Some(db_path.clone()),
            ..Default::default()
        };

        let store = phai_core::storage::local::LocalStore::new(config.clone()).unwrap();
        phai_core::migrations::run_migrations(&store, &config)
            .await
            .unwrap();

        let tx = sample_review_row("alimentacao:mercado");
        store
            .upsert_transactions(std::slice::from_ref(&tx))
            .await
            .unwrap();
        let fetched = store.transaction_by_id(&tx.transaction_id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(
            fetched.unwrap().category_id,
            Some("alimentacao:mercado".to_string())
        );

        store
            .annotate_transaction(
                &tx.transaction_id,
                Some("transporte:app"),
                Some("manual"),
                None,
                "test",
                "test-key",
            )
            .await
            .unwrap();
        let updated = store
            .transaction_by_id(&tx.transaction_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(updated.category_id, Some("transporte:app".to_string()));
        assert_eq!(updated.category_source, "manual");
    }

    /// Regression (web C5): a human-review edit from the web bridge must persist
    /// the human description / merchant / purpose — not just the category — so
    /// the edit survives a page reload (which re-seeds from the store). The web
    /// `/api/events` flush lands here via `apply_human_review`.
    #[tokio::test]
    async fn human_review_persists_description_merchant_purpose() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let config = phai_core::AppConfig {
            local_db_path: Some(db_path.clone()),
            ..Default::default()
        };
        let store = phai_core::storage::local::LocalStore::new(config.clone()).unwrap();
        phai_core::migrations::run_migrations(&store, &config)
            .await
            .unwrap();

        let tx = sample_review_row("alimentacao:mercado");
        store
            .upsert_transactions(std::slice::from_ref(&tx))
            .await
            .unwrap();

        apply_human_review(
            &store,
            &config,
            &tx.transaction_id,
            HumanReviewPatch {
                description: Some("Zenilda Faxina".to_string()),
                merchant_name: Some("Zenilda".to_string()),
                purpose: Some("Faxina mensal".to_string()),
                category_id: Some("moradia:servicos".to_string()),
                commitment_tier: None,
            },
        )
        .await
        .unwrap();

        // What the web reseed (`/api/transactions`) would surface after reload.
        let got = store
            .transaction_by_id(&tx.transaction_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.description.as_deref(), Some("Zenilda Faxina"));
        assert_eq!(got.merchant_name.as_deref(), Some("Zenilda"));
        assert_eq!(got.purpose.as_deref(), Some("Faxina mensal"));
        assert_eq!(got.category_id.as_deref(), Some("moradia:servicos"));
    }

    #[tokio::test]
    async fn merge_transactions_keeps_pluggy_amount_and_copies_manual_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let config = phai_core::AppConfig {
            local_db_path: Some(db_path.clone()),
            actor_id: "test-actor".to_string(),
            ..Default::default()
        };
        let store = phai_core::storage::local::LocalStore::new(config.clone()).unwrap();
        phai_core::migrations::run_migrations(&store, &config)
            .await
            .unwrap();

        let mut manual = sample_review_row("eletronicos:reparo");
        manual.transaction_id = "manual_watch_fix".to_string();
        manual.idempotency_key = "manual-watch".to_string();
        manual.account_id = Some("felipe_cartao".to_string());
        manual.transaction_date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        manual.raw_description = "Conserto Apple Watch - Bluefix OS49281 (1/3)".to_string();
        manual.description = Some("Conserto Apple Watch - Bluefix OS49281 (1/3)".to_string());
        manual.merchant_name = Some("Bluefix".to_string());
        manual.purpose = Some("Reparo Apple Watch".to_string());
        manual.classifier_trace = Some("lançado manualmente antes da conciliação".to_string());
        manual.amount = Decimal::new(-39267, 2);
        manual.source = "manual".to_string();
        manual.category_source = "manual".to_string();

        let mut pluggy = sample_review_row("electronics");
        pluggy.transaction_id = "pluggy_watch_fix".to_string();
        pluggy.idempotency_key = "pluggy-watch".to_string();
        pluggy.account_id = Some("felipe_cartao".to_string());
        pluggy.transaction_date = NaiveDate::from_ymd_opt(2026, 6, 10).unwrap();
        pluggy.raw_description = "Vsstore".to_string();
        pluggy.description = Some("Vsstore".to_string());
        pluggy.amount = Decimal::new(-39268, 2);
        pluggy.source = "pluggy".to_string();
        pluggy.category_source = "pluggy".to_string();

        store
            .upsert_transactions(&[manual.clone(), pluggy.clone()])
            .await
            .unwrap();

        let plan = merge_transactions(
            &store,
            &config,
            &MergeTransactionsArgs {
                from: manual.transaction_id.clone(),
                into_transaction_id: pluggy.transaction_id.clone(),
                max_amount_diff: "0.01".to_string(),
                max_days: 0,
                allow_any_source: false,
                allow_non_pluggy_target: false,
                dry_run: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(plan.amount_diff, Decimal::new(1, 2));
        assert_eq!(
            plan.copied.category_id.as_deref(),
            Some("eletronicos:reparo")
        );
        assert!(store
            .transaction_by_id(&manual.transaction_id)
            .await
            .unwrap()
            .is_none());

        let merged = store
            .transaction_by_id(&pluggy.transaction_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(merged.amount, Decimal::new(-39268, 2));
        assert_eq!(merged.source, "pluggy");
        assert_eq!(
            merged.description.as_deref(),
            Some("Conserto Apple Watch - Bluefix OS49281 (1/3)")
        );
        assert_eq!(merged.merchant_name.as_deref(), Some("Bluefix"));
        assert_eq!(merged.purpose.as_deref(), Some("Reparo Apple Watch"));
        assert_eq!(merged.category_id.as_deref(), Some("eletronicos:reparo"));
        assert_eq!(merged.category_source, "manual");
        assert_eq!(store.count_rows("audit_log").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn merge_transactions_rejects_amount_diff_above_tolerance() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("test.db");
        let config = phai_core::AppConfig {
            local_db_path: Some(db_path.clone()),
            ..Default::default()
        };
        let store = phai_core::storage::local::LocalStore::new(config.clone()).unwrap();
        phai_core::migrations::run_migrations(&store, &config)
            .await
            .unwrap();

        let mut manual = sample_review_row("eletronicos:reparo");
        manual.transaction_id = "manual-watch".to_string();
        manual.idempotency_key = "manual-watch".to_string();
        manual.account_id = Some("card".to_string());
        manual.amount = Decimal::new(-39267, 2);
        manual.source = "manual".to_string();

        let mut pluggy = sample_review_row("electronics");
        pluggy.transaction_id = "pluggy-watch".to_string();
        pluggy.idempotency_key = "pluggy-watch".to_string();
        pluggy.account_id = Some("card".to_string());
        pluggy.amount = Decimal::new(-39268, 2);
        pluggy.source = "pluggy".to_string();

        store.upsert_transactions(&[manual, pluggy]).await.unwrap();

        let err = merge_transactions(
            &store,
            &config,
            &MergeTransactionsArgs {
                from: "manual-watch".to_string(),
                into_transaction_id: "pluggy-watch".to_string(),
                max_amount_diff: "0.00".to_string(),
                max_days: 7,
                allow_any_source: false,
                allow_non_pluggy_target: false,
                dry_run: false,
            },
        )
        .await
        .unwrap_err();

        assert!(err.to_string().contains("excede --max-amount-diff"));
        assert!(store
            .transaction_by_id("manual-watch")
            .await
            .unwrap()
            .is_some());
    }

    // ── E2E: Consistency between Local (SQLite) and Remote (BigQuery) ──
    // Simulates the full TUI save pipeline with two SQLite DBs playing the
    // roles of local cache and remote BigQuery.

    #[tokio::test]
    async fn e2e_local_save_replicates_to_remote() {
        // Setup: two independent SQLite DBs
        let tmp = tempfile::tempdir().unwrap();
        let local_path = tmp.path().join("local.db");
        let remote_path = tmp.path().join("remote.db");

        let local_cfg = phai_core::AppConfig {
            local_db_path: Some(local_path.clone()),
            ..Default::default()
        };
        let local = phai_core::storage::local::LocalStore::new(local_cfg.clone()).unwrap();
        phai_core::migrations::run_migrations(&local, &local_cfg)
            .await
            .unwrap();

        let remote_cfg = phai_core::AppConfig {
            local_db_path: Some(remote_path.clone()),
            ..Default::default()
        };
        let remote = phai_core::storage::local::LocalStore::new(remote_cfg.clone()).unwrap();
        phai_core::migrations::run_migrations(&remote, &remote_cfg)
            .await
            .unwrap();

        // Seed both with the same initial transaction
        let tx = sample_review_row("outros:geral");
        local
            .upsert_transactions(std::slice::from_ref(&tx))
            .await
            .unwrap();
        remote
            .upsert_transactions(std::slice::from_ref(&tx))
            .await
            .unwrap();

        // Simulate TUI save: update local, then replicate to remote
        let patch = HumanReviewPatch {
            description: Some("Descrição E2E".to_string()),
            merchant_name: Some("Merchant E2E".to_string()),
            purpose: None,
            category_id: Some("alimentacao:restaurantes".to_string()),
            commitment_tier: None,
        };

        // Step 1: apply to local (fast, synchronous from TUI perspective)
        apply_human_review(&local, &local_cfg, &tx.transaction_id, patch.clone())
            .await
            .unwrap();

        // Step 2: replicate to remote (background sync)
        // With retry simulated
        let mut synced = false;
        for attempt in 0..3 {
            match apply_human_review(&remote, &remote_cfg, &tx.transaction_id, patch.clone()).await
            {
                Ok(_) => {
                    synced = true;
                    break;
                }
                Err(e) if attempt < 2 => {
                    eprintln!("retry {}/2: {e}", attempt + 1);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                Err(e) => {
                    panic!("sync falhou após 3 tentativas: {e}");
                }
            }
        }
        assert!(synced, "replicação deveria ter funcionado");

        // Step 3: verify consistency — both DBs have the same data
        let local_row = local
            .transaction_by_id(&tx.transaction_id)
            .await
            .unwrap()
            .unwrap();
        let remote_row = remote
            .transaction_by_id(&tx.transaction_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(local_row.description, remote_row.description);
        assert_eq!(local_row.merchant_name, remote_row.merchant_name);
        assert_eq!(local_row.category_id, remote_row.category_id);
        assert_eq!(local_row.category_source, remote_row.category_source);
        assert_eq!(local_row.category_source, "manual");
    }

    #[tokio::test]
    async fn e2e_bulk_save_consistency() {
        let tmp = tempfile::tempdir().unwrap();
        let local_path = tmp.path().join("local.db");
        let remote_path = tmp.path().join("remote.db");

        let mut cfg = phai_core::AppConfig {
            local_db_path: Some(local_path.clone()),
            ..Default::default()
        };
        let local = phai_core::storage::local::LocalStore::new(cfg.clone()).unwrap();
        phai_core::migrations::run_migrations(&local, &cfg)
            .await
            .unwrap();

        cfg.local_db_path = Some(remote_path.clone());
        let remote = phai_core::storage::local::LocalStore::new(cfg.clone()).unwrap();
        phai_core::migrations::run_migrations(&remote, &cfg)
            .await
            .unwrap();

        // Create 5 transactions with the same raw_description (bulk scenario)
        let mut txs = Vec::new();
        for i in 0..5 {
            let mut tx = sample_review_row("outros:geral");
            tx.transaction_id = format!("bulk-tx-{i}");
            txs.push(tx);
        }
        local.upsert_transactions(&txs).await.unwrap();
        remote.upsert_transactions(&txs).await.unwrap();

        let patch = HumanReviewPatch {
            description: None,
            merchant_name: None,
            purpose: None,
            category_id: Some("transporte:aplicativo".to_string()),
            commitment_tier: None,
        };

        // Apply to all 5 locally
        for tx in &txs {
            apply_human_review(&local, &cfg, &tx.transaction_id, patch.clone())
                .await
                .unwrap();
        }

        // Replicate to remote
        for tx in &txs {
            apply_human_review(&remote, &cfg, &tx.transaction_id, patch.clone())
                .await
                .unwrap();
        }

        // Verify all 5 are consistent
        for tx in &txs {
            let l = local
                .transaction_by_id(&tx.transaction_id)
                .await
                .unwrap()
                .unwrap();
            let r = remote
                .transaction_by_id(&tx.transaction_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(l.category_id, r.category_id);
            assert_eq!(l.category_id, Some("transporte:aplicativo".to_string()));
        }
    }
}
