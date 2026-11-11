use crate::models::TransactionRecord;
use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Returns the description enriched with an installment marker sourced from
/// Pluggy metadata when the stored description text lacks one. Used to make
/// `report installments` work with data synced before the Pluggy fix landed,
/// and lets a rule label keep the installment of a card purchase.
pub fn enrich_description_from_metadata(description: &str, metadata: &Value) -> String {
    if parse_installment_description(description).is_some() {
        return description.to_string();
    }
    // Try descriptionRaw first.
    if let Some(raw) = metadata
        .get("raw")
        .and_then(|r| r.get("descriptionRaw"))
        .and_then(|v| v.as_str())
        .filter(|s| parse_installment_description(s).is_some())
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

/// Identifies an installment marker in a transaction description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallmentMarker {
    /// Current installment number (1-based).
    pub current: u32,
    /// Total number of installments.
    pub total: u32,
    /// Description with the installment marker stripped and trimmed.
    pub base_description: String,
}

/// Parses an installment marker from a transaction description.
///
/// Recognized patterns (case-insensitive, checked in order):
/// 1. `"X/Y"` at end: e.g. `"ALGUMA COISA 3/12"`
/// 2. `"Parcela X de Y"` anywhere: e.g. `"Algo - Parcela 3 de 12"`
/// 3. `"X de Y"` at end: e.g. `"ALGUMA COISA 3 de 12"`
///
/// Returns `None` if no pattern matches or if the numbers are invalid
/// (X > Y, Y > 99, X == 0).
pub fn parse_installment_description(desc: &str) -> Option<InstallmentMarker> {
    parse_x_slash_y(desc)
        .or_else(|| parse_parcela_x_de_y(desc))
        .or_else(|| parse_x_de_y_at_end(desc))
}

fn validate_marker(current: u32, total: u32, base: &str) -> Option<InstallmentMarker> {
    if current == 0 || total == 0 || current > total || total > 99 {
        return None;
    }
    Some(InstallmentMarker {
        current,
        total,
        base_description: base.trim().to_string(),
    })
}

/// Parses `"X/Y"` at the end of the description.
fn parse_x_slash_y(desc: &str) -> Option<InstallmentMarker> {
    let trimmed = desc.trim();
    // Find last token that contains '/'
    let last_token = trimmed.split_whitespace().last()?;
    let slash_pos = last_token.find('/')?;
    let left = &last_token[..slash_pos];
    let right = &last_token[slash_pos + 1..];

    if left.is_empty() || right.is_empty() {
        return None;
    }
    if !left.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if !right.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let current: u32 = left.parse().ok()?;
    let total: u32 = right.parse().ok()?;

    // Strip the last token from the description to get the base
    let base = trimmed[..trimmed.len() - last_token.len()].trim_end();
    validate_marker(current, total, base)
}

/// Parses `"Parcela X de Y"` anywhere in the description (case-insensitive).
fn parse_parcela_x_de_y(desc: &str) -> Option<InstallmentMarker> {
    let lower = desc.to_ascii_lowercase();
    // Find "parcela" in the lowercased string
    let parcela_pos = lower.find("parcela")?;
    let after_parcela = lower[parcela_pos + 7..].trim_start();

    // Expect a number
    let num_end = after_parcela
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, _)| i + 1)?;
    let current_str = &after_parcela[..num_end];
    if current_str.is_empty() {
        return None;
    }
    let current: u32 = current_str.parse().ok()?;

    // After number, expect " de " (case-insensitive, already lowercased)
    let after_num = after_parcela[num_end..].trim_start();
    if !after_num.starts_with("de ") {
        return None;
    }
    let after_de = after_num[3..].trim_start();

    // Read total number
    let total_end = after_de
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, _)| i + 1)?;
    let total_str = &after_de[..total_end];
    if total_str.is_empty() {
        return None;
    }
    let total: u32 = total_str.parse().ok()?;

    // Build base description by stripping the matched region from the original desc.
    // The matched region in `desc` starts at `parcela_pos`.
    // We strip from the character before "parcela" up to end of total.
    // Actually we need to find the boundaries in the original string.
    // The region to strip is from `parcela_pos` to the end of the total digits.
    let match_len_in_lower = {
        // parcela_pos + "parcela".len() + spaces + current_digits + " de " + spaces + total_digits
        let total_offset = (lower.len() - after_de.len()) + total_end;
        total_offset - parcela_pos
    };

    // Strip the "Parcela X de Y" segment (and surrounding separators)
    let before = &desc[..parcela_pos];
    let after_full = &desc[parcela_pos + match_len_in_lower..];

    // Clean up separators around the stripped portion
    let before_clean = before.trim_end_matches(|c: char| c == '-' || c == '–' || c.is_whitespace());
    let after_clean =
        after_full.trim_start_matches(|c: char| c == '-' || c == '–' || c.is_whitespace());

    let base = if before_clean.is_empty() {
        after_clean.to_string()
    } else if after_clean.is_empty() {
        before_clean.to_string()
    } else {
        format!("{before_clean} {after_clean}")
    };

    validate_marker(current, total, &base)
}

