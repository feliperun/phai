//! `phai sheets sync`: keep the household planning sheet in step (ADR-0041).
//!
//! Each run first imports what a person changed on the month tabs (edits
//! become forecast changes, with audit events), then rewrites every month
//! tab from phai's current state: the previous month, the current one and
//! the next ones up to the configured horizon. Runs on demand and after
//! every Pluggy sync when `planning_sheet_id` is configured.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, Months, NaiveDate, Utc};
use clap::{Args, Subcommand};
use phai_core::google_sheets::{HttpSheets, SheetsApi, TabFormat, TabWrite};
use phai_core::migrations::run_migrations;
use phai_core::planning_sheet::{
    build_month_tab, diff_tab, is_phai_tab, managed_tabs, manifest_entries, month_from_title,
    parse_manifest, parse_tab, render_lists, render_manifest, render_tab, tab_title, CreditCard,
    ManifestEntry, MonthInputs, MonthTab, ParsedTab, SheetEdit, LISTS_TAB, MANIFEST_TAB, SIGNATURE,
};
use phai_core::storage::{open_store, FinanceStore};
use phai_core::{AccountRecord, AppConfig, CardSummaryRow, CategoryRecord, MonthlySpendRow};
use rust_decimal::Decimal;
use serde_json::Value;

use crate::human_format::{capitalize_first, family_label};
use crate::load_config;
use crate::planning_sheet_apply::{flush, WorkingSet, WorkingSetInit};

/// Months carried from the current one when the config does not say.
const DEFAULT_MONTHS: u32 = 12;
/// How far back series members are looked up when extending them.
const SERIES_LOOKBACK_MONTHS: u32 = 12;

#[derive(Subcommand)]
pub(crate) enum SheetsCommand {
    /// Import the edits made on the planning sheet, then rewrite its month tabs.
    Sync(SheetsSyncArgs),
}

#[derive(Args)]
pub(crate) struct SheetsSyncArgs {
    /// List the edits that would be imported, without writing phai or the sheet.
    #[arg(long)]
    dry_run: bool,
    /// Month tabs from the current month on (default: `planning_sheet_months` or 12).
    #[arg(long)]
    months: Option<u32>,
}

pub(crate) struct SyncOptions {
    pub(crate) today: NaiveDate,
    pub(crate) now: DateTime<Utc>,
    pub(crate) months: u32,
    pub(crate) dry_run: bool,
}

#[derive(Debug, Default)]
pub(crate) struct SheetSyncReport {
    /// Edits read from the sheet (imported, or pending in a dry run).
    pub(crate) edits: Vec<(String, SheetEdit)>,
    pub(crate) forecasts_written: usize,
    pub(crate) tabs_written: usize,
    pub(crate) warnings: Vec<String>,
}

impl SheetSyncReport {
    pub(crate) fn one_line(&self) -> String {
        format!(
            "Planilha de planejamento: {} edição(ões) importada(s), {} previsão(ões) gravada(s), {} aba(s) atualizada(s)",
            self.edits.len(),
            self.forecasts_written,
            self.tabs_written
        )
    }
}

pub(crate) async fn run(command: SheetsCommand) -> Result<()> {
    let SheetsCommand::Sync(args) = command;
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let api = sheets_client(&config)?;
    let options = SyncOptions {
        today: Local::now().date_naive(),
        now: Utc::now(),
        months: args
            .months
            .or(config.planning_sheet_months)
            .unwrap_or(DEFAULT_MONTHS),
        dry_run: args.dry_run,
    };
    let report = sync_planning_sheet(store.as_ref(), &api, &config, &options).await?;
    for (tab, edit) in &report.edits {
        println!("  {tab}: {}", describe(edit));
    }
    for warning in &report.warnings {
        println!("  aviso: {warning}");
    }
    if args.dry_run {
        println!(
            "\n{} edição(ões) seriam importadas (dry-run: nada foi gravado).",
            report.edits.len()
        );
    } else {
        println!("\n{}", report.one_line());
    }
    Ok(())
}

