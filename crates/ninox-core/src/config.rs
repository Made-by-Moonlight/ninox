use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::PathBuf};

use crate::harness::{HarnessRegistry, HarnessSpec};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeVariant {
    Light,
    #[default]
    Dark,
    Ninox,
}

// ---------------------------------------------------------------------------
// Agent configuration
// ---------------------------------------------------------------------------

/// Which agent harness and model to use for a session type.
///
/// Example `~/.config/ninox/config.toml`:
/// ```toml
/// [orchestrator]
/// harness = "claude-code"
/// model = "claude-opus-4-5"
///
/// [worker]
/// harness = "codex"
/// model = "gpt-4o"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent harness: `"claude-code"`, `"codex"`, `"aider"`, or `"opencode"`.
    #[serde(default = "default_harness")]
    pub harness: String,
    /// Model identifier passed to the harness CLI.
    /// Omit to use the harness default.
    pub model: Option<String>,
}

fn default_harness() -> String {
    "claude-code".to_string()
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self { harness: default_harness(), model: None }
    }
}

// Launch-command construction lives in `crate::harness` — `AgentConfig` is
// only the per-role/per-spawn pointer (harness name + model) into the
// registry; resolve via `AppConfig::registry().interactive_cmd/worker_cmd`.

// ---------------------------------------------------------------------------
// Brain configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    pub path: Option<PathBuf>,
    /// Additional named knowledge bases selectable when spawning an
    /// orchestrator. The implicit "default" catalogue (this config's
    /// `resolved_brain_path()`) is always offered first by
    /// `AppConfig::catalogue_options()` and is not duplicated even if an
    /// entry here is also named "default".
    #[serde(default)]
    pub catalogues: Vec<CatalogueRef>,
}

/// A named, selectable knowledge-base catalogue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CatalogueRef {
    pub name: String,
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// App configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port:      u16,
    pub font_size: f32,
    #[serde(default)]
    pub theme:     ThemeVariant,
    /// Override for the orchestrator root directory.
    /// Defaults to `~/.config/ninox/orchestrator`.
    #[serde(default)]
    pub orchestrator_root: Option<PathBuf>,
    /// Agent harness and model for orchestrator sessions.
    #[serde(default)]
    pub orchestrator: AgentConfig,
    /// Agent harness and model for worker sessions spawned by `ninox spawn`.
    #[serde(default)]
    pub worker: AgentConfig,
    /// GitHub personal access token. If absent, falls back to GITHUB_TOKEN env var.
    /// Requires `repo` scope for private repos, `public_repo` for public.
    #[serde(default)]
    pub github_token: Option<String>,
    /// Knowledge base (brain) configuration.
    #[serde(default)]
    pub brain: BrainConfig,
    /// Theme file name (resolves to `~/.config/ninox/themes/<name>.toml`) or
    /// an absolute/`~`-relative path. `None` uses `themes/field-notes.toml`
    /// if present, else the built-in Field Notes palettes.
    #[serde(default)]
    pub theme_file: Option<String>,
    /// Agent-harness registry overrides/extensions (`[harnesses.<name>]`).
    /// Builtin specs for claude-code/codex/opencode/aider/freebuff apply
    /// when a name is absent here. See `crate::harness`. Kept last so TOML
    /// serialization emits this table-of-tables after every scalar field.
    #[serde(default)]
    pub harnesses: BTreeMap<String, HarnessSpec>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            port:             8080,
            font_size:        13.0,
            theme:            ThemeVariant::Dark,
            orchestrator_root: None,
            orchestrator:     AgentConfig::default(),
            worker:           AgentConfig::default(),
            github_token:     None,
            brain:            BrainConfig::default(),
            theme_file:       None,
            harnesses:        BTreeMap::new(),
        }
    }
}

impl AppConfig {
    /// The effective harness registry: builtin specs overlaid by this
    /// config's `[harnesses.*]` entries.
    pub fn registry(&self) -> HarnessRegistry {
        HarnessRegistry::from_config(&self.harnesses)
    }

