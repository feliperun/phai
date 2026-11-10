//! Helpers for producing human-friendly, WhatsApp-readable report output.
//!
//! Reports default to this format; pass `--raw` (or the deprecated `--json`)
//! to get structured JSON for agents and scripts.

use chrono::{Datelike, NaiveDate, Utc};
use rust_decimal::Decimal;

/// Format an amount as Brazilian Real with grouping and 2 decimals.
/// `+R$ 1.234,56` for positive, `-R$ 1.234,56` for negative, `R$ 0,00`
/// for values that round to zero (suppresses the spurious sign).
pub fn brl_signed(value: Decimal) -> String {
    let formatted = format_brl_number(value.abs());
    let is_display_zero = formatted.chars().all(|c| matches!(c, '0' | ',' | '.'));
    if is_display_zero {
        return format!("R$ {formatted}");
    }
    let sign = if value.is_sign_negative() { "-" } else { "+" };
    format!("{sign}R$ {formatted}")
}

/// Format an unsigned amount as Brazilian Real (no leading sign).
/// `R$ 1.234,56`.
pub fn brl(value: Decimal) -> String {
    format!("R$ {}", format_brl_number(value.abs()))
}

/// Group thousands with `.` and use `,` as decimal separator.
fn format_brl_number(value: Decimal) -> String {
    let rounded = format!("{:.2}", value.round_dp(2));
    let (int_part, dec_part) = rounded.split_once('.').unwrap_or((&rounded, "00"));
    let int_with_groups = group_thousands(int_part);
    format!("{int_with_groups},{dec_part}")
}

fn group_thousands(int_part: &str) -> String {
    let chars: Vec<char> = int_part.chars().rev().collect();
    let mut out = String::new();
    for (i, c) in chars.iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('.');
        }
        out.push(*c);
    }
    out.chars().rev().collect()
}

/// Short human date: `hoje`, `ontem`, `13/mai`, or `13/mai/2025` if year differs.
pub fn short_date(date: NaiveDate) -> String {
    let today = Utc::now().date_naive();
    let days_diff = today.signed_duration_since(date).num_days();
    if days_diff == 0 {
        "hoje".to_string()
    } else if days_diff == 1 {
        "ontem".to_string()
    } else {
        let day = date.day();
        let month = month_pt_short(date.month());
        if date.year() == today.year() {
            format!("{day}/{month}")
        } else {
            format!("{day}/{month}/{}", date.year())
        }
    }
}

/// Like `short_date` but includes HH:MM when the timestamp is from today.
/// Useful for account snapshots so the user knows the balance may be hours old:
/// "hoje 00:34" is more informative than just "hoje" when asking why it diverges
/// from the bank app viewed mid-day.
pub fn short_snapshot_ts(ts: chrono::DateTime<chrono::Utc>) -> String {
    let today = Utc::now().date_naive();
    let local_ts = ts.with_timezone(&chrono::Local);
    if local_ts.date_naive() == today {
        format!("hoje {}", local_ts.format("%H:%M"))
    } else {
        short_date(local_ts.date_naive())
    }
}

pub fn month_pt_short(month: u32) -> &'static str {
    match month {
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
        _ => "??",
    }
}

/// Extract the top-level family of a colon-delimited category id.
/// `"alimentacao:restaurantes"` → `Some("alimentacao")`.
///
/// Also remaps known English categories that come from Pluggy's default
/// classifier (when no user rule matched during sync) to their canonical
/// Brazilian-Portuguese family. The underlying transaction keeps its raw
/// category_id; this remapping is purely a display-time normalization so
/// the report doesn't mix Portuguese and English category labels.
pub fn category_family(category_id: Option<&str>) -> Option<String> {
    let raw = category_id?.trim();
    if raw.is_empty() {
        return None;
    }
    let normalized = raw.replace([':', '-', '>'], " ");
    let first = normalized.split_whitespace().next()?;
    // Pluggy's defaults arrive as bare lowercase tokens. Map them onto our
    // PT family taxonomy. Anything we don't recognise is returned as-is so
    // user-defined families keep working.
    let mapped = match first.to_lowercase().as_str() {
        "groceries" | "food" | "eating" | "restaurants" => "alimentacao",
        "shopping" | "clothing" | "online" => "compras",
        "services" => "servicos",
        "leisure" | "entertainment" | "gambling" | "gaming" | "sports" | "kids" => "lazer",
        "houseware" | "household" | "home" => "casa",
        "transport" | "transportation" | "automotive" | "fuel" | "airport" | "parking"
        | "vehicle" => "transporte",
        "health" | "pharmacy" | "medical" | "hospital" => "saude",
        "education" | "bookstore" => "educacao",
        "subscriptions" => "assinaturas",
        "personal" => "pessoal",
        "utilities" | "telecommunications" => "moradia",
        "travel" | "accomodation" => "lazer",
        "pets" => "lazer",
        "income" | "salary" => "receitas",
        "bank" => "financeiro",
        _ => first,
    };
    Some(mapped.to_string())
}