/// Parses `"X de Y"` at the end of the description (case-insensitive).
fn parse_x_de_y_at_end(desc: &str) -> Option<InstallmentMarker> {
    let trimmed = desc.trim();
    let lower = trimmed.to_ascii_lowercase();

    // Work backwards: find trailing digits (total), then " de ", then more digits (current)
    // We scan tokens from the end.
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let n = tokens.len();
    if n < 3 {
        return None;
    }

    // Last token should be all digits (total)
    let total_token = tokens[n - 1];
    if !total_token.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    // Second-to-last token should be "de" (case-insensitive)
    let de_token = tokens[n - 2].to_ascii_lowercase();
    if de_token != "de" {
        return None;
    }

    // Third-to-last token should be all digits (current)
    let current_token = tokens[n - 3];
    if !current_token.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let total: u32 = total_token.parse().ok()?;
    let current: u32 = current_token.parse().ok()?;

    // Build the base by stripping the last 3 tokens
    let base_tokens = &tokens[..n - 3];
    let base = base_tokens.join(" ");
    let _ = lower; // suppress unused warning

    validate_marker(current, total, &base)
}

/// A group of transaction records that form a single installment chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallmentChain {
    /// Account identifier shared by all installments.
    pub account_id: String,
    /// Normalized base description (with installment marker stripped).
    pub base_description: String,
    /// Total number of installments declared in descriptions.
    pub total: u32,
    /// Maximum `current` seen across all known installments.
    pub current: u32,
    /// All transaction records belonging to this chain (sorted by date).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub installments: Vec<TransactionRecord>,
    /// Date of the earliest known installment.
    pub first_date: NaiveDate,
    /// Projected end date: `first_date` + (`total` - 1) months.
    pub projected_end: NaiveDate,
    /// `total - current` installments still pending.
    pub remaining: u32,
    /// `true` when only one installment remains (next month completes the chain).
    pub released_next_month: bool,
    /// Sum of amounts across all known installments.
    pub total_amount: Decimal,
}

/// Groups a flat list of transactions into installment chains.
///
/// Transactions that do not match any installment pattern are silently skipped.
///
/// Grouping key: `(account_id, base_description, total)`.
pub fn group_into_chains(transactions: &[TransactionRecord]) -> Vec<InstallmentChain> {
    // Key: (account_id, base_description, total, timeline_anchor)
    type Key = (String, String, u32, i32);

    let mut groups: BTreeMap<Key, Vec<TransactionRecord>> = BTreeMap::new();

    for tx in transactions {
        let marker = match parse_installment_description(&tx.raw_description) {
            Some(m) => m,
            None => continue,
        };

        let account_id = tx.account_id.clone().unwrap_or_default();
        let anchor = tx.transaction_date.year() * 12 + tx.transaction_date.month() as i32
            - marker.current as i32;
        let key: Key = (
            account_id,
            marker.base_description.clone(),
            marker.total,
            anchor,
        );
        groups.entry(key).or_default().push(tx.clone());
    }

    let chains: Vec<InstallmentChain> = groups
        .into_iter()
        .filter_map(
            |((account_id, base_description, total, _anchor), mut installments)| {
                installments.sort_by_key(|tx| tx.transaction_date);

                let first_date = installments.first()?.transaction_date;

                // Find the maximum `current` among all records in this group.
                let current = installments
                    .iter()
                    .filter_map(|tx| {
                        parse_installment_description(&tx.raw_description).map(|m| m.current)
                    })
                    .max()
                    .unwrap_or(0);

                if current == 0 {
                    return None;
                }

                let remaining = total.saturating_sub(current);
                let released_next_month = remaining == 1;

                // Projected end: first_date + (total - 1) months
                let projected_end = add_months(first_date, total.saturating_sub(1))?;

                let total_amount = installments
                    .iter()
                    .map(|tx| tx.amount)
                    .fold(Decimal::ZERO, |acc, v| acc + v);

                Some(InstallmentChain {
                    account_id,
                    base_description,
                    total,
                    current,
                    installments,
                    first_date,
                    projected_end,
                    remaining,
                    released_next_month,
                    total_amount,
                })
            },
        )
        .collect();

    let mut chains = merge_renamed_chains(chains);
    sort_chains(&mut chains);
    chains
}