/// The post-sync hook: `None` when no planning sheet is configured,
/// otherwise the line to print. Never fails the sync.
pub(crate) async fn sync_after_pluggy(
    store: &dyn FinanceStore,
    config: &AppConfig,
) -> Option<String> {
    config.planning_sheet_id.as_ref()?;
    let options = SyncOptions {
        today: Local::now().date_naive(),
        now: Utc::now(),
        months: config.planning_sheet_months.unwrap_or(DEFAULT_MONTHS),
        dry_run: false,
    };
    let result = match sheets_client(config) {
        Ok(api) => sync_planning_sheet(store, &api, config, &options).await,
        Err(err) => Err(err),
    };
    Some(match result {
        Ok(report) => report.one_line(),
        Err(err) => format!("aviso: planilha de planejamento não atualizada: {err:#}"),
    })
}

fn sheets_client(config: &AppConfig) -> Result<HttpSheets> {
    let sheet_id = config.planning_sheet_id.as_deref().context(
        "planning_sheet_id ausente no config.toml: informe o id da planilha \
         (o trecho entre /d/ e /edit na URL)",
    )?;
    let service_account = config
        .service_account_path()
        .context("a planilha de planejamento usa a service account do BigQuery")?;
    HttpSheets::new(service_account.to_path_buf(), sheet_id)
}

pub(crate) async fn sync_planning_sheet(
    store: &dyn FinanceStore,
    api: &dyn SheetsApi,
    config: &AppConfig,
    options: &SyncOptions,
) -> Result<SheetSyncReport> {
    let months = horizon(options.today, options.months)?;
    let titles = api.tab_titles().await?;
    let (manifest, tabs) = read_sheet(api, &titles).await?;
    let mut report = SheetSyncReport::default();
    let mut edits_by_month = Vec::new();
    for (month, parsed) in &tabs {
        let title = tab_title(*month);
        let diff = diff_tab(&title, parsed, &manifest);
        report.warnings.extend(diff.warnings);
        report
            .edits
            .extend(diff.edits.iter().map(|e| (title.clone(), e.clone())));
        edits_by_month.push((*month, diff.edits));
    }
    if options.dry_run {
        return Ok(report);
    }

    let context = SheetContext::load(store, config).await?;
    let (first, last) = (months[0], months[months.len() - 1]);
    let window_start = first
        .checked_sub_months(Months::new(SERIES_LOOKBACK_MONTHS))
        .unwrap_or(first);
    let window_end = last
        .checked_add_months(Months::new(1))
        .and_then(|d| d.pred_opt())
        .unwrap_or(last);
    let forecasts = store
        .list_forecasts(None, Some(window_start), Some(window_end))
        .await
        .context("list_forecasts")?;
    let mut set = WorkingSet::new(WorkingSetInit {
        forecasts,
        actor_id: config.actor_id.clone(),
        now: options.now,
        categories: context.category_ids.clone(),
        accounts: context.account_ids.clone(),
        cards: context.cards.clone(),
        horizon_end: last,
    });
    for (month, edits) in &edits_by_month {
        set.apply(*month, edits);
    }
    set.extend_series();
    report.warnings.append(&mut set.warnings);
    report.forecasts_written = flush(store, &set).await?;

    let rendered = render_months(store, &context, &set, &months, options.today).await?;
    let (foreign, adopted) = unclaimed_tabs(api, &titles, &manifest, &months).await?;
    for title in &foreign {
        report.warnings.push(format!(
            "a aba \"{title}\" já existia e não foi criada pelo phai; renomeie-a para o phai assumir esse mês"
        ));
    }
    for title in &adopted {
        report.warnings.push(format!(
            "a aba \"{title}\" é do phai mas não estava no registro _phai; foi reescrita sem importar edições"
        ));
    }
    let writes = tab_writes(&rendered, &foreign, &context, options.now);
    report.tabs_written = writes.len().saturating_sub(2);
    api.write_tabs(&writes).await?;
    Ok(report)
}

