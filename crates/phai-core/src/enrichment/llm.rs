//! LLM provider selection and enrichment call.
//!
//! rig-core ships native providers for Anthropic and OpenAI; for
//! Deepseek and Ollama we use the OpenAI provider with a custom
//! `base_url` (both expose OpenAI-compatible endpoints).
//!
//! Provider precedence:
//!   1. `FINANCE_LLM_PROVIDER` env var (`anthropic|openai|deepseek|ollama`)
//!   2. `AppConfig::llm_provider`
//!   3. First env var found in this order:
//!      `ANTHROPIC_API_KEY` → `OPENAI_API_KEY` → `DEEPSEEK_API_KEY` →
//!      `OLLAMA_BASE_URL`
//!
//! Model precedence:
//!   1. `FINANCE_LLM_MODEL`
//!   2. `AppConfig::llm_model`
//!   3. Provider default.
//!
//! `enrich` strategy:
//!   - Anthropic / OpenAI / Deepseek → try `Extractor<EnrichmentResult>`
//!     (tool calling).
//!   - Ollama → completion API + manual JSON parsing (Ollama's
//!     tool-calling through rig was unreliable as of rig 0.6).
//!   - On parse failure: greedy-regex `{…}` extraction and one retry
//!     with `\n\nRETURN ONLY VALID JSON MATCHING THE SCHEMA.` appended.
//!   - Both failures bubble up as `anyhow::Error`.

use crate::config::AppConfig;
use crate::enrichment::types::EnrichmentResult;
use anyhow::{anyhow, Context, Result};
use rig::completion::Prompt;
use rig::providers::{anthropic, openai};

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5-20251001";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
const DEFAULT_DEEPSEEK_MODEL: &str = "deepseek-v4-flash";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";

const DEEPSEEK_BASE_URL: &str = "https://api.deepseek.com/v1";
const DEFAULT_OLLAMA_BASE_URL: &str = "http://localhost:11434/v1";

const RETRY_SUFFIX: &str = "\n\nRETURN ONLY VALID JSON MATCHING THE SCHEMA.";

/// Active LLM provider selected at runtime.
#[derive(Debug, Clone)]
pub enum LlmProvider {
    Anthropic { api_key: String, model: String },
    Openai { api_key: String, model: String },
    Deepseek { api_key: String, model: String },
    Ollama { base_url: String, model: String },
}

impl LlmProvider {
    /// Resolve provider + model from env then config.
    pub fn from_env_or_config(config: &AppConfig) -> Result<Self> {
        let explicit = std::env::var("FINANCE_LLM_PROVIDER")
            .ok()
            .or_else(|| config.llm_provider.clone());

        let model_override = std::env::var("FINANCE_LLM_MODEL")
            .ok()
            .or_else(|| config.llm_model.clone());

        if let Some(name) = explicit {
            return Self::build(&name, model_override);
        }

        // No explicit choice — fall back to first env var found.
        if std::env::var("ANTHROPIC_AUTH_TOKEN").is_ok()
            || std::env::var("ANTHROPIC_API_KEY").is_ok()
        {
            return Self::build("anthropic", model_override);
        }
        if std::env::var("OPENAI_API_KEY").is_ok() {
            return Self::build("openai", model_override);
        }
        if std::env::var("DEEPSEEK_API_KEY").is_ok() {
            return Self::build("deepseek", model_override);
        }
        if std::env::var("OLLAMA_BASE_URL").is_ok() {
            return Self::build("ollama", model_override);
        }

        Err(anyhow!(
            "nenhum provedor LLM configurado: defina FINANCE_LLM_PROVIDER, \
             ANTHROPIC_API_KEY, OPENAI_API_KEY, DEEPSEEK_API_KEY ou OLLAMA_BASE_URL"
        ))
    }

    fn build(name: &str, model_override: Option<String>) -> Result<Self> {
        let name_norm = name.trim().to_lowercase();
        match name_norm.as_str() {
            "anthropic" => {
                // Accept ANTHROPIC_AUTH_TOKEN (Claude Code CLI convention)
                // or the classic ANTHROPIC_API_KEY — whichever is set.
                let api_key = std::env::var("ANTHROPIC_AUTH_TOKEN")
                    .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
                    .context(
                        "nenhuma chave Anthropic encontrada: \
                         defina ANTHROPIC_AUTH_TOKEN ou ANTHROPIC_API_KEY",
                    )?;
                let model = model_override.unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
                Ok(Self::Anthropic { api_key, model })
            }
            "openai" => {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .context("OPENAI_API_KEY ausente para provedor openai")?;
                let model = model_override.unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
                Ok(Self::Openai { api_key, model })
            }
            "deepseek" => {
                let api_key = std::env::var("DEEPSEEK_API_KEY")
                    .context("DEEPSEEK_API_KEY ausente para provedor deepseek")?;
                let model = model_override.unwrap_or_else(|| DEFAULT_DEEPSEEK_MODEL.to_string());
                Ok(Self::Deepseek { api_key, model })
            }
            "ollama" => {
                let base_url = std::env::var("OLLAMA_BASE_URL")
                    .unwrap_or_else(|_| DEFAULT_OLLAMA_BASE_URL.to_string());
                let model = model_override.unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
                Ok(Self::Ollama { base_url, model })
            }
            other => Err(anyhow!("provedor LLM desconhecido: {other}")),
        }
    }