/// Identity to copy onto an installment that arrived without it, taken from a
/// sibling installment of the same purchase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallmentAnatomyFill {
    pub transaction_id: String,
    pub donor_transaction_id: String,
    pub description: Option<String>,
    pub merchant_name: Option<String>,
    pub purpose: Option<String>,
    /// Set only when the donor's category was chosen by hand and the target's
    /// was not, so every installment of a purchase lands in the same category.
    pub category_id: Option<String>,
}

/// For each installment chain with a named installment, fill the siblings
/// that are still missing a description, merchant or purpose. The
/// description keeps the donor's wording with the target's own `k/n`.
/// Existing values are never overwritten.
pub fn installment_anatomy_fills(
    transactions: &[TransactionRecord],
) -> Vec<InstallmentAnatomyFill> {
    let mut fills = Vec::new();
    for chain in group_into_chains(transactions) {
        let Some(donor) = chain
            .installments
            .iter()
            .rev()
            .find(|tx| non_empty(tx.description.as_deref()).is_some())
        else {
            continue;
        };
        let donor_description = non_empty(donor.description.as_deref()).unwrap_or_default();
        let base = installment_base_name(donor_description);
        for target in &chain.installments {
            if target.transaction_id == donor.transaction_id {
                continue;
            }
            let description = match (
                non_empty(target.description.as_deref()),
                parse_installment_description(&target.raw_description),
            ) {
                (None, Some(marker)) => Some(format!("{base} {}/{}", marker.current, marker.total)),
                _ => None,
            };
            let merchant_name = missing_from(
                target.merchant_name.as_deref(),
                donor.merchant_name.as_deref(),
            );
            let purpose = missing_from(target.purpose.as_deref(), donor.purpose.as_deref());
            let category_id = (donor.category_source == "manual"
                && target.category_source != "manual"
                && donor.category_id.is_some()
                && target.category_id != donor.category_id)
                .then(|| donor.category_id.clone())
                .flatten();
            if description.is_none()
                && merchant_name.is_none()
                && purpose.is_none()
                && category_id.is_none()
            {
                continue;
            }
            fills.push(InstallmentAnatomyFill {
                transaction_id: target.transaction_id.clone(),
                donor_transaction_id: donor.transaction_id.clone(),
                description,
                merchant_name,
                purpose,
                category_id,
            });
        }
    }
    fills
}