/// The previous month, the current one and the next `months - 1`.
fn horizon(today: NaiveDate, months: u32) -> Result<Vec<NaiveDate>> {
    let current =
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1).context("mês atual inválido")?;
    let start = current
        .checked_sub_months(Months::new(1))
        .context("mês anterior inválido")?;
    (0..=months.max(1))
        .map(|offset| {
            start
                .checked_add_months(Months::new(offset))
                .context("mês do horizonte inválido")
        })
        .collect()
}

/// The manifest and every month tab it says phai wrote that still exists.
async fn read_sheet(
    api: &dyn SheetsApi,
    titles: &[String],
) -> Result<(Vec<ManifestEntry>, Vec<(NaiveDate, ParsedTab)>)> {
    if !titles.iter().any(|t| t == MANIFEST_TAB) {
        return Ok((Vec::new(), Vec::new()));
    }
    let manifest = parse_manifest(
        api.read_tabs(&[MANIFEST_TAB.to_string()])
            .await?
            .first()
            .map(Vec::as_slice)
            .unwrap_or_default(),
    );
    let mut managed: Vec<(NaiveDate, String)> = managed_tabs(&manifest)
        .into_iter()
        .filter(|t| titles.contains(t))
        .filter_map(|t| month_from_title(&t).map(|m| (m, t)))
        .collect();
    managed.sort();
    let names: Vec<String> = managed.iter().map(|(_, t)| t.clone()).collect();
    let values = api.read_tabs(&names).await?;
    let tabs = managed
        .into_iter()
        .zip(values)
        .map(|((month, _), grid)| (month, parse_tab(&grid)))
        .collect();
    Ok((manifest, tabs))
}

/// Month tabs in the horizon that exist but the manifest does not list:
/// `(foreign, adopted)`. A tab carrying phai's note in `C1` was written by
/// phai (the manifest lost it) and is rewritten without importing edits; any
/// other tab belongs to the person and is left alone.
async fn unclaimed_tabs(
    api: &dyn SheetsApi,
    titles: &[String],
    manifest: &[ManifestEntry],
    months: &[NaiveDate],
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let managed = managed_tabs(manifest);
    let unclaimed: Vec<String> = months
        .iter()
        .map(|m| tab_title(*m))
        .filter(|t| titles.contains(t) && !managed.contains(t))
        .collect();
    let grids = api.read_tabs(&unclaimed).await?;
    let (adopted, foreign): (Vec<_>, Vec<_>) = unclaimed
        .into_iter()
        .zip(grids)
        .partition(|(_, grid)| is_phai_tab(grid));
    Ok((
        foreign.into_iter().map(|(t, _)| t).collect(),
        adopted.into_iter().map(|(t, _)| t).collect(),
    ))
}

/// What phai knows beyond forecasts: categories, accounts and cards.
struct SheetContext {
    category_ids: BTreeSet<String>,
    category_labels: HashMap<String, String>,
    account_ids: BTreeSet<String>,
    cards: Vec<CreditCard>,
    internal: BTreeSet<String>,
}

impl SheetContext {
    async fn load(store: &dyn FinanceStore, config: &AppConfig) -> Result<Self> {
        let categories = store.get_categories().await.context("get_categories")?;
        let category_labels = categories
            .iter()
            .map(|c| (c.category_id.clone(), category_label(c)))
            .collect();
        let accounts = store.get_accounts().await.context("get_accounts")?;
        Ok(Self {
            category_ids: store
                .list_all_category_ids()
                .await
                .context("list_all_category_ids")?,
            category_labels,
            account_ids: accounts.iter().map(|a| a.account_id.clone()).collect(),
            cards: accounts
                .iter()
                .filter(|a| a.account_type == "credit" && a.status == "active")
                .map(|a| credit_card(a, config))
                .collect(),
            internal: store
                .internal_categories()
                .await
                .context("internal_categories")?,
        })
    }
}

