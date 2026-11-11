//! `phai admin relabel`: name past transactions from their rule's label.
//!
//! Sync names a new transaction from the label of the rule it matches
//! (`rotulo`, `set.label`). Transactions that arrived before the label existed
//! keep the bank's text; this fills them. Only transactions without a
//! description are touched, so a name a person typed always stays.

use anyhow::{Context, Result};
use chrono::{NaiveDate, Utc};
use clap::Args;
use phai_core::installments::enrich_description_from_metadata;
use phai_core::migrations::run_migrations;
use phai_core::pluggy::rule_match_text;
use phai_core::rules::{apply_rules_with_facts, compile_rules, labeled_description, CompiledRule};
use phai_core::storage::{open_store, FinanceStore, TransactionAnatomyPatch};
use phai_core::{AuditEvent, TransactionRecord};
use serde_json::json;
use uuid::Uuid;

use crate::load_config;

#[derive(Args)]
pub(crate) struct RelabelArgs {
    /// Show which transactions would be named, without writing.
    #[arg(long)]
    dry_run: bool,
}

/// A transaction and the name its rule gives it.
#[derive(Debug, PartialEq)]
pub(crate) struct Relabel {
    pub transaction_id: String,
    pub transaction_date: NaiveDate,
    pub raw_description: String,
    pub description: String,
}

/// The transactions without a description whose matching rule has a label,
/// with the name the label gives each (the same text and rule the sync uses).
pub(crate) fn relabel_plan(
    transactions: &[TransactionRecord],
    rules: &[CompiledRule],
) -> Vec<Relabel> {
    transactions
        .iter()
        .filter(|tx| {
            tx.description
                .as_deref()
                .is_none_or(|d| d.trim().is_empty())
        })
        .filter_map(|tx| {
            let raw = tx.metadata_json.get("raw");
            let text = rule_match_text(
                &tx.raw_description,
                raw.and_then(|r| r.get("paymentData")),
                tx.tx_type == "credit",
            );
            let label = apply_rules_with_facts(
                &text,
                Some(tx.amount),
                Some(tx.transaction_id.as_str()),
                tx.category_id.clone(),
                tx.category_id.is_some(),
                rules,
            )
            .label?;
            let installment =
                enrich_description_from_metadata(&tx.raw_description, &tx.metadata_json);
            Some(Relabel {
                transaction_id: tx.transaction_id.clone(),
                transaction_date: tx.transaction_date,
                raw_description: tx.raw_description.clone(),
                description: labeled_description(&label, &installment),
            })
        })
        .collect()
}

async fn write(store: &dyn FinanceStore, actor_id: &str, item: &Relabel) -> Result<()> {
    let idempotency_key = format!("relabel:{}:{}", item.transaction_id, Uuid::now_v7());
    store
        .update_transaction_anatomy(
            &item.transaction_id,
            TransactionAnatomyPatch {
                description: Some(&item.description),
                ..TransactionAnatomyPatch::default()
            },
            actor_id,
            &idempotency_key,
        )
        .await
        .with_context(|| format!("update_transaction_anatomy falhou: {}", item.transaction_id))?;
    let audit = AuditEvent::from_entity(
        "transaction",
        &item.transaction_id,
        "transaction.relabel",
        actor_id,
        &idempotency_key,
        json!({ "description": item.description, "source": "rule_label" }),
    );
    store.insert_audit_events(&[audit]).await?;
    Ok(())
}

pub(crate) async fn run(args: RelabelArgs) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;

    let rules = compile_rules(&store.active_rules().await?)?;
    let since = NaiveDate::from_ymd_opt(2020, 1, 1).context("data inicial inválida")?;
    let transactions = store
        .transactions_in_date_range(None, since, Utc::now().date_naive())
        .await?;
    let plan = relabel_plan(&transactions, &rules);
    for item in &plan {
        println!(
            "  {}  {:<48} -> {}",
            item.transaction_date,
            crate::human_format::truncate_with_ellipsis(&item.raw_description, 48),
            item.description
        );
        if !args.dry_run {
            write(store.as_ref(), &config.actor_id, item).await?;
        }
    }
    println!(
        "\n{} lançamento(s) {}pelo rótulo da regra.",
        plan.len(),
        if args.dry_run {
            "seriam nomeados (dry-run, nada gravado) "
        } else {
            "nomeados "
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use phai_core::RuleRecord;
    use rust_decimal::Decimal;
    use serde_json::Value;

    fn tx(id: &str, raw: &str, description: Option<&str>, metadata: Value) -> TransactionRecord {
        let now = Utc::now();
        TransactionRecord {
            transaction_id: id.to_string(),
            account_id: None,
            transaction_date: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
            raw_description: raw.to_string(),
            description: description.map(str::to_string),
            merchant_name: None,
            purpose: None,
            amount: Decimal::new(-35000, 2),
            amount_cents: None,
            tx_type: "debit".to_string(),
            category_id: None,
            category_source: "pluggy".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: "pluggy".to_string(),
            actor_id: "t".to_string(),
            idempotency_key: id.to_string(),
            metadata_json: metadata,
            created_at: now,
            updated_at: now,
            enrichment_attempted_at: None,
        }
    }

    fn rules(body: &str) -> Vec<CompiledRule> {
        let now = Utc::now();
        compile_rules(&[RuleRecord {
            rule_id: "r".to_string(),
            body: body.to_string(),
            status: "active".to_string(),
            actor_id: "t".to_string(),
            idempotency_key: "r".to_string(),
            created_at: now,
            updated_at: now,
        }])
        .unwrap()
    }

    #[test]
    fn names_only_unnamed_transactions_the_rule_labels() {
        let rules = rules(
            r#"{"match_type":"descricao_contains","match_value":"joana souza","categoria":"Moradia","rotulo":"Faxina semanal"}"#,
        );
        let txs = [
            tx("a", "Transferência enviada|JOANA SOUZA", None, json!({})),
            tx(
                "b",
                "Transferência enviada|JOANA SOUZA",
                Some("Faxina extra"),
                json!({}),
            ),
            tx("c", "Transferência enviada|OUTRA PESSOA", None, json!({})),
            // The receiver only shows in paymentData, as the sync sees it.
            tx(
                "d",
                "Transferência enviada",
                Some("  "),
                json!({"raw": {"paymentData": {"receiver": {"name": "Joana Souza"}}}}),
            ),
        ];
        let plan = relabel_plan(&txs, &rules);
        let named: Vec<_> = plan
            .iter()
            .map(|r| (r.transaction_id.as_str(), r.description.as_str()))
            .collect();
        assert_eq!(named, [("a", "Faxina semanal"), ("d", "Faxina semanal")]);
    }

    #[test]
    fn keeps_the_installment_from_card_metadata() {
        let rules = rules(
            r#"{"id":"x","match":{"contains_any":["oficina beta"]},"set":{"category":"Transporte","label":"Revisão do carro"}}"#,
        );
        let txs = [tx(
            "a",
            "Oficina Beta",
            None,
            json!({"raw": {"creditCardMetadata": {"installmentNumber": 2, "totalInstallments": 4}}}),
        )];
        assert_eq!(
            relabel_plan(&txs, &rules)[0].description,
            "Revisão do carro 2/4"
        );
    }
}
