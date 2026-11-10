# Architecture Decision Records

This folder contains Architecture Decision Records (ADRs) for phai — the record of *why* each structural choice was made, so future contributors don't have to re-derive it from `git log`.

## Format

Each ADR is a markdown file with YAML frontmatter. Use the template below (also available at [`0000-template.md`](0000-template.md)).

```markdown
---
type: ADR
id: "NNNN"
title: "Short decision title"
status: proposed        # proposed | active | superseded | retired
date: YYYY-MM-DD
superseded_by: "NNNN"  # only if status: superseded
---

## Context
What situation led to this decision? What forces and constraints are at play?

## Decision
**What was decided.** State it clearly in one or two sentences — bold so it stands out.

## Options considered
- **Option A** (chosen): brief description — pros / cons
- **Option B**: brief description — pros / cons
- **Option C**: brief description — pros / cons

## Consequences
What becomes easier or harder as a result?
What are the positive and negative ramifications?
What would trigger re-evaluation of this decision?
```

### Status lifecycle

```
proposed → active → superseded
                 ↘ retired      (decision no longer relevant, not replaced)
```

## Rules

- One decision per file.
- File name: `NNNN-short-title.md`, monotonic numbering.
- Once `active`, **never edit** — supersede with a new ADR instead.
- When superseded: update `status: superseded`, add `superseded_by: "NNNN"`, and link from the new ADR's Context.
- [ARCHITECTURE.md](../ARCHITECTURE.md) reflects the current state (active decisions only). When an ADR supersedes another, update ARCHITECTURE.md in the same commit.

## Index

| ID | Title | Status |
|----|-------|--------|
| [0001](0001-single-binary-rust-cli.md) | Single-binary Rust CLI as the product surface | active |
| [0002](0002-financestore-trait-dual-backend.md) | `FinanceStore` trait with SQLite + BigQuery backends | active |
| [0003](0003-rust-decimal-end-to-end.md) | `rust_decimal::Decimal` end-to-end for monetary amounts | active |
| [0004](0004-pluggy-as-exclusive-aggregator.md) | Pluggy as the exclusive bank aggregator (Brazil-first) | active |
| [0005](0005-append-only-audit-events.md) | Append-only `AuditEvent` log on every write | active |
| [0006](0006-mirrored-idempotent-migrations.md) | Mirrored idempotent migrations across both backends | active |
| [0007](0007-atomic-self-update.md) | Atomic self-update with SHA-256 verification | superseded by [0017](0017-release-signature-verification.md) |
| [0008](0008-privacy-no-personal-data-in-shared-source.md) | Privacy: no personal data in shared source | active |
| [0009](0009-proactive-pulse-and-closing-plan.md) | Pulse as a proactive closing-plan, not a retrospective transaction list | active |
| [0010](0010-card-billing-cycle.md) | `v_card_summary` groups by billing cycle, not calendar month | active |
| [0011](0011-canonical-payment-status.md) | Canonical `payment_status` vocabulary (`posted`/`pending`/`installment`) | active |
| [0012](0012-amount-cents-exact-aggregation.md) | Exact decimal aggregation via `amount_cents` companion column | active |
| [0013](0013-auto-migrate-on-every-command.md) | Auto-apply pending migrations on every command invocation | active |
| [0014](0014-transaction-anatomy.md) | Transaction anatomy separates raw, human, merchant, purpose, and trace fields | active |
| [0015](0015-anatomy-replication.md) | Anatomy replication propagates human-curated description and purpose from prior same-merchant transactions | superseded by [0033](0033-recurring-human-review-replication.md) |
| [0016](0016-forecast-automation.md) | Forecast automation: templates, detection layers and reconciliation | accepted |
| [0017](0017-release-signature-verification.md) | Release signature verification (minisign) on top of SHA-256 | active |
| [0018](0018-serve-local-dashboard.md) | `fin serve`: local HTTP+WebSocket dashboard for forecast review | superseded by [0019](0019-serve-loopback-only-localhost-alias.md) |
| [0019](0019-serve-loopback-only-localhost-alias.md) | `fin serve` uses loopback-only binding with a localhost alias | active |
| [0020](0020-single-workspace-version.md) | Single workspace version: one number for the whole product | active |
| [0021](0021-ondisk-identity-migration-to-phai.md) | On-disk identity migration from finance-os to phai (legacy fallback) | active |
| [0022](0022-forecast-template-natural-key-idempotency.md) | Forecast template idempotency via natural keys + self-healing dedup | active |
| [0023](0023-web-app-on-livestore-client-only.md) | Web app on LiveStore (client-only) bridged to the store | accepted |
| [0024](0024-cashflow-chart-accrual-source.md) | Cashflow chart sources from `v_cashflow` (accrual) with a net-derived saldo line | superseded by [0025](0025-cashflow-basis-bill-explosion.md) |
| [0025](0025-cashflow-basis-bill-explosion.md) | Cash-flow basis with credit-card bill explosion is the canonical reporting model | accepted |
| [0026](0026-single-view-chain-canonical-source.md) | One deduped view chain is the single source of truth for all reporting | accepted |
| [0027](0027-mcp-server-read-only-self-exec.md) | MCP server: read-only v1, hand-rolled stdio loop, self-exec tools | active |
| [0028](0028-launchd-agent-and-launcher-app.md) | `phai serve install`: launchd agent + launcher app, not a daemon | active |
| [0029](0029-serve-system-daemon-admin-auth.md) | `phai serve install --system`: root LaunchDaemon on port 80 via one admin-auth prompt | active |
| [0030](0030-commitment-tiers.md) | Commitment tiers: a derived controllability axis for planning | proposed |
| [0031](0031-persisted-planning-overlay.md) | Persisted planning overlay: plan as a first-class layer on the chart | proposed |
| [0032](0032-per-transaction-tier-override.md) | Per-transaction commitment-tier override via a sparse table | proposed |
| [0033](0033-recurring-human-review-replication.md) | Recurring human review replication includes category | active |
| [0034](0034-self-contained-encrypted-invite.md) | Self-contained encrypted invite for multi-device activation | active |
| [0035](0035-google-sheets-export.md) | Open in Google Sheets via the Sheets API (OAuth) | proposed |
| [0036](0036-serve-bridge-no-per-request-auth.md) | `phai serve` bridge: loopback + Host/Origin guard as the security boundary, no per-request auth | active |
| [0037](0037-named-planning-scenarios.md) | Named planning scenarios: persisted what-if deltas over the live forecast baseline | accepted |
| [0038](0038-unified-sheet.md) | Unified sheet: forecasts and scenario items as inline rows | accepted |
| [0039](0039-native-desktop-shell.md) | Native macOS desktop shell (Pake/Tauri WKWebView) over the local serve app; COEP require-corp + self-hosted fonts | accepted |
| [0040](0040-single-production-daemon-port-80.md) | Single production daemon on port 80 | active |
| [0041](0041-planning-sheet-month-tabs.md) | Planning sheet: one Google Sheets tab per month, edits imported as forecasts | accepted |

---

*The structure of these docs — the AGENTS.md workflow, the `docs/` layout, and the ADR format — is inspired by [tolaria](https://github.com/refactoringhq/tolaria).*