/// `Alimentação › Restaurantes` from the category's family and its name.
fn category_label(category: &CategoryRecord) -> String {
    match category.parent_category_id.as_deref() {
        Some(parent) => format!(
            "{} › {}",
            family_label(parent),
            capitalize_first(&category.name)
        ),
        None => family_label(&category.category_id),
    }
}

fn credit_card(account: &AccountRecord, config: &AppConfig) -> CreditCard {
    let day = |key: &str| {
        let value = account.metadata_json.get(key)?;
        let day = match value {
            Value::String(s) => s.trim().parse::<u32>().ok()?,
            Value::Number(n) => u32::try_from(n.as_u64()?).ok()?,
            _ => return None,
        };
        (1..=31).contains(&day).then_some(day)
    };
    CreditCard {
        account_id: account.account_id.clone(),
        label: config
            .account_labels
            .get(&account.account_id)
            .cloned()
            .unwrap_or_else(|| account.label.clone()),
        closing_day: day("billing_closing_day"),
        due_day: day("billing_due_day"),
    }
}

async fn render_months(
    store: &dyn FinanceStore,
    context: &SheetContext,
    set: &WorkingSet,
    months: &[NaiveDate],
    today: NaiveDate,
) -> Result<Vec<MonthTab>> {
    let card_totals = store.card_summary(None).await.context("card_summary")?;
    let spend = store.monthly_spend(None).await.context("monthly_spend")?;
    let closed = set.closed_series();
    let forecasts: Vec<_> = set.forecasts.values().cloned().collect();
    Ok(months
        .iter()
        .map(|month| {
            let month_ref = month.format("%Y-%m").to_string();
            let in_month: Vec<_> = forecasts
                .iter()
                .filter(|f| {
                    f.due_date
                        .is_some_and(|d| d.format("%Y-%m").to_string() == month_ref)
                })
                .cloned()
                .collect();
            let totals: Vec<CardSummaryRow> = card_totals
                .iter()
                .filter(|t| t.month_ref == month_ref)
                .cloned()
                .collect();
            let category_spend = spend_in(&spend, &month_ref, &context.internal);
            build_month_tab(&MonthInputs {
                month: *month,
                today,
                forecasts: &in_month,
                cards: &context.cards,
                card_totals: &totals,
                category_spend: &category_spend,
                category_labels: &context.category_labels,
                closed_series: &closed,
            })
        })
        .collect())
}

/// Spend per category in the month, internal moves left out.
fn spend_in(
    rows: &[MonthlySpendRow],
    month_ref: &str,
    internal: &BTreeSet<String>,
) -> Vec<(String, Decimal)> {
    let mut totals: HashMap<String, Decimal> = HashMap::new();
    for row in rows.iter().filter(|r| r.month_ref == month_ref) {
        if internal.contains(&row.category_id) || row.category_id.is_empty() {
            continue;
        }
        *totals.entry(row.category_id.clone()).or_default() += row.expenses.abs();
    }
    totals.into_iter().collect()
}

