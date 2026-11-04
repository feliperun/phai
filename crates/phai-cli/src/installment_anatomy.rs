//! Carry a purchase's identity across its installments.
//!
//! Card processors only send the merchant name, so once a person names one
//! installment ("Bike aro 24 Bruno 4/7"), the following ones arrive bare. This
//! fills them from the named sibling of the same chain, right after each sync
//! and on demand via `phai tx propagate-installments`.

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate, Utc};
use clap::Args;
use phai_core::migrations::run_migrations;
use phai_core::storage::{open_store, FinanceStore, TransactionAnatomyPatch};
use phai_core::{installment_anatomy_fills, AuditEvent, InstallmentAnatomyFill};
use serde_json::json;
use uuid::Uuid;

use crate::{enrich_description_from_metadata, load_config, shift_month};

/// Months of history scanned for sibling installments by default.
pub(crate) const DEFAULT_LOOKBACK_MONTHS: u32 = 24;

#[derive(Args)]
pub(crate) struct PropagateInstallmentsArgs {
    /// Months of history to scan for installment chains.
    #[arg(long, default_value_t = DEFAULT_LOOKBACK_MONTHS)]
    lookback_months: u32,
    /// Show what would be filled without writing any changes.
    #[arg(long)]
    dry_run: bool,
}

/// Compute the fills for the lookback window and, unless `dry_run`, write
/// them with one audit event per transaction.
pub(crate) async fn propagate_installment_anatomy(
    store: &dyn FinanceStore,
    actor_id: &str,
    lookback_months: u32,
    dry_run: bool,
) -> Result<Vec<InstallmentAnatomyFill>> {
    let today = Utc::now().date_naive();
    let month_start =
        NaiveDate::from_ymd_opt(today.year(), today.month(), 1).context("mês atual inválido")?;
    let from = shift_month(month_start, -(lookback_months as i32))?;
    let transactions: Vec<_> = store
        .transactions_in_date_range(None, from, today)
        .await?
        .into_iter()
        .map(|mut tx| {
            tx.raw_description =
                enrich_description_from_metadata(&tx.raw_description, &tx.metadata_json);
            tx
        })
        .collect();
    let fills = installment_anatomy_fills(&transactions);
    if !dry_run {
        for fill in &fills {
            write_fill(store, actor_id, fill).await?;
        }
    }
    Ok(fills)
}

async fn write_fill(
    store: &dyn FinanceStore,
    actor_id: &str,
    fill: &InstallmentAnatomyFill,
) -> Result<()> {
    let idempotency_key = format!("anatomy_chain:{}:{}", fill.transaction_id, Uuid::now_v7());
    if fill.description.is_some() || fill.merchant_name.is_some() || fill.purpose.is_some() {
        store
            .update_transaction_anatomy(
                &fill.transaction_id,
                TransactionAnatomyPatch {
                    description: fill.description.as_deref(),
                    merchant_name: fill.merchant_name.as_deref(),
                    purpose: fill.purpose.as_deref(),
                    ..TransactionAnatomyPatch::default()
                },
                actor_id,
                &idempotency_key,
            )
            .await
            .with_context(|| {
                format!("update_transaction_anatomy falhou: {}", fill.transaction_id)
            })?;
    }
    // `manual` so the next sync's MERGE keeps it: the donor's category was a
    // person's decision about this same purchase.
    if let Some(category_id) = fill.category_id.as_deref() {
        store
            .annotate_transaction(
                &fill.transaction_id,
                Some(category_id),
                Some("manual"),
                None,
                actor_id,
                &idempotency_key,
            )
            .await
            .with_context(|| format!("annotate_transaction falhou: {}", fill.transaction_id))?;
    }
    let audit = AuditEvent::from_entity(
        "transaction",
        &fill.transaction_id,
        "installment_anatomy_propagated",
        actor_id,
        &idempotency_key,
        json!({
            "donor_id": fill.donor_transaction_id,
            "description": fill.description,
            "merchant_name": fill.merchant_name,
            "purpose": fill.purpose,
            "category_id": fill.category_id,
        }),
    );
    store.insert_audit_events(&[audit]).await?;
    Ok(())
}

pub(crate) async fn tx_propagate_installments(args: PropagateInstallmentsArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    let fills = propagate_installment_anatomy(
        store.as_ref(),
        &config.actor_id,
        args.lookback_months,
        args.dry_run,
    )
    .await?;
    for fill in &fills {
        println!(
            "  fill  {}  ←  {}{}{}{}{}",
            fill.transaction_id,
            fill.donor_transaction_id,
            fill.description
                .as_deref()
                .map(|d| format!("  desc={d:?}"))
                .unwrap_or_default(),
            fill.merchant_name
                .as_deref()
                .map(|m| format!("  merchant={m:?}"))
                .unwrap_or_default(),
            fill.purpose
                .as_deref()
                .map(|p| format!("  purpose={p:?}"))
                .unwrap_or_default(),
            fill.category_id
                .as_deref()
                .map(|c| format!("  category={c:?}"))
                .unwrap_or_default(),
        );
    }
    println!(
        "\nParcelas preenchidas a partir da mesma compra: {}{}",
        fills.len(),
        if args.dry_run {
            "  (dry-run — nenhuma alteração gravada)"
        } else {
            ""
        },
    );
    Ok(())
}