/// Emoji for a category family. Falls back to `💸` for unknown expense, `❓`
/// for missing categorization.
pub fn category_emoji(category_id: Option<&str>, amount: Option<Decimal>) -> &'static str {
    let family = category_family(category_id);
    if family.as_deref() == Some("receitas")
        || family.as_deref() == Some("salario")
        || amount.is_some_and(|v| v > Decimal::ZERO)
    {
        "💰"
    } else if family.as_deref().is_some_and(|f| f.starts_with("transfer")) {
        "🔁"
    } else if family.as_deref() == Some("assinaturas") {
        "🔂"
    } else if matches!(family.as_deref(), Some("moradia" | "casa")) {
        "🏠"
    } else if family.as_deref() == Some("alimentacao") {
        "🍽️"
    } else if family.as_deref() == Some("saude") {
        "🩺"
    } else if matches!(family.as_deref(), Some("transporte" | "mobilidade")) {
        "🚗"
    } else if family.as_deref() == Some("educacao") {
        "📚"
    } else if family.as_deref() == Some("lazer") {
        "🎉"
    } else if family.as_deref() == Some("investimentos") {
        "📈"
    } else if family.as_deref() == Some("financeiro") {
        "🧾"
    } else if family.as_deref() == Some("vestuario") {
        "👕"
    } else if family.is_none() {
        "❓"
    } else {
        "💸"
    }
}

/// Human label for a category family, in Brazilian Portuguese.
/// `"alimentacao"` → `"Alimentação"`.
pub fn family_label(family: &str) -> String {
    match family {
        "alimentacao" => "Alimentação".into(),
        "moradia" => "Moradia".into(),
        "casa" => "Casa".into(),
        "compras" => "Compras".into(),
        "pessoal" => "Pessoal".into(),
        "servicos" => "Serviços".into(),
        "transporte" => "Transporte".into(),
        "mobilidade" => "Mobilidade".into(),
        "saude" => "Saúde".into(),
        "lazer" => "Lazer".into(),
        "educacao" => "Educação".into(),
        "vestuario" => "Vestuário".into(),
        "assinaturas" => "Assinaturas".into(),
        "investimentos" => "Investimentos".into(),
        "financeiro" => "Financeiro".into(),
        "receitas" => "Entradas".into(),
        "salario" => "Salário".into(),
        other if other.starts_with("transfer") => "Transferências".into(),
        other => capitalize_first(other),
    }
}

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Strip a transaction description down to the bit a human cares about:
/// remove pipe-joined raw tags, leading emoji (already shown in the group
/// header), collapse whitespace.
pub fn short_description(description: &str) -> String {
    let main = description.split('|').next().unwrap_or(description).trim();
    // Skip leading non-alphanumeric tokens (typically emojis injected by
    // display_label upstream). The first token that starts with an alnum
    // letter is the real start of the description.
    let cleaned = main
        .split_whitespace()
        .skip_while(|token| !token.chars().next().is_some_and(|c| c.is_alphanumeric()))
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        main.to_string()
    } else {
        cleaned
    }
}

/// WhatsApp bold marker.
pub fn bold(s: &str) -> String {
    format!("*{s}*")
}

/// Render a subsection delimiter as `── *Title* ──`, used inside multi-block
/// reports (e.g. card-closed-insights) where visual grouping helps but
/// emojis would feel repetitive.
pub fn subsection_header(title: &str) -> String {
    format!("── {} ──", bold(title))
}

/// Format a percentage value as `XX%` (rounded to integer).
pub fn pct(value: Decimal) -> String {
    let rounded = value.round_dp(0);
    format!("{rounded}%")
}

/// Format `"YYYY-MM"` as `"<month-pt>/<YYYY>"`. Falls back to the raw string
/// if it doesn't parse.
/// `"2026-05"` → `"maio/2026"`.
pub fn month_label(month_ref: &str) -> String {
    let parts: Vec<&str> = month_ref.split('-').collect();
    if parts.len() != 2 {
        return month_ref.to_string();
    }
    let year = match parts[0].parse::<i32>() {
        Ok(y) => y,
        Err(_) => return month_ref.to_string(),
    };
    let month = match parts[1].parse::<u32>() {
        Ok(m) if (1..=12).contains(&m) => m,
        _ => return month_ref.to_string(),
    };
    format!("{}/{}", month_pt_full(month), year)
}

fn month_pt_full(month: u32) -> &'static str {
    match month {
        1 => "janeiro",
        2 => "fevereiro",
        3 => "março",
        4 => "abril",
        5 => "maio",
        6 => "junho",
        7 => "julho",
        8 => "agosto",
        9 => "setembro",
        10 => "outubro",
        11 => "novembro",
        12 => "dezembro",
        _ => "??",
    }
}