fn tab_writes(
    tabs: &[MonthTab],
    foreign: &BTreeSet<String>,
    context: &SheetContext,
    now: DateTime<Utc>,
) -> Vec<TabWrite> {
    let label = format!(
        "{SIGNATURE} em {}. Edite Item, Previsto, Dia, Repete, Categoria, Conta e Obs.",
        now.with_timezone(&Local).format("%d/%m/%Y %H:%M")
    );
    let ours: Vec<&MonthTab> = tabs
        .iter()
        .filter(|t| !foreign.contains(&t.title()))
        .collect();
    let mut writes: Vec<TabWrite> = ours
        .iter()
        .map(|tab| {
            let rendered = render_tab(tab, &label);
            TabWrite {
                title: tab.title(),
                cells: rendered.cells,
                hidden: false,
                format: TabFormat::Month {
                    bold_rows: rendered.bold_rows,
                },
            }
        })
        .collect();
    let manifest: Vec<ManifestEntry> = ours.iter().flat_map(|t| manifest_entries(t)).collect();
    let categories: Vec<String> = context.category_ids.iter().cloned().collect();
    let accounts: Vec<String> = context.account_ids.iter().cloned().collect();
    writes.push(TabWrite {
        title: MANIFEST_TAB.to_string(),
        cells: render_manifest(&manifest),
        hidden: true,
        format: TabFormat::Plain,
    });
    writes.push(TabWrite {
        title: LISTS_TAB.to_string(),
        cells: render_lists(&categories, &accounts),
        hidden: true,
        format: TabFormat::Plain,
    });
    writes
}