    /// Hard-coded default model per provider. Useful for diagnostics and
    /// for the CLI to print what will be used.
    pub fn default_model(&self) -> &str {
        match self {
            Self::Anthropic { .. } => DEFAULT_ANTHROPIC_MODEL,
            Self::Openai { .. } => DEFAULT_OPENAI_MODEL,
            Self::Deepseek { .. } => DEFAULT_DEEPSEEK_MODEL,
            Self::Ollama { .. } => DEFAULT_OLLAMA_MODEL,
        }
    }
}

/// Reasonable upper bound for the JSON enrichment payload (plenty for a
/// single `EnrichmentResult` object). The Anthropic API rejects requests
/// that omit `max_tokens`, so every `AgentBuilder` call must set this.
const ANTHROPIC_MAX_TOKENS: u64 = 1024;

/// Call the LLM with `prompt` and return the parsed `EnrichmentResult`.
///
/// Strategy:
///   - Anthropic / OpenAI / Deepseek → `rig::Extractor<EnrichmentResult>`.
///   - Ollama → completion API + manual JSON parsing.
///
/// On JSON parse failure (Ollama path or extractor returning malformed
/// JSON), retry once with the prompt + [`RETRY_SUFFIX`] using the
/// plain-completion path. Both failures surface as `anyhow::Error`.
///
/// # Anthropic note
/// The Anthropic API requires `max_tokens` on every request. The
/// `Extractor` path handles this internally via rig; the completion
/// fallback (`retry_via_completion_anthropic`) must set it explicitly
/// via `.max_tokens(ANTHROPIC_MAX_TOKENS)` on the `AgentBuilder`.
pub async fn enrich(provider: &LlmProvider, prompt: &str) -> Result<EnrichmentResult> {
    match provider {
        LlmProvider::Anthropic { api_key, model } => {
            let client = anthropic::ClientBuilder::new(api_key).build();
            let extractor = client.extractor::<EnrichmentResult>(model).build();
            match extractor.extract(prompt).await {
                Ok(result) => Ok(result),
                Err(_) => retry_via_completion_anthropic(&client, model, prompt).await,
            }
        }
        LlmProvider::Openai { api_key, model } => {
            let client = openai::Client::new(api_key);
            let extractor = client.extractor::<EnrichmentResult>(model).build();
            match extractor.extract(prompt).await {
                Ok(result) => Ok(result),
                Err(_) => retry_via_completion_openai(&client, model, prompt).await,
            }
        }
        LlmProvider::Deepseek { api_key, model } => {
            let client = openai::Client::from_url(api_key, DEEPSEEK_BASE_URL);
            let extractor = client.extractor::<EnrichmentResult>(model).build();
            match extractor.extract(prompt).await {
                Ok(result) => Ok(result),
                Err(_) => retry_via_completion_openai(&client, model, prompt).await,
            }
        }
        LlmProvider::Ollama { base_url, model } => {
            // Ollama-via-OpenAI-compatible endpoint. No api key required;
            // pass an empty string.
            let client = openai::Client::from_url("ollama", base_url);
            let agent = client.agent(model).build();
            let raw = agent
                .prompt(prompt)
                .await
                .context("falha ao chamar Ollama via API OpenAI-compatível")?;
            match parse_json_lenient(&raw) {
                Ok(r) => Ok(r),
                Err(_) => {
                    let retry_prompt = format!("{prompt}{RETRY_SUFFIX}");
                    let raw2 = agent
                        .prompt(retry_prompt.as_str())
                        .await
                        .context("retry Ollama falhou")?;
                    parse_json_lenient(&raw2).context("Ollama retornou JSON inválido após retry")
                }
            }
        }
    }
}

async fn retry_via_completion_anthropic(
    client: &anthropic::Client,
    model: &str,
    prompt: &str,
) -> Result<EnrichmentResult> {
    let retry_prompt = format!("{prompt}{RETRY_SUFFIX}");
    // `max_tokens` is mandatory for the Anthropic API; omitting it causes a
    // 400 "max_tokens must be set" error.
    let agent = client.agent(model).max_tokens(ANTHROPIC_MAX_TOKENS).build();
    let raw = agent
        .prompt(retry_prompt.as_str())
        .await
        .context("retry Anthropic completion falhou")?;
    parse_json_lenient(&raw).context("Anthropic retornou JSON inválido após retry")
}

