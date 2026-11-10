//! `phai category`: the curated category list (names people read).
//!
//! Category ids are slugs (`moradia:agua`); the `name` is what reports, the
//! web app and the planning sheet show ("Água"). Sync derives a slug-shaped
//! name for categories it has never seen, and leaves curated names alone.

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use phai_core::migrations::run_migrations;
use phai_core::storage::{open_store, FinanceStore};
use phai_core::{AuditEvent, CategoryRecord};
use serde_json::json;
use uuid::Uuid;

use crate::load_config;

#[derive(Subcommand)]
pub(crate) enum CategoryCommand {
    /// List categories as a tree: id and name.
    List,
    /// Create or rename categories, one by flags or many from a CSV
    /// (`id,name`); the parent comes from the id (`moradia:agua` → `moradia`).
    Upsert(CategoryUpsertArgs),
}

#[derive(Args)]
pub(crate) struct CategoryUpsertArgs {
    /// Category id: `parent` or `parent:sub`, lowercase slug.
    #[arg(long, requires = "name", conflicts_with = "file")]
    id: Option<String>,
    /// Name to show, e.g. "Água".
    #[arg(long)]
    name: Option<String>,
    /// CSV with `id,name` columns (header first).
    #[arg(long)]
    file: Option<PathBuf>,
}

pub(crate) async fn run(command: CategoryCommand) -> Result<()> {
    let (_, config) = load_config().await?;
    let store = open_store(&config).await?;
    run_migrations(store.as_ref(), &config).await?;
    match command {
        CategoryCommand::List => list(store.as_ref()).await,
        CategoryCommand::Upsert(args) => {
            let rows = match (&args.file, &args.id, &args.name) {
                (Some(path), _, _) => read_csv(path)?,
                (None, Some(id), Some(name)) => vec![(id.clone(), name.clone())],
                _ => bail!("informe --id e --name, ou --file"),
            };
            let count = upsert(store.as_ref(), &config.actor_id, &rows).await?;
            println!("{count} categoria(s) gravada(s).");
            Ok(())
        }
    }
}

async fn list(store: &dyn FinanceStore) -> Result<()> {
    let mut categories = store.get_categories().await?;
    categories.sort_by(|a, b| a.category_id.cmp(&b.category_id));
    for c in &categories {
        let indent = if c.parent_category_id.is_some() {
            "  "
        } else {
            ""
        };
        println!("{indent}{:<44} {}", c.category_id, c.name);
    }
    Ok(())
}

fn read_csv(path: &PathBuf) -> Result<Vec<(String, String)>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("não consegui ler {}", path.display()))?;
    text.lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (id, name) = line
                .split_once(',')
                .with_context(|| format!("linha sem vírgula: {line}"))?;
            Ok((id.trim().to_string(), name.trim().to_string()))
        })
        .collect()
}

/// `true` for `slug` or `slug:slug`, where a slug is lowercase ASCII
/// letters, digits, `-` and `_`.
fn is_category_id(id: &str) -> bool {
    let slug = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    };
    match id.split_once(':') {
        Some((parent, sub)) => slug(parent) && slug(sub),
        None => slug(id),
    }
}

/// Curated category records for `(id, name)` pairs, parent taken from the id.
pub(crate) fn curated_records(
    actor_id: &str,
    rows: &[(String, String)],
) -> Result<Vec<CategoryRecord>> {
    let now = Utc::now();
    rows.iter()
        .map(|(id, name)| {
            if !is_category_id(id) {
                bail!(
                    "\"{id}\" não é um id de categoria: use slug em minúsculas, como moradia:agua"
                );
            }
            if name.is_empty() {
                bail!("\"{id}\" está sem nome");
            }
            Ok(CategoryRecord {
                category_id: id.clone(),
                name: name.clone(),
                parent_category_id: id.split_once(':').map(|(parent, _)| parent.to_string()),
                metadata_json: json!({"source": "curated"}),
                actor_id: actor_id.to_string(),
                updated_at: now,
            })
        })
        .collect()
}

async fn upsert(
    store: &dyn FinanceStore,
    actor_id: &str,
    rows: &[(String, String)],
) -> Result<usize> {
    let records = curated_records(actor_id, rows)?;
    store.upsert_categories(&records).await?;
    let events: Vec<AuditEvent> = records
        .iter()
        .map(|r| {
            AuditEvent::from_entity(
                "category",
                &r.category_id,
                "category.upsert",
                actor_id,
                &format!("category-upsert:{}:{}", r.category_id, Uuid::now_v7()),
                serde_json::to_value(r).unwrap_or_default(),
            )
        })
        .collect();
    store.insert_audit_events(&events).await?;
    Ok(records.len())
}

/// The categories a sync derived that the store does not know yet: names of
/// existing categories may have been curated and must not be overwritten.
pub(crate) fn unseen_categories(
    derived: Vec<CategoryRecord>,
    known: &BTreeSet<String>,
) -> Vec<CategoryRecord> {
    derived
        .into_iter()
        .filter(|c| !known.contains(&c.category_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, name: &str) -> CategoryRecord {
        CategoryRecord {
            category_id: id.to_string(),
            name: name.to_string(),
            parent_category_id: None,
            metadata_json: json!({}),
            actor_id: "t".to_string(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn ids_must_be_lowercase_slugs_with_at_most_one_level() {
        assert!(is_category_id("moradia"));
        assert!(is_category_id("moradia:agua"));
        assert!(is_category_id("_revisar"));
        assert!(!is_category_id("Moradia"));
        assert!(!is_category_id("moradia:água"));
        assert!(!is_category_id("a:b:c"));
        assert!(!is_category_id("moradia:"));
    }

    #[test]
    fn curated_records_take_the_parent_from_the_id() {
        let rows = vec![
            ("moradia".to_string(), "Moradia".to_string()),
            ("moradia:agua".to_string(), "Água".to_string()),
        ];
        let records = curated_records("t", &rows).unwrap();
        assert_eq!(records[0].parent_category_id, None);
        assert_eq!(records[1].parent_category_id.as_deref(), Some("moradia"));
        assert_eq!(records[1].metadata_json["source"], "curated");
        assert!(curated_records("t", &[("x".to_string(), String::new())]).is_err());
    }

    #[test]
    fn sync_only_adds_categories_it_has_not_seen() {
        let known: BTreeSet<String> = ["moradia".to_string()].into();
        let kept = unseen_categories(
            vec![record("moradia", "moradia"), record("moradia:agua", "agua")],
            &known,
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].category_id, "moradia:agua");
    }
}