/// A person's name for an installment without its `k/n`, whether written as
/// a trailing marker ("Bike 2/7") or in parentheses ("Conserto (1/3)").
fn installment_base_name(description: &str) -> String {
    if let Some(marker) = parse_installment_description(description) {
        return marker.base_description;
    }
    let trimmed = description.trim_end();
    if let Some(open) = trimmed.strip_suffix(')').and_then(|rest| rest.rfind('(')) {
        let inner = &trimmed[open + 1..trimmed.len() - 1];
        let is_marker = inner.split_once('/').is_some_and(|(k, n)| {
            [k, n]
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        });
        if is_marker {
            return trimmed[..open].trim_end().to_string();
        }
    }
    trimmed.to_string()
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn missing_from(target: Option<&str>, donor: Option<&str>) -> Option<String> {
    match (non_empty(target), non_empty(donor)) {
        (None, Some(value)) => Some(value.to_string()),
        _ => None,
    }
}

/// Sort: released_next_month descending, then projected_end ascending, then base_description.
fn sort_chains(chains: &mut [InstallmentChain]) {
    chains.sort_by(|a, b| {
        b.released_next_month
            .cmp(&a.released_next_month)
            .then_with(|| a.projected_end.cmp(&b.projected_end))
            .then_with(|| a.base_description.cmp(&b.base_description))
    });
}

/// Canonical plan identity behind a chain's naming. Card processors flip one
/// plan's description between the POS capture (`"Pdv*Beagle"`, `"Pg *Loja"`)
/// and the statement line (`"Beagle - Parcela"`), so the same identity must
/// survive both: lowercase, drop a trailing `"- parcela"` qualifier and drop
/// everything up to the first `*` (POS acquirer prefix). Never empty — when a
/// strip would consume the whole name, the pre-strip form is kept.
fn normalize_plan_name(name: &str) -> String {
    let lower = name.to_lowercase();
    let lower = lower.trim();
    let no_suffix = lower
        .strip_suffix("parcela")
        .map(|rest| rest.trim_end().trim_end_matches('-').trim_end())
        .filter(|rest| !rest.is_empty())
        .unwrap_or(lower);
    no_suffix
        .split_once('*')
        .map(|(_, after)| after.trim())
        .filter(|after| !after.is_empty())
        .unwrap_or(no_suffix)
        .to_string()
}

/// Timeline anchor: the (virtual) month index of "parcela zero". Parcela `k`
/// of a plan lands `k` months after it, so every sighting of one plan yields
/// the same anchor even when each naming saw different parcelas — while a
/// second plan at the same merchant started in another month differs.
fn chain_anchor(chain: &InstallmentChain) -> Option<i32> {
    let last = chain.installments.last()?;
    let current = parse_installment_description(&last.raw_description)
        .map(|m| m.current)
        .unwrap_or(chain.current);
    Some(last.transaction_date.year() * 12 + last.transaction_date.month() as i32 - current as i32)
}

/// Merges chains that are the same real installment plan observed under
/// different namings (see [`normalize_plan_name`]). Without this, each naming
/// forks its own chain and every remaining parcela is projected once per fork.
/// Identity = account + normalized name + declared total + per-parcela amount,
/// with timeline anchors at most one month apart (a statement re-post lands up
/// to one cycle after the POS capture, so the same parcela can cross a month
/// boundary); the freshest sighting donates the canonical naming. Plans at the
/// same merchant started ≥2 months apart keep distinct anchors and never merge.
fn merge_renamed_chains(chains: Vec<InstallmentChain>) -> Vec<InstallmentChain> {
    type MergeKey = (String, String, u32, Decimal);
    let mut merged: Vec<InstallmentChain> = Vec::new();
    let mut groups: BTreeMap<MergeKey, Vec<(i32, InstallmentChain)>> = BTreeMap::new();

    for chain in chains {
        let key = chain.installments.last().zip(chain_anchor(&chain)).map(
            |(last, anchor)| -> (MergeKey, i32) {
                (
                    (
                        chain.account_id.clone(),
                        normalize_plan_name(&chain.base_description),
                        chain.total,
                        last.amount.abs().round_dp(2),
                    ),
                    anchor,
                )
            },
        );
        match key {
            Some((key, anchor)) => groups.entry(key).or_default().push((anchor, chain)),
            // No installment evidence → nothing to merge on.
            None => merged.push(chain),
        }
    }

    for (_, mut keyed) in groups {
        // Cluster by anchor adjacency: consecutive anchors ≤1 month apart are
        // the same plan seen across a posting boundary.
        keyed.sort_by_key(|(anchor, _)| *anchor);
        let mut clusters: Vec<Vec<InstallmentChain>> = Vec::new();
        let mut last_anchor: Option<i32> = None;
        for (anchor, chain) in keyed {
            match (last_anchor, clusters.last_mut()) {
                (Some(prev), Some(cluster)) if anchor - prev <= 1 => cluster.push(chain),
                _ => clusters.push(vec![chain]),
            }
            last_anchor = Some(anchor);
        }
        for group in clusters {
            merged.extend(merge_chain_cluster(group));
        }
    }

    merged
}

/// Folds one cluster of same-plan chains into a single chain (passthrough for
/// singleton clusters).
fn merge_chain_cluster(mut group: Vec<InstallmentChain>) -> Vec<InstallmentChain> {
    if group.len() == 1 {
        return group;
    }
    // Freshest evidence first: its naming matches what the processor
    // emits today, so reconciliation keeps matching new parcelas.
    group.sort_by(|a, b| {
        let last_date = |c: &InstallmentChain| c.installments.last().map(|t| t.transaction_date);
        last_date(b)
            .cmp(&last_date(a))
            .then(b.current.cmp(&a.current))
            .then_with(|| a.base_description.cmp(&b.base_description))
    });

    let total = group[0].total;
    let account_id = group[0].account_id.clone();
    let base_description = group[0].base_description.clone();
    let current = group.iter().map(|c| c.current).max().unwrap_or(0);
    let first_date = group.iter().map(|c| c.first_date).min();
    let mut installments: Vec<TransactionRecord> = group
        .drain(..)
        .flat_map(|c| c.installments.into_iter())
        .collect();
    installments.sort_by_key(|t| t.transaction_date);

    let (Some(first_date), Some(projected_end)) = (
        first_date,
        first_date.and_then(|d| add_months(d, total.saturating_sub(1))),
    ) else {
        return Vec::new(); // unreachable: every grouped chain carries installments
    };
    let remaining = total.saturating_sub(current);
    let total_amount = installments
        .iter()
        .map(|tx| tx.amount)
        .fold(Decimal::ZERO, |acc, v| acc + v);

    vec![InstallmentChain {
        account_id,
        base_description,
        total,
        current,
        installments,
        first_date,
        projected_end,
        remaining,
        released_next_month: remaining == 1,
        total_amount,
    }]
}

fn add_months(date: NaiveDate, months: u32) -> Option<NaiveDate> {
    let mut year = date.year();
    let mut month = date.month() as i32 + months as i32;
    while month > 12 {
        year += 1;
        month -= 12;
    }
    // Clamp the day to the last valid day of the target month
    let max_day = days_in_month(year, month as u32)?;
    let day = date.day().min(max_day);
    NaiveDate::from_ymd_opt(year, month as u32, day)
}

fn days_in_month(year: i32, month: u32) -> Option<u32> {
    // First day of the next month minus one day
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    let last = first_of_next.pred_opt()?;
    Some(last.day())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal::prelude::FromStr;
    use serde_json::json;

    // ── parse_installment_description tests ────────────────────────────────

    #[test]
    fn parse_x_slash_y_basic() {
        let m = parse_installment_description("ALGUMA COISA 3/12").unwrap();
        assert_eq!(m.current, 3);
        assert_eq!(m.total, 12);
        assert_eq!(m.base_description, "ALGUMA COISA");
    }

    #[test]
    fn parse_x_slash_y_at_end_single_word() {
        let m = parse_installment_description("PURCHASE 1/6").unwrap();
        assert_eq!(m.current, 1);
        assert_eq!(m.total, 6);
        assert_eq!(m.base_description, "PURCHASE");
    }

    #[test]
    fn parse_x_slash_y_first_installment() {
        let m = parse_installment_description("NOTEBOOK XYZ 1/12").unwrap();
        assert_eq!(m.current, 1);
        assert_eq!(m.total, 12);
    }

    #[test]
    fn parse_parcela_x_de_y_basic() {
        let m = parse_installment_description("Algo - Parcela 3 de 12").unwrap();
        assert_eq!(m.current, 3);
        assert_eq!(m.total, 12);
        assert_eq!(m.base_description, "Algo");
    }

    #[test]
    fn parse_parcela_x_de_y_case_insensitive() {
        let m = parse_installment_description("Something PARCELA 2 DE 6 extra").unwrap();
        assert_eq!(m.current, 2);
        assert_eq!(m.total, 6);
    }

    #[test]
    fn parse_x_de_y_at_end() {
        let m = parse_installment_description("ALGUMA COISA 3 de 12").unwrap();
        assert_eq!(m.current, 3);
        assert_eq!(m.total, 12);
        assert_eq!(m.base_description, "ALGUMA COISA");
    }

    #[test]
    fn parse_x_de_y_case_insensitive() {
        let m = parse_installment_description("ITEM 1 DE 6").unwrap();
        assert_eq!(m.current, 1);
        assert_eq!(m.total, 6);
        assert_eq!(m.base_description, "ITEM");
    }

    #[test]
    fn parse_no_match_plain_description() {
        assert!(parse_installment_description("Supermercado Angeloni").is_none());
    }

    #[test]
    fn parse_no_match_empty() {
        assert!(parse_installment_description("").is_none());
    }

    #[test]
    fn parse_invalid_x_greater_than_y() {
        assert!(parse_installment_description("THING 7/6").is_none());
    }

    #[test]
    fn parse_invalid_x_equals_zero() {
        assert!(parse_installment_description("THING 0/6").is_none());
    }

    #[test]
    fn parse_invalid_y_greater_than_99() {
        assert!(parse_installment_description("THING 1/100").is_none());
    }

    #[test]
    fn parse_invalid_y_equals_zero() {
        assert!(parse_installment_description("THING 1/0").is_none());
    }

    #[test]
    fn parse_x_slash_y_not_at_end_returns_none() {
        // "3/12" is in the middle — X/Y must be the last token to match pattern 1.
        // "parcela" is absent, "de" not followed by number at end.
        assert!(parse_installment_description("THING 3/12 extra words").is_none());
    }

    #[test]
    fn parse_parcela_in_middle() {
        let m = parse_installment_description("Loja XYZ Parcela 2 de 5 pagamento").unwrap();
        assert_eq!(m.current, 2);
        assert_eq!(m.total, 5);
    }

    // ── group_into_chains tests ────────────────────────────────────────────

    fn make_tx(id: &str, account_id: &str, date: &str, description: &str) -> TransactionRecord {
        TransactionRecord {
            transaction_id: id.to_string(),
            account_id: Some(account_id.to_string()),
            transaction_date: NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            raw_description: description.to_string(),
            description: None,
            merchant_name: None,
            purpose: None,
            amount: Decimal::from_str("-100.00").unwrap(),
            tx_type: "DEBIT".to_string(),
            category_id: None,
            category_source: "manual".to_string(),
            context: None,
            classifier_trace: None,
            payment_status: "posted".to_string(),
            source: "manual".to_string(),
            actor_id: "test".to_string(),
            idempotency_key: id.to_string(),
            metadata_json: json!({}),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            enrichment_attempted_at: None,
            amount_cents: None,
        }
    }

    #[test]
    fn group_empty_input() {
        let chains = group_into_chains(&[]);
        assert!(chains.is_empty());
    }

    #[test]
    fn group_single_chain_multiple_installments() {
        let txs = vec![
            make_tx("t3", "acc1", "2026-03-01", "PURCHASE XYZ 3/6"),
            make_tx("t1", "acc1", "2026-01-01", "PURCHASE XYZ 1/6"),
            make_tx("t2", "acc1", "2026-02-01", "PURCHASE XYZ 2/6"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1);
        let chain = &chains[0];
        assert_eq!(chain.base_description, "PURCHASE XYZ");
        assert_eq!(chain.total, 6);
        assert_eq!(chain.current, 3);
        assert_eq!(chain.remaining, 3);
        assert!(!chain.released_next_month);
        // installments sorted by date
        assert_eq!(chain.installments[0].transaction_id, "t1");
        assert_eq!(chain.installments[1].transaction_id, "t2");
        assert_eq!(chain.installments[2].transaction_id, "t3");
        // projected_end: first_date(2026-01-01) + 5 months = 2026-06-01
        assert_eq!(
            chain.projected_end,
            NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()
        );
    }

    #[test]
    fn group_multiple_chains() {
        let txs = vec![
            make_tx("a1", "acc1", "2026-01-01", "ITEM A 1/3"),
            make_tx("b1", "acc1", "2026-01-01", "ITEM B 1/6"),
            make_tx("a2", "acc1", "2026-02-01", "ITEM A 2/3"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2);
        let descriptions: Vec<&str> = chains.iter().map(|c| c.base_description.as_str()).collect();
        assert!(descriptions.contains(&"ITEM A"));
        assert!(descriptions.contains(&"ITEM B"));
    }

    #[test]
    fn group_keeps_same_merchant_plans_with_different_start_months_apart() {
        let txs = vec![
            make_tx("a1", "card", "2026-01-05", "APPLE 1/12"),
            make_tx("a2", "card", "2026-02-05", "APPLE 2/12"),
            make_tx("b1", "card", "2026-03-05", "APPLE 1/12"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2, "{chains:#?}");
    }

    #[test]
    fn group_chains_across_different_accounts() {
        let txs = vec![
            make_tx("x1", "acc1", "2026-01-01", "ITEM 1/3"),
            make_tx("x2", "acc2", "2026-01-01", "ITEM 1/3"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2);
        let accounts: Vec<&str> = chains.iter().map(|c| c.account_id.as_str()).collect();
        assert!(accounts.contains(&"acc1"));
        assert!(accounts.contains(&"acc2"));
    }

    #[test]
    fn group_partial_chain_missing_some_installments() {
        // Only installment 5/6 present — still creates a chain
        let txs = vec![make_tx("p5", "acc1", "2026-05-01", "PHONE 5/6")];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1);
        let chain = &chains[0];
        assert_eq!(chain.current, 5);
        assert_eq!(chain.remaining, 1);
        assert!(chain.released_next_month);
    }

    #[test]
    fn group_no_installment_transactions_skipped() {
        let txs = vec![
            make_tx("plain", "acc1", "2026-01-01", "Supermercado"),
            make_tx("t1", "acc1", "2026-01-01", "ITEM 1/3"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].installments.len(), 1);
    }

    #[test]
    fn group_complete_chain_has_remaining_zero() {
        let txs = vec![
            make_tx("q1", "acc1", "2026-01-01", "GADGET 1/3"),
            make_tx("q2", "acc1", "2026-02-01", "GADGET 2/3"),
            make_tx("q3", "acc1", "2026-03-01", "GADGET 3/3"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1);
        let chain = &chains[0];
        assert_eq!(chain.remaining, 0);
        assert!(!chain.released_next_month);
    }

    // ── renamed-chain merging tests ────────────────────────────────────────

    fn make_tx_amount(
        id: &str,
        account_id: &str,
        date: &str,
        description: &str,
        amount: &str,
    ) -> TransactionRecord {
        let mut tx = make_tx(id, account_id, date, description);
        tx.amount = Decimal::from_str(amount).unwrap();
        tx
    }

    #[test]
    fn group_merges_same_plan_seen_under_pos_and_statement_naming() {
        // Same real plan: the POS capture names it "Pdv*Beagle", the statement
        // line "Beagle - Parcela". Each naming saw different parcelas.
        let txs = vec![
            make_tx_amount(
                "s3",
                "card",
                "2026-04-09",
                "Beagle - Parcela 3/5",
                "-102.86",
            ),
            make_tx_amount("p4", "card", "2026-05-03", "Pdv*Beagle 4/5", "-102.86"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1, "renamed sightings must merge: {chains:#?}");
        let chain = &chains[0];
        // Canonical naming follows the freshest evidence.
        assert_eq!(chain.base_description, "Pdv*Beagle");
        assert_eq!(chain.current, 4);
        assert_eq!(chain.remaining, 1);
        assert_eq!(chain.installments.len(), 2);
        // first_date = earliest evidence across both namings.
        assert_eq!(
            chain.first_date,
            NaiveDate::from_ymd_opt(2026, 4, 9).unwrap()
        );
    }

    #[test]
    fn group_merge_unions_duplicate_parcela_sightings() {
        // The statement re-posts an already-seen parcela under the other
        // naming (real Pluggy behavior); the union must not double-count
        // progress nor fork the chain.
        let txs = vec![
            make_tx_amount("a4", "card", "2026-04-10", "Globo Globoplay 4/12", "-44.90"),
            make_tx_amount(
                "b4",
                "card",
                "2026-04-28",
                "Globo Globoplay - Parcela 4/12",
                "-44.90",
            ),
            make_tx_amount(
                "b5",
                "card",
                "2026-05-10",
                "Globo Globoplay - Parcela 5/12",
                "-44.90",
            ),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1, "{chains:#?}");
        assert_eq!(chains[0].current, 5);
        assert_eq!(chains[0].base_description, "Globo Globoplay - Parcela");
    }

    #[test]
    fn group_merges_statement_repost_that_crossed_a_month_boundary() {
        // The statement line re-posts up to one cycle after the POS capture,
        // so the same plan's anchors can differ by one month — still one plan.
        let txs = vec![
            make_tx_amount(
                "h2",
                "card",
                "2026-05-01",
                "Htm*Psicoeducacao 2/12",
                "-59.88",
            ),
            make_tx_amount(
                "h3",
                "card",
                "2026-05-10",
                "Htm*Psicoeducacao - Parcela 3/12",
                "-59.88",
            ),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 1, "{chains:#?}");
        assert_eq!(chains[0].current, 3);
    }

    #[test]
    fn group_keeps_distinct_plans_with_different_anchors() {
        // Same store, same price, same total — but the second plan started two
        // months later: different timeline anchor, so they are different plans.
        let txs = vec![
            make_tx_amount("m2", "card", "2026-03-10", "Pdv*Beagle 2/5", "-102.86"),
            make_tx_amount(
                "n0",
                "card",
                "2026-05-12",
                "Beagle - Parcela 2/5",
                "-102.86",
            ),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2, "{chains:#?}");
    }

    #[test]
    fn group_keeps_unrelated_merchants_apart_despite_equal_price_and_timing() {
        // Coincidental same amount/total/anchor at two different stores must
        // never merge — names share no normalized identity.
        let txs = vec![
            make_tx_amount("u1", "card", "2026-05-03", "Loja Azul 2/4", "-150.00"),
            make_tx_amount(
                "u2",
                "card",
                "2026-05-07",
                "Bazar Verde - Parcela 2/4",
                "-150.00",
            ),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2, "{chains:#?}");
    }

    #[test]
    fn group_keeps_same_plan_shape_with_different_amounts_apart() {
        let txs = vec![
            make_tx_amount("v1", "card", "2026-05-03", "Pdv*Beagle 2/4", "-150.00"),
            make_tx_amount("v2", "card", "2026-05-07", "Beagle - Parcela 2/4", "-90.00"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2, "{chains:#?}");
    }

    #[test]
    fn normalize_plan_name_strips_pos_prefix_and_parcela_suffix() {
        assert_eq!(normalize_plan_name("Pdv*Beagle"), "beagle");
        assert_eq!(normalize_plan_name("Beagle - Parcela"), "beagle");
        assert_eq!(
            normalize_plan_name("Globo Globoplay - Parcela"),
            "globo globoplay"
        );
        assert_eq!(normalize_plan_name("Globo Globoplay"), "globo globoplay");
        assert_eq!(normalize_plan_name("Mercadolivre*Mercadol"), "mercadol");
        // Strip never leaves an empty identity.
        assert_eq!(normalize_plan_name("Parcela"), "parcela");
        assert_eq!(normalize_plan_name("Pdv*"), "pdv*");
    }

    #[test]
    fn group_sort_released_next_month_first() {
        let txs = vec![
            make_tx("z1", "acc1", "2026-01-01", "LONG ITEM 1/12"),
            make_tx("z2", "acc1", "2026-01-01", "SHORT ITEM 5/6"),
        ];
        let chains = group_into_chains(&txs);
        assert_eq!(chains.len(), 2);
        // SHORT ITEM 5/6 has remaining=1 => released_next_month=true, should be first
        assert_eq!(chains[0].base_description, "SHORT ITEM");
        assert!(chains[0].released_next_month);
    }

    // ── installment_anatomy_fills tests ────────────────────────────────────

    fn named(
        mut tx: TransactionRecord,
        description: &str,
        merchant: &str,
        purpose: &str,
    ) -> TransactionRecord {
        tx.description = Some(description.to_string());
        tx.merchant_name = Some(merchant.to_string());
        tx.purpose = Some(purpose.to_string());
        tx
    }

    fn categorized(mut tx: TransactionRecord, category: &str, source: &str) -> TransactionRecord {
        tx.category_id = Some(category.to_string());
        tx.category_source = source.to_string();
        tx
    }

    #[test]
    fn anatomy_fills_copy_sibling_identity_with_own_installment_number() {
        let donor = named(
            make_tx("t2", "card", "2026-05-06", "LOJA EXEMPLO 2/4"),
            "Bicicleta infantil 2/4",
            "Loja Exemplo",
            "Presente de aniversário",
        );
        let txs = vec![
            make_tx("t1", "card", "2026-04-06", "LOJA EXEMPLO 1/4"),
            donor,
            make_tx("t3", "card", "2026-06-06", "LOJA EXEMPLO 3/4"),
        ];

        let fills = installment_anatomy_fills(&txs);

        assert_eq!(fills.len(), 2);
        let third = fills.iter().find(|f| f.transaction_id == "t3").unwrap();
        assert_eq!(third.description.as_deref(), Some("Bicicleta infantil 3/4"));
        assert_eq!(third.merchant_name.as_deref(), Some("Loja Exemplo"));
        assert_eq!(third.purpose.as_deref(), Some("Presente de aniversário"));
        assert_eq!(third.donor_transaction_id, "t2");
        let first = fills.iter().find(|f| f.transaction_id == "t1").unwrap();
        assert_eq!(first.description.as_deref(), Some("Bicicleta infantil 1/4"));
    }

    #[test]
    fn anatomy_fills_never_overwrite_and_skip_unnamed_chains() {
        let mut partial = make_tx("t3", "card", "2026-06-06", "LOJA EXEMPLO 3/4");
        partial.purpose = Some("Já explicado".to_string());
        let txs = vec![
            named(
                make_tx("t2", "card", "2026-05-06", "LOJA EXEMPLO 2/4"),
                "Bicicleta infantil 2/4",
                "Loja Exemplo",
                "Presente de aniversário",
            ),
            partial,
            make_tx("u1", "card", "2026-04-10", "OUTRA LOJA 1/3"),
            make_tx("u2", "card", "2026-05-10", "OUTRA LOJA 2/3"),
        ];

        let fills = installment_anatomy_fills(&txs);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].transaction_id, "t3");
        assert_eq!(fills[0].purpose, None);
        assert_eq!(
            fills[0].description.as_deref(),
            Some("Bicicleta infantil 3/4")
        );
    }

    #[test]
    fn anatomy_fills_carry_only_manual_categories() {
        let donor = categorized(
            named(
                make_tx("t1", "card", "2026-04-06", "LOJA EXEMPLO 1/3"),
                "Presente 1/3",
                "Loja Exemplo",
                "Presente",
            ),
            "compras:presentes",
            "manual",
        );
        let ruled = categorized(
            make_tx("t2", "card", "2026-05-06", "LOJA EXEMPLO 2/3"),
            "compras:e-commerce",
            "rule",
        );
        let kept_manual = categorized(
            make_tx("t3", "card", "2026-06-06", "LOJA EXEMPLO 3/3"),
            "outros:geral",
            "manual",
        );
        let rule_donor = categorized(
            named(
                make_tx("r1", "card", "2026-04-15", "MERCADO X 1/2"),
                "Feira 1/2",
                "Mercado X",
                "Feira",
            ),
            "alimentacao:mercado",
            "rule",
        );
        let rule_target = categorized(
            make_tx("r2", "card", "2026-05-15", "MERCADO X 2/2"),
            "compras",
            "pluggy",
        );

        let fills =
            installment_anatomy_fills(&[donor, ruled, kept_manual, rule_donor, rule_target]);

        let by_id = |id: &str| fills.iter().find(|f| f.transaction_id == id).unwrap();
        assert_eq!(
            by_id("t2").category_id.as_deref(),
            Some("compras:presentes")
        );
        assert_eq!(by_id("t3").category_id, None);
        assert_eq!(by_id("r2").category_id, None);
    }

    #[test]
    fn anatomy_fills_drop_parenthesized_marker_from_donor_name() {
        let txs = vec![
            named(
                make_tx("p1", "card", "2026-04-06", "ASSISTENCIA X 1/3"),
                "Conserto relógio - OS 123 (1/3)",
                "Assistência X",
                "Conserto",
            ),
            make_tx("p2", "card", "2026-05-06", "ASSISTENCIA X 2/3"),
        ];

        let fills = installment_anatomy_fills(&txs);

        assert_eq!(
            fills[0].description.as_deref(),
            Some("Conserto relógio - OS 123 2/3")
        );
    }
}