/// Truncate `s` to at most `max_chars` (counted by Unicode scalar values),
/// appending `…` when truncated. Returns the original string when within
/// the limit. Used to keep WhatsApp-friendly labels from running long.
pub fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    // Reserve one slot for the ellipsis.
    let cutoff = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(cutoff).collect();
    out.push('…');
    out
}

/// Render a 10-cell unicode progress bar, `[▓▓▓░░░░░░░] 35%`.
/// Caps fill at 10 cells even if `pct > 100`.
pub fn progress_bar(pct: i64) -> String {
    let pct_clamped = pct.clamp(0, 100);
    let filled = ((pct_clamped as f64 / 10.0).round() as usize).min(10);
    let empty = 10 - filled;
    format!("[{}{}] {}%", "▓".repeat(filled), "░".repeat(empty), pct)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn brl_signed_formats_positive() {
        assert_eq!(brl_signed(dec!(1234.56)), "+R$ 1.234,56");
    }

    #[test]
    fn brl_signed_formats_negative() {
        assert_eq!(brl_signed(dec!(-1234.56)), "-R$ 1.234,56");
    }

    #[test]
    fn brl_groups_thousands() {
        assert_eq!(brl(dec!(1234567.89)), "R$ 1.234.567,89");
    }

    #[test]
    fn brl_rounds_to_two_decimals() {
        assert_eq!(brl(dec!(1.2345)), "R$ 1,23");
    }

    #[test]
    fn brl_handles_zero() {
        assert_eq!(brl(Decimal::ZERO), "R$ 0,00");
    }

    #[test]
    fn category_family_extracts_prefix() {
        assert_eq!(
            category_family(Some("alimentacao:restaurantes")),
            Some("alimentacao".into())
        );
        assert_eq!(category_family(Some("moradia")), Some("moradia".into()));
        assert_eq!(category_family(Some("  ")), None);
        assert_eq!(category_family(None), None);
    }

    #[test]
    fn family_label_translates_known_families() {
        assert_eq!(family_label("alimentacao"), "Alimentação");
        assert_eq!(family_label("saude"), "Saúde");
        assert_eq!(family_label("transferencias_internas"), "Transferências");
    }

    #[test]
    fn family_label_falls_back_to_capitalize() {
        assert_eq!(family_label("custom"), "Custom");
    }

    #[test]
    fn short_description_strips_pipe_suffix() {
        assert_eq!(
            short_description("Transferência enviada|Maicon Steffen Rios"),
            "Transferência enviada"
        );
    }

    #[test]
    fn short_description_collapses_whitespace() {
        assert_eq!(
            short_description("  Mercado    Angeloni  \n  loja 42  "),
            "Mercado Angeloni loja 42"
        );
    }

    #[test]
    fn short_description_strips_leading_emoji() {
        assert_eq!(
            short_description("🏠 Aluguel do imóvel"),
            "Aluguel do imóvel"
        );
        assert_eq!(short_description("🍽️ Mercado Angeloni"), "Mercado Angeloni");
    }

    #[test]
    fn short_description_keeps_purely_emoji_input() {
        // If the description is just emoji, don't return empty
        let out = short_description("🏠 🚗");
        assert!(!out.is_empty());
    }

    #[test]
    fn pct_formats_integer_percentage() {
        assert_eq!(pct(dec!(35.4)), "35%");
        assert_eq!(pct(dec!(35.6)), "36%");
        assert_eq!(pct(dec!(100)), "100%");
        assert_eq!(pct(dec!(0)), "0%");
    }

    #[test]
    fn month_label_formats_known_month() {
        assert_eq!(month_label("2026-05"), "maio/2026");
        assert_eq!(month_label("2024-12"), "dezembro/2024");
        assert_eq!(month_label("2030-01"), "janeiro/2030");
    }

    #[test]
    fn month_label_falls_back_on_invalid_input() {
        assert_eq!(month_label("not-a-month"), "not-a-month");
        assert_eq!(month_label("2026-13"), "2026-13");
        assert_eq!(month_label(""), "");
    }

    #[test]
    fn progress_bar_renders_at_thresholds() {
        assert_eq!(progress_bar(0), "[░░░░░░░░░░] 0%");
        assert_eq!(progress_bar(50), "[▓▓▓▓▓░░░░░] 50%");
        assert_eq!(progress_bar(100), "[▓▓▓▓▓▓▓▓▓▓] 100%");
    }

    #[test]
    fn progress_bar_caps_overflow_visually_but_shows_real_pct() {
        // Bar fill caps at 10 cells but the percentage label can exceed 100.
        let out = progress_bar(150);
        assert!(out.starts_with("[▓▓▓▓▓▓▓▓▓▓]"));
        assert!(out.ends_with("150%"));
    }
}
