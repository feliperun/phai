use crate::idempotency::category_id;
use crate::installments::parse_installment_description;
use crate::models::decimal_from_str;
use crate::models::RuleRecord;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use deunicode::deunicode;
use rust_decimal::Decimal;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledRule {
    pub rule_id: String,
    pub category_id: String,
    pub context: Option<String>,
    pub amount_sign: Option<AmountSign>,
    /// Name the rule gives what it matches (`rotulo` / `set.label`).
    pub label: Option<String>,
    matcher: RuleMatcher,
    amount_match: Option<Decimal>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmountSign {
    Positive,
    Negative,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleMatcher {
    ContainsAny(Vec<String>),
    Exact(String),
    TransactionIdExact(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleApplication {
    pub category_id: Option<String>,
    pub category_source: String,
    pub context: Option<String>,
    pub amount_sign: Option<AmountSign>,
    /// Name for the transaction when the matching rule has one.
    pub label: Option<String>,
}

pub fn compile_rules(rows: &[RuleRecord]) -> Result<Vec<CompiledRule>> {
    let mut compiled = Vec::new();
    for row in rows
        .iter()
        .filter(|row| row.status.eq_ignore_ascii_case("active"))
    {
        if let Some(rule) = compile_rule(row)? {
            compiled.push(rule);
        }
    }
    compiled.sort_by(|left, right| {
        right
            .has_transaction_id_match()
            .cmp(&left.has_transaction_id_match())
            .then_with(|| right.has_amount_match().cmp(&left.has_amount_match()))
            .then_with(|| right.matcher_rank().cmp(&left.matcher_rank()))
            .then_with(|| right.matcher_len().cmp(&left.matcher_len()))
            .then_with(|| right.updated_at.cmp(&left.updated_at))
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.rule_id.cmp(&right.rule_id))
    });
    Ok(compiled)
}

pub fn apply_rules(
    description: &str,
    base_category_id: Option<String>,
    had_base_category: bool,
    rules: &[CompiledRule],
) -> (Option<String>, String) {
    let result = apply_rules_with_amount(
        description,
        None,
        base_category_id,
        had_base_category,
        rules,
    );
    (result.category_id, result.category_source)
}

pub fn apply_rules_with_amount(
    description: &str,
    amount: Option<Decimal>,
    base_category_id: Option<String>,
    had_base_category: bool,
    rules: &[CompiledRule],
) -> RuleApplication {
    apply_rules_with_facts(
        description,
        amount,
        None,
        base_category_id,
        had_base_category,
        rules,
    )
}

pub fn apply_rules_with_facts(
    description: &str,
    amount: Option<Decimal>,
    transaction_id: Option<&str>,
    base_category_id: Option<String>,
    had_base_category: bool,
    rules: &[CompiledRule],
) -> RuleApplication {
    let normalized_description = normalize_text(description);
    let normalized_transaction_id = transaction_id.map(normalize_identifier);
    if let Some(rule) = rules.iter().find(|rule| {
        rule.matches(
            &normalized_description,
            normalized_transaction_id.as_deref(),
            amount,
        )
    }) {
        return RuleApplication {
            category_id: Some(rule.category_id.clone()),
            category_source: "rule".to_string(),
            context: rule.context.clone(),
            amount_sign: rule.amount_sign,
            label: rule.label.clone(),
        };
    }

    let source = if had_base_category {
        "pluggy"
    } else {
        "unclassified"
    };
    RuleApplication {
        category_id: base_category_id,
        category_source: source.to_string(),
        context: None,
        amount_sign: None,
        label: None,
    }
}

impl CompiledRule {
    fn matches(
        &self,
        normalized_description: &str,
        normalized_transaction_id: Option<&str>,
        amount: Option<Decimal>,
    ) -> bool {
        if let Some(expected) = self.amount_match {
            let Some(actual) = amount else {
                return false;
            };
            if !amounts_match(actual, expected) {
                return false;
            }
        }
        match &self.matcher {
            RuleMatcher::ContainsAny(needles) => needles
                .iter()
                .any(|needle| contains_bounded(normalized_description, needle)),
            RuleMatcher::Exact(needle) => normalized_description == needle,
            RuleMatcher::TransactionIdExact(needle) => {
                normalized_transaction_id.is_some_and(|transaction_id| transaction_id == needle)
            }
        }
    }

    fn has_transaction_id_match(&self) -> bool {
        matches!(self.matcher, RuleMatcher::TransactionIdExact(_))
    }

    fn has_amount_match(&self) -> bool {
        self.amount_match.is_some()
    }

    fn matcher_rank(&self) -> usize {
        match self.matcher {
            RuleMatcher::TransactionIdExact(_) => 2,
            RuleMatcher::Exact(_) => 1,
            RuleMatcher::ContainsAny(_) => 0,
        }
    }

    fn matcher_len(&self) -> usize {
        match &self.matcher {
            RuleMatcher::TransactionIdExact(needle) => needle.len(),
            RuleMatcher::Exact(needle) => needle.len(),
            RuleMatcher::ContainsAny(needles) => {
                needles.iter().map(|needle| needle.len()).max().unwrap_or(0)
            }
        }
    }
}

fn compile_rule(row: &RuleRecord) -> Result<Option<CompiledRule>> {
    if let Some(rule) = parse_dsl_rule(row)? {
        return Ok(Some(rule));
    }
    parse_json_rule(row)
}

fn parse_dsl_rule(row: &RuleRecord) -> Result<Option<CompiledRule>> {
    let body = row.body.trim();
    if body.starts_with('{') || body.starts_with('[') {
        return Ok(None);
    }

    let normalized = normalize_text(body);
    let prefix = "if description contains ";
    let separator = " then category ";
    if !normalized.starts_with(prefix) {
        bail!(
            "Regra {} usa formato não suportado. Use `if description contains ... then category ...`",
            row.rule_id
        );
    }
    let tail = normalized
        .strip_prefix(prefix)
        .context("Prefixo inválido na regra DSL")?;
    let (needle, target) = tail.split_once(separator).with_context(|| {
        format!(
            "Regra {} inválida. Esperado `if description contains ... then category ...`",
            row.rule_id
        )
    })?;
    build_contains_rule(
        row,
        &row.rule_id,
        vec![strip_wrapping_quotes(needle).to_string()],
        normalize_category_target(target)?,
        None,
        None,
        None,
    )
    .map(Some)
}

fn parse_json_rule(row: &RuleRecord) -> Result<Option<CompiledRule>> {
    let value: Value = serde_json::from_str(&row.body)
        .with_context(|| format!("Rule {} contém JSON inválido", row.rule_id))?;

    if let Some(rule) = parse_legacy_context_rule(row, &value)? {
        return Ok(Some(rule));
    }
    if let Some(rule) = parse_legacy_yaml_rule(row, &value)? {
        return Ok(Some(rule));
    }

    Ok(None)
}

fn parse_legacy_context_rule(row: &RuleRecord, value: &Value) -> Result<Option<CompiledRule>> {
    let rule_id = row.rule_id.as_str();
    let match_type = value
        .get("match_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if !matches!(
        match_type,
        "descricao_contains"
            | "description_contains"
            | "descricao_exata"
            | "description_exact"
            | "pluggy_id"
    ) {
        return Ok(None);
    }

    let needle = value
        .get("match_value")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(needle) = needle else {
        return Ok(None);
    };
    let category = value
        .get("categoria")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(category) = category else {
        return Ok(None);
    };
    let subcategory = value
        .get("subcategoria")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let amount_match = value
        .get("valor_match")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(decimal_from_str)
        .transpose()
        .with_context(|| format!("Rule {rule_id} contém valor_match inválido"))?;
    let context = value
        .get("finalidade")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let amount_sign = parse_amount_sign(value.get("amount_sign"))?;
    let label = optional_text(value.get("rotulo"));

    let category_id = category_id(category, subcategory);
    let rule = match match_type {
        "descricao_exata" | "description_exact" => build_exact_rule(
            row,
            rule_id,
            normalize_text(needle),
            category_id,
            context,
            amount_match,
            amount_sign,
        )?,
        "pluggy_id" => build_transaction_id_rule(
            row,
            rule_id,
            normalize_identifier(needle),
            category_id,
            context,
            amount_sign,
        )?,
        _ => build_contains_rule(
            row,
            rule_id,
            vec![normalize_text(needle)],
            category_id,
            context,
            amount_match,
            amount_sign,
        )?,
    };

    Ok(Some(CompiledRule { label, ..rule }))
}

fn parse_legacy_yaml_rule(row: &RuleRecord, value: &Value) -> Result<Option<CompiledRule>> {
    let rule_id = row.rule_id.as_str();
    let contains_any = value
        .get("match")
        .and_then(|matcher| matcher.get("contains_any"))
        .and_then(Value::as_array);
    let Some(contains_any) = contains_any else {
        return Ok(None);
    };

    let needles = contains_any
        .iter()
        .filter_map(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(normalize_text)
        .collect::<Vec<_>>();
    if needles.is_empty() {
        bail!("Rule {rule_id} sem `match.contains_any` válido");
    }

    let category = value
        .get("set")
        .and_then(|setter| setter.get("category"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("Rule {rule_id} sem `set.category`"))?;
    let subcategory = value
        .get("set")
        .and_then(|setter| setter.get("subcategory"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let amount_sign = parse_amount_sign(
        value
            .get("set")
            .and_then(|setter| setter.get("amount_sign")),
    )?;

    let label = optional_text(value.get("set").and_then(|setter| setter.get("label")));

    build_contains_rule(
        row,
        rule_id,
        needles,
        category_id(category, subcategory),
        None,
        None,
        amount_sign,
    )
    .map(|rule| Some(CompiledRule { label, ..rule }))
}

fn optional_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// The name a rule's label gives a transaction: the label plus the
/// installment the bank text carries (`3/10`), so each parcel keeps its own.
pub fn labeled_description(label: &str, raw_description: &str) -> String {
    let label = label.trim();
    if parse_installment_description(label).is_some() {
        return label.to_string();
    }
    match parse_installment_description(raw_description) {
        Some(marker) => format!("{label} {}/{}", marker.current, marker.total),
        None => label.to_string(),
    }
}

fn build_contains_rule(
    row: &RuleRecord,
    rule_id: &str,
    needles: Vec<String>,
    category_id: String,
    context: Option<String>,
    amount_match: Option<Decimal>,
    amount_sign: Option<AmountSign>,
) -> Result<CompiledRule> {
    let needles = needles
        .into_iter()
        .map(|needle| needle.trim().to_string())
        .filter(|needle| !needle.is_empty())
        .collect::<Vec<_>>();
    if needles.is_empty() {
        bail!("Rule {rule_id} sem termo de busca");
    }
    if category_id.trim().is_empty() {
        bail!("Rule {rule_id} sem categoria de destino");
    }
    Ok(CompiledRule {
        rule_id: rule_id.to_string(),
        category_id,
        context,
        amount_sign,
        label: None,
        matcher: RuleMatcher::ContainsAny(needles),
        amount_match,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn build_exact_rule(
    row: &RuleRecord,
    rule_id: &str,
    needle: String,
    category_id: String,
    context: Option<String>,
    amount_match: Option<Decimal>,
    amount_sign: Option<AmountSign>,
) -> Result<CompiledRule> {
    let needle = needle.trim().to_string();
    if needle.is_empty() {
        bail!("Rule {rule_id} sem termo de busca");
    }
    if category_id.trim().is_empty() {
        bail!("Rule {rule_id} sem categoria de destino");
    }
    Ok(CompiledRule {
        rule_id: rule_id.to_string(),
        category_id,
        context,
        amount_sign,
        label: None,
        matcher: RuleMatcher::Exact(needle),
        amount_match,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn build_transaction_id_rule(
    row: &RuleRecord,
    rule_id: &str,
    transaction_id: String,
    category_id: String,
    context: Option<String>,
    amount_sign: Option<AmountSign>,
) -> Result<CompiledRule> {
    if transaction_id.trim().is_empty() {
        bail!("Rule {rule_id} sem `match_value` válido para `pluggy_id`");
    }
    if category_id.trim().is_empty() {
        bail!("Rule {rule_id} sem categoria de destino");
    }
    Ok(CompiledRule {
        rule_id: rule_id.to_string(),
        category_id,
        context,
        amount_sign,
        label: None,
        matcher: RuleMatcher::TransactionIdExact(transaction_id),
        amount_match: None,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn parse_amount_sign(value: Option<&Value>) -> Result<Option<AmountSign>> {
    let raw = value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(raw) = raw else {
        return Ok(None);
    };

    match raw.to_ascii_lowercase().as_str() {
        "positive" => Ok(Some(AmountSign::Positive)),
        "negative" => Ok(Some(AmountSign::Negative)),
        other => bail!("amount_sign inválido: {other}. Valores aceitos: positive, negative"),
    }
}

fn normalize_category_target(raw: &str) -> Result<String> {
    let trimmed = strip_wrapping_quotes(raw.trim());
    if trimmed.is_empty() {
        bail!("Categoria de destino vazia");
    }
    let mut parts = trimmed.splitn(2, ':');
    let category = parts.next().unwrap_or_default();
    let subcategory = parts.next();
    Ok(category_id(category, subcategory))
}

fn strip_wrapping_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn normalize_text(value: &str) -> String {
    deunicode(value).to_ascii_lowercase()
}

fn normalize_identifier(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn contains_bounded(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack.match_indices(needle).any(|(start, _)| {
        let end = start + needle.len();
        let before_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        before_ok && after_ok
    })
}

fn amounts_match(actual: Decimal, expected: Decimal) -> bool {
    actual.abs().round_dp(2) == expected.abs().round_dp(2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn rule(rule_id: &str, body: &str) -> RuleRecord {
        rule_at(
            rule_id,
            body,
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap(),
        )
    }

    fn rule_at(rule_id: &str, body: &str, updated_at: DateTime<Utc>) -> RuleRecord {
        RuleRecord {
            rule_id: rule_id.to_string(),
            body: body.to_string(),
            status: "active".to_string(),
            actor_id: "test-actor".to_string(),
            idempotency_key: format!("rule:{rule_id}"),
            created_at: updated_at,
            updated_at,
        }
    }

    #[test]
    fn parses_dsl_rule_with_quotes() {
        let compiled = compile_rules(&[rule(
            "pix_credito",
            "if description contains \"PIX no Crédito\" then category transfer-internal",
        )])
        .unwrap();

        let (category_id, source) = apply_rules(
            "Valor adicionado na conta via PIX no Credito",
            None,
            false,
            &compiled,
        );
        assert_eq!(category_id.as_deref(), Some("transfer-internal"));
        assert_eq!(source, "rule");
    }

    #[test]
    fn parses_legacy_context_json_rule() {
        let compiled = compile_rules(&[rule(
            "mercado_rule",
            r#"{"match_type":"descricao_contains","match_value":"mercado","categoria":"Alimentacao","subcategoria":"Mercado"}"#,
        )])
        .unwrap();

        let (category_id, source) = apply_rules("Compra no Mercado X", None, false, &compiled);
        assert_eq!(category_id.as_deref(), Some("alimentacao:mercado"));
        assert_eq!(source, "rule");
    }

    #[test]
    fn parses_legacy_exact_json_rule() {
        let compiled = compile_rules(&[rule(
            "bill_rule",
            r#"{"match_type":"descricao_exata","match_value":"Pagamento de fatura","categoria":"credit-card-payment"}"#,
        )])
        .unwrap();

        let (category_id, source) = apply_rules("Pagamento de fatura", None, false, &compiled);
        assert_eq!(category_id.as_deref(), Some("credit-card-payment"));
        assert_eq!(source, "rule");

        let (category_id, source) = apply_rules("Pagamento de fatura Visa", None, false, &compiled);
        assert_eq!(category_id, None);
        assert_eq!(source, "unclassified");
    }

    #[test]
    fn ignores_unsupported_json_rule_shapes() {
        let compiled = compile_rules(&[rule("unknown_rule", r#"{"foo":"bar"}"#)]).unwrap();
        assert!(compiled.is_empty());
    }

    #[test]
    fn first_rule_is_deterministic_by_rule_id() {
        let compiled = compile_rules(&[
            rule(
                "20_secondary",
                "if description contains farmacia then category saude:secundario",
            ),
            rule(
                "10_primary",
                "if description contains farmacia then category saude:farmacia",
            ),
        ])
        .unwrap();

        let (category_id, source) = apply_rules("Farmacia Central", None, false, &compiled);
        assert_eq!(category_id.as_deref(), Some("saude:farmacia"));
        assert_eq!(source, "rule");
    }

    #[test]
    fn newer_rule_wins_equal_specificity() {
        let old = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let new = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();
        let compiled = compile_rules(&[
            rule_at(
                "a_old",
                "if description contains farmacia then category saude:farmacia",
                old,
            ),
            rule_at(
                "z_new",
                "if description contains farmacia then category saude:consulta",
                new,
            ),
        ])
        .unwrap();

        let (category_id, _) = apply_rules("Farmacia Central", None, false, &compiled);
        assert_eq!(category_id.as_deref(), Some("saude:consulta"));
    }

    #[test]
    fn contains_rule_does_not_match_inside_word() {
        let compiled = compile_rules(&[rule(
            "uber_rule",
            "if description contains uber then category transporte:taxi-app",
        )])
        .unwrap();

        let (category_id, source) = apply_rules("SUPERMERCADO UBERLANDIA", None, false, &compiled);
        assert_eq!(category_id, None);
        assert_eq!(source, "unclassified");
    }

    #[test]
    fn context_rule_with_amount_overrides_generic_rule() {
        let compiled = compile_rules(&[
            rule(
                "000_generic",
                "if description contains \"distribuidora exemplo\" then category moradia:gas",
            ),
            rule(
                "context_specific",
                r#"{"match_type":"descricao_contains","match_value":"distribuidora exemplo de agua","valor_match":"22","categoria":"Alimentacao","subcategoria":"Mercado","finalidade":"Reposição de água para casa"}"#,
            ),
        ])
        .unwrap();

        let result = apply_rules_with_amount(
            "Transferência enviada|Distribuidora Exemplo de Agua",
            Some(Decimal::new(-2200, 2)),
            None,
            false,
            &compiled,
        );

        assert_eq!(result.category_id.as_deref(), Some("alimentacao:mercado"));
        assert_eq!(
            result.context.as_deref(),
            Some("Reposição de água para casa")
        );
        assert_eq!(result.category_source, "rule");
    }

    #[test]
    fn pluggy_id_rule_matches_transaction_id() {
        let compiled = compile_rules(&[
            rule(
                "000_generic",
                "if description contains transferencia enviada then category transfer-out",
            ),
            rule(
                "context_specific",
                r#"{"match_type":"pluggy_id","match_value":"00000000-0000-4000-8000-000000000123","categoria":"Educacao","subcategoria":"Material Escolar","finalidade":"Compra de material escolar"}"#,
            ),
        ])
        .unwrap();

        let result = apply_rules_with_facts(
            "Transferência enviada|Amazon",
            Some(Decimal::new(-4999, 2)),
            Some("00000000-0000-4000-8000-000000000123"),
            None,
            false,
            &compiled,
        );

        assert_eq!(
            result.category_id.as_deref(),
            Some("educacao:material-escolar")
        );
        assert_eq!(
            result.context.as_deref(),
            Some("Compra de material escolar")
        );
        assert_eq!(result.category_source, "rule");
    }

    #[test]
    fn parses_amount_sign_from_legacy_context_rule() {
        let compiled = compile_rules(&[rule(
            "iof_cashback",
            r#"{"match_type":"descricao_contains","match_value":"iof de volta","categoria":"Receitas","subcategoria":"Cashback / Beneficios","amount_sign":"positive"}"#,
        )])
        .unwrap();

        let result = apply_rules_with_facts(
            "IOF de volta de assinatura",
            Some(Decimal::new(-72, 2)),
            Some("00000000-0000-4000-8000-000000000321"),
            None,
            false,
            &compiled,
        );

        assert_eq!(
            result.category_id.as_deref(),
            Some("receitas:cashback-beneficios")
        );
        assert_eq!(result.amount_sign, Some(AmountSign::Positive));
    }

    #[test]
    fn parses_amount_sign_from_yaml_style_rule() {
        let compiled = compile_rules(&[rule(
            "bonus_rule",
            r#"{"id":"bonus_rule","match":{"contains_any":["bonus"]},"set":{"category":"Receitas","subcategory":"Cashback / Beneficios","amount_sign":"positive"}}"#,
        )])
        .unwrap();

        let result = apply_rules_with_facts(
            "Bonus de estorno",
            Some(Decimal::new(-100, 2)),
            None,
            None,
            false,
            &compiled,
        );

        assert_eq!(result.amount_sign, Some(AmountSign::Positive));
    }

    fn label_for(body: &str, text: &str) -> Option<String> {
        let compiled = compile_rules(&[rule("labeled", body)]).unwrap();
        apply_rules_with_facts(text, None, None, None, false, &compiled).label
    }

    #[test]
    fn context_rule_rotulo_names_what_it_matches() {
        let body = r#"{"match_type":"descricao_contains","match_value":"joana souza","categoria":"Moradia","subcategoria":"Servicos","rotulo":"Faxina semanal"}"#;
        assert_eq!(
            label_for(body, "Pix enviado|JOANA SOUZA").as_deref(),
            Some("Faxina semanal")
        );
    }

    #[test]
    fn yaml_rule_label_comes_from_set_label() {
        let body = r#"{"id":"x","match":{"contains_any":["oficina beta"]},"set":{"category":"Transporte","subcategory":"Manutencao","label":"Revisão do carro"}}"#;
        assert_eq!(
            label_for(body, "OFICINA BETA LTDA").as_deref(),
            Some("Revisão do carro")
        );
    }

    #[test]
    fn rules_without_a_label_leave_the_name_alone() {
        let dsl = r#"if description contains "mercado" then category alimentacao:mercado"#;
        assert_eq!(label_for(dsl, "Mercado X"), None);
        let blank = r#"{"match_type":"descricao_contains","match_value":"mercado","categoria":"Mercado","rotulo":"  "}"#;
        assert_eq!(label_for(blank, "Mercado X"), None);
        let other = r#"{"match_type":"descricao_contains","match_value":"padaria","categoria":"Mercado","rotulo":"Pão"}"#;
        assert_eq!(label_for(other, "Mercado X"), None);
    }

    #[test]
    fn labeled_description_keeps_the_installment_of_the_raw_text() {
        assert_eq!(
            labeled_description("Curso de bordado", "HTM*ATELIE X 8/12"),
            "Curso de bordado 8/12"
        );
        assert_eq!(
            labeled_description("Curso de bordado", "Atelie X - Parcela 3 de 10"),
            "Curso de bordado 3/10"
        );
        assert_eq!(
            labeled_description("Mensalidade", "Pix enviado|ESCOLA"),
            "Mensalidade"
        );
        assert_eq!(labeled_description("Sofá 2/10", "LOJA 2/10"), "Sofá 2/10");
    }
}