    /// Path to the knowledge-base (brain) directory.
    ///
    /// Honors the `NINOX_BRAIN` environment variable as an override: if
    /// set, it is treated as an absolute path to the brain directory and
    /// returned as-is, mirroring how `config_path()` honors `NINOX_CONFIG`.
    /// This lets a selected catalogue (see `catalogue_options()`) be handed
    /// to a spawned orchestrator session via its environment without
    /// mutating `config.toml`, and lets tests redirect brain reads/writes
    /// without touching the real user brain directory.
    ///
    /// Falls back to `self.brain.path` when set, else `<config_dir>/ninox/brain`.
    pub fn resolved_brain_path(&self) -> PathBuf {
        if let Ok(p) = std::env::var("NINOX_BRAIN") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Some(ref p) = self.brain.path {
            return p.clone();
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("brain")
    }

    /// All selectable knowledge-base catalogues: the implicit "default"
    /// (this config's `resolved_brain_path()`) followed by any additional
    /// catalogues configured under `[[brain.catalogues]]` — skipping any
    /// entry literally named "default" to avoid a confusing duplicate.
    pub fn catalogue_options(&self) -> Vec<CatalogueRef> {
        let mut options = vec![CatalogueRef {
            name: "default".to_string(),
            path: self.resolved_brain_path(),
        }];
        options.extend(
            self.brain
                .catalogues
                .iter()
                .filter(|c| c.name != "default")
                .cloned(),
        );
        options
    }

    pub fn resolved_orchestrator_root(&self) -> PathBuf {
        self.orchestrator_root.clone().unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("ninox")
                .join("orchestrator")
        })
    }

    /// Path to the `config.toml` file.
    ///
    /// Honors the `NINOX_CONFIG` environment variable as an override: if
    /// set, it is treated as an absolute path to the config file itself
    /// (not a directory) and returned as-is. This is the same override
    /// consumed by spawned agent sessions (see the `NINOX_CONFIG` env var
    /// set alongside `NINOX_BIN` when launching orchestrator sessions), and
    /// it also lets tests redirect config reads/writes away from the real
    /// user config file (e.g. `~/Library/Application Support/ninox/config.toml`
    /// on macOS) without mutating developer machine state.
    ///
    /// Falls back to `<config_dir>/ninox/config.toml` when unset.
    pub fn config_path() -> PathBuf {
        if let Ok(p) = std::env::var("NINOX_CONFIG") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("config.toml")
    }

    /// Directory for Ninox-managed shell wrappers prepended to agent PATH.
    /// Default: `~/.config/ninox/bin/`
    pub fn ninox_bin_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("bin")
    }

    /// Directory where per-session metadata JSON files are written by wrapper hooks.
    /// Default: `~/.config/ninox/sessions/`
    pub fn sessions_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ninox")
            .join("sessions")
    }

    fn path() -> PathBuf { Self::config_path() }

    pub fn load() -> Result<Self> {
        let p = Self::path();
        if !p.exists() { return Ok(Self::default()); }
        Ok(toml::from_str(&fs::read_to_string(p)?)?)
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path();
        fs::create_dir_all(p.parent().unwrap())?;
        fs::write(p, toml::to_string(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig { port: 9090, font_size: 14.0, theme: ThemeVariant::Light, ..AppConfig::default() };
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.port, 9090);
        assert_eq!(loaded.theme, ThemeVariant::Light);
        assert!(loaded.orchestrator_root.is_none());
    }

    #[test]
    fn default_theme_is_dark() {
        assert_eq!(AppConfig::default().theme, ThemeVariant::Dark);
    }

    #[test]
    fn missing_theme_field_defaults_to_dark() {
        let cfg: AppConfig = toml::from_str("port = 8080\nfont_size = 13.0\n").unwrap();
        assert_eq!(cfg.theme, ThemeVariant::Dark);
    }

    #[test]
    fn agent_config_round_trip() {
        let toml = "port = 8080\nfont_size = 13.0\n\n[orchestrator]\nharness = \"claude-code\"\nmodel = \"claude-opus-4-5\"\n\n[worker]\nharness = \"codex\"\n";
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.orchestrator.harness, "claude-code");
        assert_eq!(cfg.orchestrator.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(cfg.worker.harness, "codex");
        assert!(cfg.worker.model.is_none());
    }

    // Launch-shape tests for the four known harnesses moved to
    // `crate::harness::tests` with the registry.

    #[test]
    fn resolved_orchestrator_root_default() {
        let cfg = AppConfig::default();
        assert!(cfg.resolved_orchestrator_root().ends_with("ninox/orchestrator"));
    }

    /// Serializes tests that mutate process-global env vars (`NINOX_CONFIG`,
    /// `NINOX_BRAIN`) against each other — `cargo test` runs test fns on
    /// parallel threads, so without this guard one test's env mutation could
    /// leak into another's read.
    static ENV_TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Set `key=value` for the duration of `f`, restoring the prior value
    /// (or unsetting it) afterward. Serialized via `ENV_TEST_GUARD` since env
    /// vars are process-global state shared across parallel test threads.
    /// Mirrors `ninox_app::app::tests::with_env_override`.
    fn with_env_override<T>(
        key: &str,
        value: impl AsRef<std::ffi::OsStr>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        match prior {
            Some(v) => std::env::set_var(key, v),
            None    => std::env::remove_var(key),
        }
        result.unwrap()
    }

    #[test]
    fn config_path_honors_ninox_config_env() {
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("config_path_honors_ninox_config_env.toml");

        with_env_override("NINOX_CONFIG", &override_path, || {
            assert_eq!(AppConfig::config_path(), override_path);
        });
    }

    #[test]
    fn resolved_brain_path_honors_ninox_brain_env() {
        let dir = tempdir().unwrap();
        let override_path = dir.path().join("brain-override");

        with_env_override("NINOX_BRAIN", &override_path, || {
            let cfg = AppConfig::default();
            assert_eq!(cfg.resolved_brain_path(), override_path);
        });
    }

    #[test]
    fn catalogue_options_defaults_to_single_entry() {
        // Serialize against resolved_brain_path_honors_ninox_brain_env: this
        // test reads resolved_brain_path() twice (via catalogue_options and
        // directly) and must not straddle that test's NINOX_BRAIN window.
        let _guard = ENV_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let cfg = AppConfig::default();
        let options = cfg.catalogue_options();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].name, "default");
        assert_eq!(options[0].path, cfg.resolved_brain_path());
    }

    #[test]
    fn catalogue_options_appends_configured_catalogues_and_skips_duplicate_default() {
        let mut cfg = AppConfig::default();
        cfg.brain.catalogues = vec![
            CatalogueRef { name: "docs".to_string(), path: PathBuf::from("/tmp/docs-brain") },
            CatalogueRef { name: "default".to_string(), path: PathBuf::from("/tmp/should-be-skipped") },
        ];
        let options = cfg.catalogue_options();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].name, "default");
        assert_eq!(options[1].name, "docs");
        assert_eq!(options[1].path, PathBuf::from("/tmp/docs-brain"));
    }
}
