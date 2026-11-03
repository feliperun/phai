use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    Bigquery,
    #[default]
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub version: u32,
    pub backend: BackendKind,
    pub actor_id: String,
    pub project_id: Option<String>,
    pub dataset_id: Option<String>,
    pub service_account_path: Option<PathBuf>,
    pub local_db_path: Option<PathBuf>,
    pub pluggy_start_date: Option<String>,
    /// Path to the Pluggy account/item config (`pluggy-config.json`). Set when
    /// the user wants the web app's "sync" button to pull from Pluggy.
    #[serde(default)]
    pub pluggy_config_path: Option<PathBuf>,
    /// Path to a dotenv file holding `PLUGGY_CLIENT_ID` / `PLUGGY_CLIENT_SECRET`,
    /// loaded into the sync subprocess at request time so the creds stay off the
    /// daemon plist.
    #[serde(default)]
    pub pluggy_env_path: Option<PathBuf>,
    /// Path to the account registry (`contas.csv`) that carries each account's
    /// owner, type, bank and label. `sync pluggy` reads it when no
    /// `--accounts-csv` is given.
    #[serde(default)]
    pub pluggy_accounts_csv_path: Option<PathBuf>,
    /// Category ids (`parent` or `parent:sub`) that are committed/fixed (rent,
    /// school, fixed bills): the web serves them with a `locked` tier so they
    /// drop out of planning — for past *and* future transactions, without a
    /// per-transaction override. An explicit override still wins (ADR-0030/0032).
    #[serde(default)]
    pub locked_categories: Vec<String>,
    /// Friendly per-account display names keyed by `account_id`, overriding the
    /// raw bank label from Pluggy (which can be identical across a household's
    /// accounts). Surfaced by `/api/accounts`.
    #[serde(default)]
    pub account_labels: std::collections::HashMap<String, String>,
    /// LLM provider for enrichment (Phase 2+): "anthropic", "openai",
    /// "deepseek", "ollama". Defaults read at runtime — no logic in
    /// Phase 1.
    #[serde(default)]
    pub llm_provider: Option<String>,
    /// LLM model override. Defaults vary per provider.
    #[serde(default)]
    pub llm_model: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            backend: BackendKind::Local,
            actor_id: "unset-actor".to_string(),
            project_id: None,
            dataset_id: None,
            service_account_path: None,
            local_db_path: None,
            pluggy_start_date: None,
            pluggy_config_path: None,
            pluggy_env_path: None,
            pluggy_accounts_csv_path: None,
            locked_categories: Vec::new(),
            account_labels: std::collections::HashMap::new(),
            llm_provider: None,
            llm_model: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConfigPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub config_file: PathBuf,
    pub local_db_file: PathBuf,
}

/// Canonical on-disk identity. The product was renamed `finance-os` → `phai`;
/// resolution always prefers the new `phai` name and falls back to the legacy
/// `finance-os` name only when the new one is absent, so existing installs keep
/// working with zero data movement. See ADR-0021.
const APP_DIR: &str = "phai";
const LEGACY_APP_DIR: &str = "finance-os";
const DB_FILE: &str = "phai.local.db";
const LEGACY_DB_FILE: &str = "finance-os.local.db";