async fn retry_via_completion_openai(
    client: &openai::Client,
    model: &str,
    prompt: &str,
) -> Result<EnrichmentResult> {
    let retry_prompt = format!("{prompt}{RETRY_SUFFIX}");
    let agent = client.agent(model).build();
    let raw = agent
        .prompt(retry_prompt.as_str())
        .await
        .context("retry OpenAI-compatible completion falhou")?;
    parse_json_lenient(&raw).context("OpenAI-compatible retornou JSON inválido após retry")
}

/// Attempt a direct `serde_json::from_str`; on failure, greedy-extract
/// the first `{ ... }` block (so we tolerate ```json fences, prose, or
/// trailing whitespace).
fn parse_json_lenient(text: &str) -> Result<EnrichmentResult> {
    if let Ok(parsed) = serde_json::from_str::<EnrichmentResult>(text) {
        return parsed.validate_llm_output();
    }
    let start = text.find('{').context("nenhum '{' na resposta do LLM")?;
    let end = text.rfind('}').context("nenhum '}' na resposta do LLM")?;
    if end <= start {
        return Err(anyhow!("delimitadores JSON inválidos na resposta"));
    }
    let candidate = &text[start..=end];
    serde_json::from_str::<EnrichmentResult>(candidate)
        .with_context(|| format!("falha ao parsear JSON extraído: {candidate}"))?
        .validate_llm_output()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use serial_test::serial;

    const ENV_VARS: [&str; 7] = [
        "FINANCE_LLM_PROVIDER",
        "FINANCE_LLM_MODEL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
        "OLLAMA_BASE_URL",
    ];

    /// Clear every LLM-related env var so each test starts from a known
    /// state. `serial_test::serial` guarantees we never race other
    /// tests that touch the same process-global environment.
    fn clear_env() {
        for v in ENV_VARS {
            // SAFETY: tests are serialized via `#[serial]`.
            unsafe {
                std::env::remove_var(v);
            }
        }
    }

    fn set(var: &str, val: &str) {
        // SAFETY: tests are serialized via `#[serial]`.
        unsafe {
            std::env::set_var(var, val);
        }
    }

    #[test]
    #[serial]
    fn test_provider_selection_anthropic_env() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "anthropic");
        set("ANTHROPIC_API_KEY", "sk-anthropic");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        assert!(matches!(p, LlmProvider::Anthropic { .. }));
        clear_env();
    }

    /// ANTHROPIC_AUTH_TOKEN (Claude Code CLI convention) must be accepted as
    /// an alias for ANTHROPIC_API_KEY.
    #[test]
    #[serial]
    fn test_provider_selection_anthropic_auth_token() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "anthropic");
        set("ANTHROPIC_AUTH_TOKEN", "sk-auth-token");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Anthropic { api_key, .. } => assert_eq!(api_key, "sk-auth-token"),
            other => panic!("expected Anthropic, got {other:?}"),
        }
        clear_env();
    }

    /// ANTHROPIC_AUTH_TOKEN takes precedence over ANTHROPIC_API_KEY when
    /// both are set (mirrors Claude Code CLI behaviour).
    #[test]
    #[serial]
    fn test_anthropic_auth_token_takes_precedence() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "anthropic");
        set("ANTHROPIC_AUTH_TOKEN", "tok-priority");
        set("ANTHROPIC_API_KEY", "sk-fallback");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Anthropic { api_key, .. } => assert_eq!(api_key, "tok-priority"),
            other => panic!("expected Anthropic, got {other:?}"),
        }
        clear_env();
    }

    /// Fallback auto-detection must trigger on ANTHROPIC_AUTH_TOKEN (no
    /// explicit FINANCE_LLM_PROVIDER set).
    #[test]
    #[serial]
    fn test_fallback_detects_anthropic_auth_token() {
        clear_env();
        set("ANTHROPIC_AUTH_TOKEN", "tok-autodetect");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        assert!(matches!(p, LlmProvider::Anthropic { .. }));
        clear_env();
    }

    #[test]
    #[serial]
    fn test_provider_selection_deepseek_env() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "deepseek");
        set("DEEPSEEK_API_KEY", "sk-deepseek");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        assert!(matches!(p, LlmProvider::Deepseek { .. }));
        clear_env();
    }

    #[test]
    #[serial]
    fn test_provider_selection_openai_env() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "openai");
        set("OPENAI_API_KEY", "sk-openai");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        assert!(matches!(p, LlmProvider::Openai { .. }));
        clear_env();
    }

    #[test]
    #[serial]
    fn test_provider_selection_ollama_env() {
        clear_env();
        set("FINANCE_LLM_PROVIDER", "ollama");
        set("OLLAMA_BASE_URL", "http://example:11434/v1");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Ollama { base_url, .. } => {
                assert_eq!(base_url, "http://example:11434/v1");
            }
            other => panic!("expected Ollama, got {other:?}"),
        }
        clear_env();
    }

    #[test]
    #[serial]
    fn test_provider_selection_fallback_first_available() {
        clear_env();
        set("OPENAI_API_KEY", "sk-openai");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        assert!(matches!(p, LlmProvider::Openai { .. }));
        clear_env();
    }

    #[test]
    #[serial]
    fn test_provider_selection_error_when_none() {
        clear_env();
        let err = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("nenhum provedor LLM"));
    }

    #[test]
    #[serial]
    fn test_default_model_per_provider() {
        clear_env();
        set("ANTHROPIC_API_KEY", "k");
        set("FINANCE_LLM_PROVIDER", "anthropic");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Anthropic { model, .. } => assert_eq!(model, DEFAULT_ANTHROPIC_MODEL),
            other => panic!("got {other:?}"),
        }
        clear_env();

        set("OPENAI_API_KEY", "k");
        set("FINANCE_LLM_PROVIDER", "openai");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Openai { model, .. } => assert_eq!(model, DEFAULT_OPENAI_MODEL),
            other => panic!("got {other:?}"),
        }
        clear_env();

        set("DEEPSEEK_API_KEY", "k");
        set("FINANCE_LLM_PROVIDER", "deepseek");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Deepseek { model, .. } => assert_eq!(model, DEFAULT_DEEPSEEK_MODEL),
            other => panic!("got {other:?}"),
        }
        clear_env();

        set("OLLAMA_BASE_URL", "http://x");
        set("FINANCE_LLM_PROVIDER", "ollama");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Ollama { model, .. } => assert_eq!(model, DEFAULT_OLLAMA_MODEL),
            other => panic!("got {other:?}"),
        }
        clear_env();
    }

    #[test]
    #[serial]
    fn test_model_override_via_env() {
        clear_env();
        set("ANTHROPIC_API_KEY", "k");
        set("FINANCE_LLM_PROVIDER", "anthropic");
        set("FINANCE_LLM_MODEL", "custom-model-1");
        let p = LlmProvider::from_env_or_config(&AppConfig::default()).unwrap();
        match p {
            LlmProvider::Anthropic { model, .. } => assert_eq!(model, "custom-model-1"),
            other => panic!("got {other:?}"),
        }
        clear_env();
    }

    /// `parse_json_lenient` should handle: pure JSON, fenced JSON, and
    /// prose around JSON. Test it against the fixtures so the parsing
    /// stays compatible with what we expect from real responses.
    #[test]
    fn test_parse_json_lenient_handles_fenced_response() {
        let raw = "Here is the result:\n```json\n{\n  \"reasoning\": \"x\",\n  \"merchant_name\": \"Sapiens\",\n  \"category\": \"alimentacao\",\n  \"subcategory\": \"restaurantes\",\n  \"confidence\": 0.91,\n  \"needs_user_input\": false,\n  \"user_prompt\": null\n}\n```";
        let parsed = parse_json_lenient(raw).unwrap();
        assert_eq!(parsed.merchant_name, "Sapiens");
    }

    #[test]
    fn test_parse_json_lenient_handles_pure_json_fixture() {
        let raw = include_str!("../../tests/fixtures/enrichment/sapiens_response.json");
        let parsed = parse_json_lenient(raw).unwrap();
        assert_eq!(parsed.category, "alimentacao");
        assert_eq!(parsed.subcategory, "restaurantes");
        assert!(parsed.confidence > 0.85);
    }

    #[test]
    fn test_parse_json_lenient_handles_low_confidence_fixture() {
        let raw = include_str!("../../tests/fixtures/enrichment/pix_pessoa_fisica_response.json");
        let parsed = parse_json_lenient(raw).unwrap();
        assert!(parsed.needs_user_input);
        assert!(parsed.user_prompt.is_some());
        assert!(parsed.confidence < 0.60);
    }

    #[test]
    fn test_parse_json_lenient_rejects_unknown_category() {
        let raw = r#"{
            "reasoning": "x",
            "merchant_name": "Coin Shop",
            "category": "cripto",
            "subcategory": "memecoin",
            "confidence": 0.99,
            "needs_user_input": false,
            "user_prompt": null
        }"#;
        assert!(parse_json_lenient(raw).is_err());
    }
}
