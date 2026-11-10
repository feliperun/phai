---
type: ADR
id: "0041"
title: "Planning sheet: one Google Sheets tab per month, edits imported as forecasts"
status: accepted
date: 2026-11-10
---

## Context

The household plans its month on a spreadsheet: one tab per month with
`Previsto` (planned) next to `Realizado` (actual), a line per fixed bill, one
line per credit card, income at the bottom and the month balance. phai has the
actual side (transactions, card cycles, reconciled forecasts) and a forecast
table, but projecting future months in the web sheet is less flexible than a
spreadsheet the person can bend with their own formulas.

A spreadsheet already fed phai once: BigQuery external tables read override
tabs live, until that path was removed in May 2026 (`feat!: remove Google Sheets
category-override sync`). It broke whenever a tab or a column moved, needed a
Drive scope on every BigQuery query, and changed reports the moment someone
typed, with no audit trail.

ADR-0035 covers a different shape (create a throwaway spreadsheet from the
current view through per-user OAuth) and stays as proposed.

## Decision

**phai keeps month tabs in a spreadsheet the person owns, and imports what they
type there as forecast changes, in an explicit sync.**

- **Ownership.** The spreadsheet belongs to the person and is shared with the
  BigQuery service account as an editor; `planning_sheet_id` in `config.toml`
  names it. The client asks for the `spreadsheets` scope only.
- **Tabs.** `phai sheets sync` writes the current month and the next
  `planning_sheet_months - 1` (default 12; a household that plans one month
  at a time sets 1), titled `NOVEMBRO 2026`. A tab with a month title that
  phai did not create is never overwritten, and tabs that leave the horizon
  stay as history. Two hidden tabs hold bookkeeping: `_phai` (the manifest)
  and `_phai_listas` (dropdown sources).
- **Layout.** It follows the household's own month tab, cell for cell in
  look: `A:E` is `Item | Previsto | Realizado | Status | Ve.` with fixed bills
  and `Subtotal fixas`, one row per card and `Total Cartão`, `Total Despesas`,
  `RECEITAS` and `Total Receitas`, `SALDO MES`, and `Fluxo Caixa` (income by
  the 15th against what is due before it). `G:L` holds `Parcelamentos Cartão`
  (`Conta | C | P | T | R | Valor`) and `Débito / Dinheiro`; `N:S` holds one
  block per category budget, three per band, with `orçado`, `realizado` and
  what is left. Totals are formulas. `V:Z` carry the fields phai needs to
  round-trip a row (`Repete`, `Categoria`, `Conta`, `Obs.`, hidden `id`); phai's
  note in `T1` marks the tab as phai's.
- **Realizado.** A reconciled forecast shows its transaction; a card row shows
  the total of the billing cycle that closes in the month; a category block
  sums the month's purchases in the category by purchase date and lists the
  largest; `Débito / Dinheiro` lists non-card spending no paid row covers.
  `Status` uses the household's words (`Pago`, `Agendado`, `Previsto`,
  `Atrasado`, `Cancelado`).
- **Edits.** Each sync first reads the tabs, then rewrites them. The manifest
  records what the last sync rendered, so a row that still matches its snapshot
  was not touched (phai's current value wins) and a row that differs was edited
  (the edit wins). Typed rows become forecasts, deleted rows are discarded,
  edited rows are patched, and a card row's `Previsto` becomes a planned bill
  forecast (`metadata.kind = card_budget`). `Status = Cancelado` removes a
  row too. Paid rows are read-only. Category blocks are read-only for now.
- **Monthly series.** `Repete = mensal` creates one independent forecast per
  month, linked by `metadata.series_id`, and the series grows as the horizon
  moves. A change in one month flows to later months that still had the old
  value; months set differently keep theirs. Clearing `Repete` ends the series
  after that month. Template-backed forecasts keep their recurrence in the app.
- **Safety.** Every write goes through `upsert_forecasts` with one audit event
  per forecast (actor `planning-sheet`). Rows typed into the sheet get ids from
  their content, so a sync that failed between writing phai and rewriting the
  sheet does not duplicate them. A tab whose blocks were renamed or removed is
  reported and its edits ignored, never read as mass deletion.
- **When.** On demand (`phai sheets sync`, with `--dry-run`) and after every
  Pluggy sync when `planning_sheet_id` is set. Failures never fail the sync.

## Options considered

- **Explicit sync through the Sheets API (chosen).** Edits arrive as audited
  forecast writes at a known moment; layout drift is detected instead of
  breaking queries. Costs: a sync loop and a manifest tab.
- **BigQuery external tables over the sheet (the May 2026 path).** No sync
  code, but every query depends on the sheet's exact shape, needs Drive access,
  and has no audit trail. Rejected again.
- **BigQuery Connected Sheets, read-only.** Zero code, but one-way: the person
  asked for projections typed in the sheet to become phai forecasts.
- **Templates (ADR-0016) for `Repete`.** Re-materialization rewrites every
  future instance, so a single month's edit would not stick. Independent
  members plus a series id keep per-month edits.

## Consequences

- New module pair in `phai-core` (`planning_sheet`, pure; `google_sheets`,
  HTTP) and in `phai-cli` (`planning_sheet_cmd`, `planning_sheet_apply`).
- The person can change `Item`, `Previsto`, `Ve.`, `Status` (to cancel),
  `Repete`, `Categoria`, `Conta` and `Obs.`; any other cell on a month tab is
  rewritten on the next sync, so free-form work belongs on other tabs. Edits typed while a sync is
  running can be lost.
- A planned card bill coexists with the open-bill and installment forecasts of
  the same card. The sheet shows one of them per card; other planning views
  that sum every active forecast count both until they learn the
  `card_budget` kind.
- The legacy Sheets-backed views a private runtime may still carry
  (`sheet_*_raw`, a rewired `v_forecast_effective`) are not used by this path
  and can be dropped there.