impl ConfigPaths {
    pub fn discover() -> Result<Self> {
        let config_root = crate::compat::env_var_os("PHAI_CONFIG_DIR", "FINANCE_OS_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(Self::resolve_config_root)
            .context("Não foi possível resolver o diretório de configuração")?;

        let data_root = crate::compat::env_var_os("PHAI_DATA_DIR", "FINANCE_OS_DATA_DIR")
            .map(PathBuf::from)
            .or_else(Self::resolve_data_root)
            .context("Não foi possível resolver o diretório de dados")?;

        Ok(Self {
            config_dir: config_root.clone(),
            data_dir: data_root.clone(),
            config_file: config_root.join("config.toml"),
            local_db_file: Self::resolve_db_file(&data_root),
        })
    }

    /// Resolve the config directory with XDG precedence over platform native,
    /// and the new `phai` name preferred over the legacy `finance-os` name.
    ///
    /// Order:
    /// 1. `$XDG_CONFIG_HOME/phai` if it carries a `config.toml`; then the same
    ///    for the legacy `$XDG_CONFIG_HOME/finance-os` — this keeps existing
    ///    Linux/macOS configs portable and matches the production launcher.
    /// 2. The platform-native directory from `dirs::config_dir()` (on macOS
    ///    that's `~/Library/Application Support/<app>`), preferring `phai` and
    ///    falling back to a pre-existing `finance-os` dir.
    ///
    /// Falling back to native only when XDG has no `config.toml` avoids the
    /// classic macOS trap where a user puts their config under `~/.config`
    /// (the XDG default) but the binary silently reads a stale config from
    /// `~/Library/Application Support`.
    fn resolve_config_root() -> Option<PathBuf> {
        for app in [APP_DIR, LEGACY_APP_DIR] {
            if let Some(xdg) = Self::xdg_config(app) {
                if xdg.join("config.toml").is_file() {
                    return Some(xdg);
                }
            }
        }
        let base = dirs::config_dir()?;
        Some(Self::prefer_existing(
            base.join(APP_DIR),
            base.join(LEGACY_APP_DIR),
        ))
    }

    fn resolve_data_root() -> Option<PathBuf> {
        // Mirror the config resolver: prefer the XDG data dir when it exists
        // (new name first, then legacy), else fall back to the platform-native
        // data dir, preferring `phai` over a pre-existing `finance-os` dir.
        for app in [APP_DIR, LEGACY_APP_DIR] {
            if let Some(xdg) = Self::xdg_data(app) {
                if xdg.exists() {
                    return Some(xdg);
                }
            }
        }
        let base = dirs::data_dir()?;
        Some(Self::prefer_existing(
            base.join(APP_DIR),
            base.join(LEGACY_APP_DIR),
        ))
    }

    /// Resolve the local DB filename inside `data_root`, preferring the new
    /// `phai.local.db` and falling back to a pre-existing `finance-os.local.db`
    /// so existing databases are still found. Fresh installs use the new name.
    fn resolve_db_file(data_root: &Path) -> PathBuf {
        Self::prefer_existing(data_root.join(DB_FILE), data_root.join(LEGACY_DB_FILE))
    }

    /// Pick the legacy path only when the new path is absent and the legacy
    /// one exists; otherwise pick the new path (covers fresh installs).
    fn prefer_existing(new: PathBuf, legacy: PathBuf) -> PathBuf {
        if !new.exists() && legacy.exists() {
            legacy
        } else {
            new
        }
    }

    fn xdg_config(app: &str) -> Option<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|root| root.join(app))
    }

    fn xdg_data(app: &str) -> Option<PathBuf> {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
            })
            .map(|root| root.join(app))
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.config_dir)
            .with_context(|| format!("Falha ao criar {}", self.config_dir.display()))?;
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("Falha ao criar {}", self.data_dir.display()))?;
        Ok(())
    }
}

impl AppConfig {
    pub fn load(paths: &ConfigPaths) -> Result<Self> {
        if !paths.config_file.exists() {
            return Ok(Self::default().with_default_paths(paths));
        }
        let content = fs::read_to_string(&paths.config_file)
            .with_context(|| format!("Falha ao ler {}", paths.config_file.display()))?;
        let config: Self = toml::from_str(&content)
            .with_context(|| format!("Falha ao parsear {}", paths.config_file.display()))?;
        Ok(config.with_default_paths(paths))
    }

    pub fn save(&self, paths: &ConfigPaths) -> Result<()> {
        paths.ensure()?;
        let serialized = toml::to_string_pretty(self).context("Falha ao serializar config")?;
        fs::write(&paths.config_file, &serialized)
            .with_context(|| format!("Falha ao gravar {}", paths.config_file.display()))?;
        #[cfg(unix)]
        {
            let perms = std::fs::Permissions::from_mode(0o600);
            fs::set_permissions(&paths.config_file, perms).with_context(|| {
                format!(
                    "Falha ao definir permissões em {}",
                    paths.config_file.display()
                )
            })?;
        }
        Ok(())
    }

    pub fn with_default_paths(mut self, paths: &ConfigPaths) -> Self {
        // Only auto-assign a local DB path for the Local backend. With
        // BigQuery, the SQLite file is not needed — leave it None unless
        // the user set it explicitly in config.toml.
        if self.local_db_path.is_none() && matches!(self.backend, BackendKind::Local) {
            self.local_db_path = Some(paths.local_db_file.clone());
        }
        self
    }

    pub fn effective_backend(&self) -> BackendKind {
        match self.backend {
            BackendKind::Local => BackendKind::Local,
            BackendKind::Bigquery => {
                if self
                    .service_account_path
                    .as_ref()
                    .is_some_and(|path| path.exists())
                    && self.project_id.as_deref().is_some()
                    && self.dataset_id.as_deref().is_some()
                {
                    BackendKind::Bigquery
                } else {
                    eprintln!(
                        "aviso: backend configurado como bigquery mas credenciais ausentes — usando local como fallback"
                    );
                    BackendKind::Local
                }
            }
        }
    }

    pub fn project_id(&self) -> Result<&str> {
        self.project_id
            .as_deref()
            .context("project_id não configurado")
    }

    pub fn dataset_id(&self) -> Result<&str> {
        self.dataset_id
            .as_deref()
            .context("dataset_id não configurado")
    }

