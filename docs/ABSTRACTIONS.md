# Abstractions

The domain models, traits, and conventions that shape phai. Read [ARCHITECTURE.md](ARCHITECTURE.md) for the system map first; this document zooms into the types and the contracts between them.

## Design Philosophy

Two ideas drive every model in this repo:

1. **Decimal precision is non-negotiable.** Money is `rust_decimal::Decimal` from the API boundary to the database row. Floats never touch an amount.
2. **Every mutation is an event.** The dataset is a replayable log: raw events on one side, derived views on the other. You don't fix data by overwriting it — you append a correction and let the views resolve it.

The full set of design principles is in [ARCHITECTURE.md §Design Principles](ARCHITECTURE.md#design-principles).

## The Storage Trait — `FinanceStore`

`FinanceStore` (in `crates/phai-core/src/storage/mod.rs`) is the only seam between domain logic and persistence. It is `async` and `Send + Sync`, with one implementation per backend:

| Backend | Module | Use case |
|---|---|---|
| SQLite | `storage::local` | Local single-user, dev, E2E tests, zero setup |
| BigQuery | `storage::bigquery` | Multi-device, production |

### Surface (grouped)

```
// schema management
applied_migrations()           record_migration(version)         apply_sql(sql)

// accounts
upsert_accounts(rows)          get_accounts()                    insert_account_snapshots(rows)

// transactions
upsert_transactions(rows)      existing_transaction_ids(ids)     transaction_by_id(id)
find_transactions_by_description(q, limit)                       transactions_on_date(date)
similar_transactions(tx)       transactions_in_date_range(from, to)
latest_pluggy_transaction_date()                                 mark_enrichment_attempted(ids)
annotate_transaction(id, ctx, category)                          transactions_with_context(limit)
count_transactions_with_context()

// splits
apply_transaction_split(payload)                                 transaction_split_detail(id)
clear_transaction_split(id)                                      split_candidates(since)

// rules
upsert_rules(rows)             all_rules()                       active_rules()
internal_categories()

// categories & budgets
upsert_categories(rows)        upsert_category_budget(record)    list_category_budgets(month)
budget_status_for_month(month)

// reports (business-logic-encoded queries)
daily_pulse(since)             monthly_spend(month_ref)          cashflow(months)
forecast_vs_actual(month_ref)  card_summary(month_ref)           card_closed_transactions(...)
card_reportable_transactions(...)                                effective_transactions_window(...)
uncategorized(limit)           count_uncategorized()             count_rows(table)
item_prices(query, since)

// audit & forecasting
insert_audit_events(rows)      upsert_forecasts(rows)

// planning scenarios (ADR-0037)
upsert_plan_scenarios(rows)    list_plan_scenarios(status)       get_plan_scenario(id)
set_plan_scenario_status(id, status)                             delete_plan_scenario(id)
upsert_plan_changes(rows)      list_plan_changes(scenario, status)
delete_plan_change(id)

// validation
validate_table_name(table)     // standalone fn — allowlist gate for any dynamic SQL identifier
```

### Invariants

1. **No string-interpolated values in SQL.** Every parameter is bound. The only dynamic identifier permitted is a table name, and it must pass `validate_table_name` (allowlist check).
2. **Reports return decimals.** Rows for `daily_pulse`, `card_summary`, etc. carry `Decimal`, not `f64`, not strings-meant-as-decimals.
3. **Every write op emits an `AuditEvent`.** The store does not magically log writes — callers do. Reviewing a write path means checking it inserts an event.
4. **Idempotent upserts.** `upsert_*` methods are safe to re-run on identical input. The SQL uses `ON CONFLICT` / `MERGE` semantics; never `INSERT` followed by application-level dedupe.
5. **Migrations are append-only.** Numeric prefixes are monotonic. A migration that's been released is never edited — supersede with a new one.

## Core Domain Types

Models live in `crates/phai-core/src/models.rs`. The shapes are intentionally flat: a row in, a row out. Joining and shaping happen in SQL views.

### `TransactionRecord`

The atomic unit. Every Pluggy transaction, manual entry, and split line surfaces as a `TransactionRecord` somewhere in the pipeline.

Key fields (non-exhaustive):

| Field | Type | Meaning |
|---|---|---|
| `id` | `String` | Stable transaction id — Pluggy id, or derived idempotency key for manual entries |
| `account_id` | `String` | FK to `AccountRecord` |
| `posted_at` | `DateTime<Utc>` | Effective transaction date |
| `amount` | `Decimal` | Signed; expenses negative, income positive. Credit-card sign normalization happens in views, not at write time |
| `raw_description` | `String` | Original bank/Pluggy text used for rules, audit, technical search, and debugging |
| `description` | `Option<String>` | Short human description of what was bought; user-facing views fall back to merchant/raw text when empty |
| `merchant_name` | `Option<String>` | Clean establishment name for grouping and enrichment suggestions |
| `purpose` | `Option<String>` | Optional human intent/reason for the purchase |
| `category` | `Option<String>` | Effective category from rules + overrides; views resolve precedence |
| `classifier_trace` | `Option<String>` | Technical rule/enrichment trace; not shown in normal reports |
| `context` | `Option<String>` | Deprecated compatibility column; use the explicit anatomy fields instead |
| `metadata` | `Value` (JSON) | Aggregator-specific payload — Pluggy installment markers, MCC, original currency, etc. |

### `AuditEvent`

Append-only record of every mutation. Lives in the `audit_events` table.

```rust
pub struct AuditEvent {
    pub id: Uuid,             // v7 — chronologically sortable
    pub actor_id: String,     // who triggered the change (CLI actor, agent id, …)
    pub action: String,       // e.g. "tx.categorize", "sync.pluggy", "split.apply"
    pub entity: String,       // e.g. "transaction", "rule", "budget"
    pub entity_id: String,    // the affected row's id
    pub payload: Value,       // JSON snapshot of the change (before/after where relevant)
    pub created_at: DateTime<Utc>,
}
```

**Rule:** every public write method on `FinanceStore` is paired with an `insert_audit_events` call by the caller in the same logical operation. Tests assert on the audit row, not just on the mutated state.

### Idempotency

`idempotency.rs` derives stable keys from input payloads (Pluggy transaction id, content hashes for manual entries). Upserts use these keys, so:

- Re-running `sync pluggy` over the same window does not duplicate.
- A manual entry with identical fields is a no-op on re-run.
- A bug fix that re-syncs historical data is safe by construction.

## Money — `rust_decimal::Decimal`

Hard rules:

- **API boundary in:** parse strings (`decimal_from_str`) — never parse JSON numbers as `f64` first.
- **In Rust:** `Decimal` everywhere. Arithmetic uses operator overloads; no `as f64`.
- **Storage:**
  - SQLite — store as `TEXT` (decimal-as-string). `rusqlite` ↔ `Decimal` goes through `serde` / explicit conversion.
  - BigQuery — store as `NUMERIC` (38-digit precision). Serialize as JSON string in REST payloads.
- **API boundary out:** human format applies locale formatting at the very last step (`human_format.rs`).

Why this matters: floating-point error in finance is observable to the user in seconds — totals don't reconcile, balances drift by cents, "round" amounts come back ugly. `Decimal` makes those bugs structurally impossible.

## Reporting Views (semantic SQL layer)

Reports do not read raw tables. They read **views** that encode the business meaning:

| View | What it bakes in | Migration |
|---|---|---|
| `effective_transactions` | Internal-transfer exclusion, rule-based category, override precedence | `012_effective_transactions_view.sql` |
| `display_labels` | User-facing label: `description ?? merchant_name ?? raw_description`, emoji prefix from display rules | `033_transaction_anatomy.sql` |
| `reportable_transactions` | What appears in user-facing reports (post-exclusion, post-categorization) | `015_reportable_transactions_view.sql` (sqlite) / `014_…` (bigquery) |

Adding a new report:

1. Identify which view(s) it reads from. If none fit, add a new view migration in **both** backends.
2. Add a method to `FinanceStore` returning a `Vec<…Row>` shape.
3. Implement in `local.rs` and `bigquery.rs`.
4. Add a `human_format::…` formatter. The CLI dispatches `--raw` to JSON and `--csv` to CSV from the same serializable report payload.
5. Add an E2E test in `crates/phai-cli/tests/` using SQLite.

## Rules Engine

`rules.rs` defines `RuleRecord` and the matching algorithm: ordered, first-match-wins, with conditions on `raw_description`, account, amount range, and date range. Rules are runtime data, not code — the only path to add personal classification logic.

**Never** hardcode a counterparty pattern in `enrichment/heuristics.rs` or in a migration. If it's specific to one user's bank statement, it goes in the `rules` table on that user's machine.

## Enrichment Pipeline

`enrichment/` is the LLM-assisted classification glue. Stages, all optional:

1. `pluggy_map` — translate Pluggy's structured category hints into the local taxonomy.
2. `cnpj` — look up Brazilian company tax IDs to enrich descriptions.
3. `heuristics` — generic, **non-personal** patterns (currency markers, installment text, common merchant suffixes).
4. `fuzzy` — match unknown descriptions against historical labeled transactions.
5. `llm` (via `rig-core`) — last-resort suggestion with `prompt.rs` controlling the system prompt.
6. `rule_gen` — propose a `RuleRecord` from a confident suggestion; the user accepts/rejects.

Enrichment proposes; the `rules` table persists. The pipeline does not write categories directly to transactions — it produces suggestions that the user (or `rule upsert`) materializes.

## Pluggy Client

`pluggy.rs` is the only network-facing module besides BigQuery REST and the self-updater. Contract:

- HMAC client credentials, automatic token refresh.
- Pagination is handled internally — callers see a single `Vec<Transaction>`.
- Decimal amounts come in as JSON strings and stay strings until parsed into `Decimal`.
- Pluggy installment metadata is preserved in `metadata` and surfaced via the description-enrichment step (see ADR-0006 in tolaria's pattern, and the recent `fix(installments)` commits in `git log`).

## Splits

A single bank transaction can be split into N categorized lines (groceries → food + cleaning + pets). `splits.rs` and `split_payload.rs` define:

- `TransactionSplitPayload` — the user's intent (target tx + list of (amount, category, context)).
- Validation: line amounts sum to the parent transaction's signed amount, exactly, in `Decimal`.
- Receipt-item analytics: optional `receipt_items` rows enable `report item-prices` (historical prices for a specific item across receipts).
- `apply_transaction_split` (on `FinanceStore`) — writes the split rows + emits an audit event.
- Views treat split children as the reportable rows when present; the parent is excluded.

### Backend support (current state)

**Splits are BigQuery-only today.** The schema lives in `schema/bigquery/014_transaction_splits.sql` (4 tables: `transaction_splits`, `transaction_split_lines`, `receipt_items`, `split_review_policies`). The full implementation is in `storage/bigquery.rs`. The SQLite implementation in `storage/local.rs` returns `split_bigquery_only_error()` for every split-related method.

This is the one place where the dual-backend parity promised by [ADR-0002](adr/0002-financestore-trait-dual-backend.md) is currently broken. It's documented in [tx-splits-cli-test-plan.md](tx-splits-cli-test-plan.md) and tracked as known parity debt — porting requires a SQLite split migration, `MERGE`→`UPSERT` translation in 5 trait methods, and (the sharpest edge) reproducing `item_prices` text search without BigQuery's analytics layer (likely via FTS5).

Until ported, the CLI surfaces a clear unsupported-backend error on local for: `tx split preview/apply/show/clear`, `report split-candidates`, `report item-prices`.

## Installments (parcelas)

`installments.rs` detects installment chains in transaction descriptions (`PARC 2/12`, `1 de 6`, Pluggy structured markers). A chain groups related rows so reports can show "X de Y" and project the chain's end. Detection happens at sync time and during description enrichment.

Chains also carry a purchase's identity: once one installment is named, `installment_anatomy_fills` copies its description (with each row's own `k/n`), merchant and purpose to the siblings still missing them, plus the category when the donor's was chosen by hand. The sync runs it right after ingestion, before LLM enrichment, and `phai tx propagate-installments` runs it on demand. Existing values are never overwritten.

## Planning Scenarios (ADR-0037)

A scenario (`plan_scenario`) is a named set of typed deltas (`plan_change`:
`add_one_shot` | `adjust_amount` | `skip_forecast` | `end_template` |
`hypothetical_installment`) layered over the live forecast baseline at read
time by the pure engine `phai_core::scenario::apply_scenario`. Nothing is
copied, so the what-if projection never goes stale; changes whose target was
realized/removed become orphaned no-ops (recomputed on read, pruned only
explicitly). Promotion applies the deltas to the real plan with natural
idempotency keys (`scenario-{scenario_id}-{change_id}`, ADR-0022).

## Planning Sheet (ADR-0041)

`phai_core::planning_sheet` turns a month's forecasts, card cycles and transactions into a `MonthTab` (`build_month_tab`: table rows, card installments, debit spending, category blocks), lays it out as cells plus `Style`/`Range`/`Validation` presentation (`render_tab`, mapped to Sheets requests in `google_sheets::format`), reads edited cells back (`parse_tab`) and compares them with the manifest the previous sync left (`diff_tab` → `SheetEdit::{Create, Update, Remove, CardPlan}`). The manifest marks the tabs phai owns (`TAB_MARKER`), so a month tab a person made is never overwritten. `google_sheets::SheetsApi` is the network seam (`HttpSheets` in production, a fake in tests). In the CLI, `planning_sheet_apply::WorkingSet` applies edits to the window's forecasts in memory: monthly series are independent forecasts sharing `metadata.series_id`, edits flow forward only to months still holding the old value, and rows typed in the sheet get content-derived ids so a retried sync never duplicates them.

## Config & Paths

`config::ConfigPaths` resolves OS-appropriate config/data directories:

- macOS: `~/Library/Application Support/finance-os`
- Linux: `~/.config/finance-os`
- Overridable via `FINANCE_OS_CONFIG_DIR` / `FINANCE_OS_DATA_DIR`.

`AppConfig` knows the backend kind, credentials path (for BigQuery), and the actor id used in audit events.

## Audit Event Conventions

Action names use `entity.verb`:

- `tx.upsert`, `tx.categorize`, `tx.set-context`, `tx.split.apply`, `tx.split.clear`
- `rule.upsert`, `account.upsert`, `forecast.upsert`, `budget.upsert`
- Planning sheet writes use actor `planning-sheet` and actions `sheet_create`, `sheet_update`, `sheet_discard`, `sheet_card_plan`, `sheet_series_extend`, `sheet_series_end`
- `sync.pluggy`, `import.legacy`

When adding a new write op, choose a name that fits this grammar. Replay tools and analytics depend on it being predictable.