fn describe(edit: &SheetEdit) -> String {
    let money = |v: Option<Decimal>| v.map_or("sem valor".to_string(), |v| format!("{v:.2}"));
    match edit {
        SheetEdit::Create { section, row } => format!(
            "nova linha em {}: {} {}{}",
            section.title(),
            row.item,
            money(row.planned),
            if row.repeats { " (mensal)" } else { "" }
        ),
        SheetEdit::Update { before, after, .. } => {
            format!(
                "alterada: {} {} → {} {}",
                before.item,
                money(before.planned),
                after.item,
                money(after.planned)
            )
        }
        SheetEdit::Remove { key } => format!("removida: {key}"),
        SheetEdit::CardPlan {
            account_id,
            planned,
        } => format!("fatura planejada de {account_id}: {}", money(*planned)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use phai_core::planning_sheet::Cell;
    use phai_core::ForecastRecord;
    use serde_json::json;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An in-memory spreadsheet that hands back what was written the way the
    /// Sheets API does (numbers as numbers, everything else as text).
    #[derive(Default)]
    struct FakeSheet {
        tabs: RefCell<BTreeMap<String, Vec<Vec<Value>>>>,
    }

    impl FakeSheet {
        fn grid(&self, title: &str) -> Vec<Vec<Value>> {
            self.tabs.borrow().get(title).cloned().unwrap_or_default()
        }

        fn edit(&self, title: &str, f: impl FnOnce(&mut Vec<Vec<Value>>)) {
            f(self.tabs.borrow_mut().get_mut(title).unwrap());
        }
    }

    #[async_trait(?Send)]
    impl SheetsApi for FakeSheet {
        async fn tab_titles(&self) -> Result<Vec<String>> {
            Ok(self.tabs.borrow().keys().cloned().collect())
        }

        async fn read_tabs(&self, titles: &[String]) -> Result<Vec<Vec<Vec<Value>>>> {
            Ok(titles.iter().map(|t| self.grid(t)).collect())
        }

        async fn write_tabs(&self, tabs: &[TabWrite]) -> Result<()> {
            for tab in tabs {
                let grid = tab
                    .cells
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
                    .collect();
                self.tabs.borrow_mut().insert(tab.title.clone(), grid);
            }
            Ok(())
        }
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    async fn seeded_store() -> (tempfile::TempDir, AppConfig, Box<dyn FinanceStore>) {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            local_db_path: Some(dir.path().join("test.db")),
            actor_id: "test-actor".into(),
            ..AppConfig::default()
        };
        let store = open_store(&config).await.unwrap();
        run_migrations(store.as_ref(), &config).await.unwrap();
        let now = Utc::now();
        let account = |id: &str, kind: &str, meta: Value| AccountRecord {
            account_id: id.to_string(),
            owner: "casa".to_string(),
            account_type: kind.to_string(),
            bank: "banco".to_string(),
            label: format!("Conta {id}"),
            pluggy_account_id: None,
            pluggy_item_id: None,
            status: "active".to_string(),
            actor_id: "test-actor".to_string(),
            idempotency_key: format!("acc-{id}"),
            metadata_json: meta,
            created_at: now,
            updated_at: now,
        };
        store
            .upsert_accounts(&[
                account("conta_corrente", "checking", json!({})),
                account(
                    "cartao_a",
                    "credit",
                    json!({"billing_closing_day": "10", "billing_due_day": "17"}),
                ),
            ])
            .await
            .unwrap();
        store
            .upsert_categories(&[CategoryRecord {
                category_id: "moradia:aluguel".to_string(),
                name: "Aluguel".to_string(),
                parent_category_id: None,
                metadata_json: json!({}),
                actor_id: "test-actor".to_string(),
                updated_at: now,
            }])
            .await
            .unwrap();
        store
            .upsert_forecasts(&[ForecastRecord {
                forecast_id: "aluguel-maio".to_string(),
                due_date: Some(d(2030, 5, 5)),
                description: "Aluguel".to_string(),
                amount: Decimal::new(-5000, 0),
                category_id: Some("moradia:aluguel".to_string()),
                account_id: Some("conta_corrente".to_string()),
                status: "ativo".to_string(),
                recurrence: None,
                actor_id: "test-actor".to_string(),
                idempotency_key: "k-aluguel".to_string(),
                metadata_json: json!({}),
                created_at: now,
                updated_at: now,
                template_id: None,
                realized_transaction_id: None,
                realized_at: None,
            }])
            .await
            .unwrap();
        (dir, config, store)
    }

    fn options() -> SyncOptions {
        SyncOptions {
            today: d(2030, 5, 10),
            now: Utc::now(),
            months: 2,
            dry_run: false,
        }
    }

    fn row_index(grid: &[Vec<Value>], item: &str) -> usize {
        grid.iter()
            .position(|r| r.first() == Some(&json!(item)))
            .unwrap_or_else(|| panic!("no row {item}"))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn first_sync_writes_the_horizon_and_its_bookkeeping() {
        let (_dir, config, store) = seeded_store().await;
        let sheet = FakeSheet::default();
        let report = sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        assert_eq!(report.tabs_written, 3);
        let titles = sheet.tab_titles().await.unwrap();
        for title in [
            "ABRIL 2030",
            "MAIO 2030",
            "JUNHO 2030",
            MANIFEST_TAB,
            LISTS_TAB,
        ] {
            assert!(titles.contains(&title.to_string()), "{title} missing");
        }
        let may = sheet.grid("MAIO 2030");
        let rent = &may[row_index(&may, "Aluguel")];
        assert_eq!(rent[1], json!(5000));
        assert_eq!(rent[9], json!("aluguel-maio"));
        let card = &may[row_index(&may, "Conta cartao_a")];
        assert_eq!(card[4], json!(17), "card row carries its due day");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn edits_on_the_sheet_become_forecasts_and_a_quiet_resync_changes_nothing() {
        let (_dir, config, store) = seeded_store().await;
        let sheet = FakeSheet::default();
        sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        sheet.edit("MAIO 2030", |grid| {
            let rent = row_index(grid, "Aluguel");
            grid[rent][1] = json!(5150);
            // The empty line under the expense rows takes a new monthly item.
            let typed = row_index(grid, "Total despesas") - 1;
            grid[typed] = vec![
                json!("Natação"),
                json!("440,00"),
                json!(""),
                json!(""),
                json!(12),
                json!("mensal"),
            ];
            let card = row_index(grid, "Conta cartao_a");
            grid[card][1] = json!(4000);
        });

        let report = sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        assert_eq!(report.edits.len(), 3, "{:?}", report.edits);
        let rent = store.get_forecast("aluguel-maio").await.unwrap().unwrap();
        assert_eq!(rent.amount, Decimal::new(-5150, 0));
        let all = store.list_forecasts(None, None, None).await.unwrap();
        let swims: Vec<_> = all.iter().filter(|f| f.description == "Natação").collect();
        assert_eq!(swims.len(), 2, "May and June");
        assert!(swims.iter().all(|f| f.amount == Decimal::new(-440, 0)));
        let card = store
            .get_forecast("sheet-card-cartao_a-203005")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(card.amount, Decimal::new(-4000, 0));
        let june = sheet.grid("JUNHO 2030");
        assert_eq!(june[row_index(&june, "Natação")][5], json!("mensal"));

        let quiet = sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        assert!(quiet.edits.is_empty(), "{:?}", quiet.edits);
        assert_eq!(quiet.forecasts_written, 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_month_tab_the_person_made_is_left_alone() {
        let (_dir, config, store) = seeded_store().await;
        let sheet = FakeSheet::default();
        sheet
            .tabs
            .borrow_mut()
            .insert("MAIO 2030".to_string(), vec![vec![json!("meu rascunho")]]);
        let report = sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        assert_eq!(sheet.grid("MAIO 2030"), vec![vec![json!("meu rascunho")]]);
        assert_eq!(report.tabs_written, 2);
        assert!(report.warnings.iter().any(|w| w.contains("MAIO 2030")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_phai_tab_missing_from_the_manifest_is_rewritten_without_importing_edits() {
        let (_dir, config, store) = seeded_store().await;
        let sheet = FakeSheet::default();
        sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        sheet.edit(MANIFEST_TAB, |grid| grid.truncate(1));
        sheet.edit("MAIO 2030", |grid| {
            let rent = row_index(grid, "Aluguel");
            grid[rent][1] = json!(1);
        });
        let report = sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        assert!(report.edits.is_empty());
        assert_eq!(report.tabs_written, 3);
        assert!(report.warnings.iter().any(|w| w.contains("registro _phai")));
        let may = sheet.grid("MAIO 2030");
        assert_eq!(may[row_index(&may, "Aluguel")][1], json!(5000));
        let rent = store.get_forecast("aluguel-maio").await.unwrap().unwrap();
        assert_eq!(rent.amount, Decimal::new(-5000, 0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dry_run_reports_edits_without_writing() {
        let (_dir, config, store) = seeded_store().await;
        let sheet = FakeSheet::default();
        sync_planning_sheet(store.as_ref(), &sheet, &config, &options())
            .await
            .unwrap();
        sheet.edit("MAIO 2030", |grid| {
            let rent = row_index(grid, "Aluguel");
            grid[rent][1] = json!(1);
        });
        let before = sheet.grid("MAIO 2030");
        let dry = SyncOptions {
            dry_run: true,
            ..options()
        };
        let report = sync_planning_sheet(store.as_ref(), &sheet, &config, &dry)
            .await
            .unwrap();
        assert_eq!(report.edits.len(), 1);
        assert_eq!(sheet.grid("MAIO 2030"), before);
        let rent = store.get_forecast("aluguel-maio").await.unwrap().unwrap();
        assert_eq!(rent.amount, Decimal::new(-5000, 0));
    }

    #[test]
    fn category_label_reads_family_then_name() {
        let category = |id: &str, name: &str, parent: Option<&str>| CategoryRecord {
            category_id: id.to_string(),
            name: name.to_string(),
            parent_category_id: parent.map(str::to_string),
            metadata_json: json!({}),
            actor_id: "t".to_string(),
            updated_at: Utc::now(),
        };
        assert_eq!(
            category_label(&category(
                "alimentacao:restaurantes",
                "restaurantes",
                Some("alimentacao")
            )),
            "Alimentação › Restaurantes"
        );
        assert_eq!(category_label(&category("saude", "saude", None)), "Saúde");
    }

    #[test]
    fn horizon_runs_from_last_month_through_the_requested_months() {
        let months = horizon(d(2030, 12, 20), 2).unwrap();
        assert_eq!(months, vec![d(2030, 11, 1), d(2030, 12, 1), d(2031, 1, 1)]);
    }
}