    pub fn service_account_path(&self) -> Result<&Path> {
        self.service_account_path
            .as_deref()
            .context("service_account_path não configurado")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn setup_env(home: &Path, xdg_config: Option<&Path>, xdg_data: Option<&Path>) {
        unsafe {
            std::env::set_var("HOME", home);
            match xdg_config {
                Some(p) => std::env::set_var("XDG_CONFIG_HOME", p),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
            match xdg_data {
                Some(p) => std::env::set_var("XDG_DATA_HOME", p),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
            std::env::remove_var("FINANCE_OS_CONFIG_DIR");
            std::env::remove_var("FINANCE_OS_DATA_DIR");
            std::env::remove_var("PHAI_CONFIG_DIR");
            std::env::remove_var("PHAI_DATA_DIR");
        }
    }

    #[test]
    #[serial]
    fn discover_finds_legacy_finance_os_xdg_config() {
        // Backward compat: an existing `~/.config/finance-os/config.toml`
        // (and no `phai` dir) must still be discovered after the rename.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let xdg = home.join(".config").join("finance-os");
        fs::create_dir_all(&xdg).unwrap();
        fs::write(xdg.join("config.toml"), "backend = \"bigquery\"").unwrap();

        setup_env(home, None, None);
        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.config_dir, xdg);
    }

    #[test]
    #[serial]
    fn discover_prefers_phai_xdg_over_legacy_finance_os() {
        // When both names carry a config.toml, the new `phai` dir wins.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let phai = home.join(".config").join("phai");
        let legacy = home.join(".config").join("finance-os");
        fs::create_dir_all(&phai).unwrap();
        fs::create_dir_all(&legacy).unwrap();
        fs::write(phai.join("config.toml"), "backend = \"local\"").unwrap();
        fs::write(legacy.join("config.toml"), "backend = \"bigquery\"").unwrap();

        setup_env(home, None, None);
        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.config_dir, phai);
    }

    #[test]
    #[serial]
    fn discover_falls_back_to_native_when_xdg_has_no_config() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        setup_env(home, None, None);

        let paths = ConfigPaths::discover().unwrap();
        // The fallback must come from `dirs::config_dir()`. On Linux that's
        // `$XDG_CONFIG_HOME/finance-os` (so it coincides with the XDG path
        // we'd otherwise prefer); on macOS it's
        // `~/Library/Application Support/finance-os`. Either way the path
        // must be under HOME — that's all this test can portably assert.
        assert!(
            paths.config_dir.starts_with(home),
            "expected fallback under HOME, got {:?}",
            paths.config_dir
        );
        // And critically: on macOS specifically, fallback must NOT equal
        // the XDG path, because that's the whole reason for the resolver.
        #[cfg(target_os = "macos")]
        {
            let xdg_path = home.join(".config").join("finance-os");
            assert_ne!(paths.config_dir, xdg_path);
        }
    }

    #[test]
    #[serial]
    fn discover_honours_explicit_env_var_override() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let custom = home.join("custom-cfg");
        fs::create_dir_all(&custom).unwrap();
        setup_env(home, None, None);
        unsafe {
            std::env::set_var("FINANCE_OS_CONFIG_DIR", &custom);
        }

        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.config_dir, custom);

        unsafe {
            std::env::remove_var("FINANCE_OS_CONFIG_DIR");
        }
    }

    #[test]
    #[serial]
    fn discover_prefers_phai_env_over_legacy_env() {
        // PHAI_CONFIG_DIR wins over FINANCE_OS_CONFIG_DIR.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let phai_dir = home.join("phai-cfg");
        let legacy_dir = home.join("legacy-cfg");
        fs::create_dir_all(&phai_dir).unwrap();
        fs::create_dir_all(&legacy_dir).unwrap();
        setup_env(home, None, None);
        unsafe {
            std::env::set_var("PHAI_CONFIG_DIR", &phai_dir);
            std::env::set_var("FINANCE_OS_CONFIG_DIR", &legacy_dir);
        }

        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.config_dir, phai_dir);

        unsafe {
            std::env::remove_var("PHAI_CONFIG_DIR");
            std::env::remove_var("FINANCE_OS_CONFIG_DIR");
        }
    }

    #[test]
    #[serial]
    fn discover_uses_legacy_db_file_when_present() {
        // An existing `finance-os.local.db` in the data dir must keep being
        // used; the new `phai.local.db` name only applies to fresh data dirs.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let xdg_data = home.join("xdg-data");
        let legacy_data = xdg_data.join("finance-os");
        fs::create_dir_all(&legacy_data).unwrap();
        fs::write(legacy_data.join("finance-os.local.db"), b"db").unwrap();

        setup_env(home, None, Some(&xdg_data));
        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(paths.data_dir, legacy_data);
        assert_eq!(paths.local_db_file, legacy_data.join("finance-os.local.db"));
    }

    #[test]
    #[serial]
    fn discover_uses_phai_db_file_for_fresh_install() {
        // No pre-existing data dir or db: everything resolves to the new name.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let xdg_data = home.join("xdg-data");
        setup_env(home, None, Some(&xdg_data));

        let paths = ConfigPaths::discover().unwrap();
        assert_eq!(
            paths.local_db_file.file_name().unwrap().to_str().unwrap(),
            "phai.local.db"
        );
    }
}
