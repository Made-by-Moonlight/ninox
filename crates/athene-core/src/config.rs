use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeVariant {
    Light,
    #[default]
    Dark,
    Athene,
}

// ---------------------------------------------------------------------------
// Agent configuration
// ---------------------------------------------------------------------------

/// Which agent harness and model to use for a session type.
///
/// Example `~/.config/athene/config.toml`:
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

impl AgentConfig {
    /// Interactive launch command for an orchestrator session.
    pub fn interactive_cmd(&self) -> String {
        let binary = harness_binary(&self.harness);
        match &self.model {
            Some(m) => format!("{binary} --model {m}"),
            None    => binary.to_string(),
        }
    }

    /// Launch command for a worker session.
    pub fn worker_cmd(&self, prompt: &str) -> String {
        let binary = harness_binary(&self.harness);
        let quoted = shell_quote(prompt);
        match self.harness.as_str() {
            "claude-code" => {
                // Interactive mode with positional prompt: the full agent TUI is
                // visible in the terminal and the agent runs autonomously.
                // --dangerously-skip-permissions allows tool calls without approval.
                let model_part = self.model.as_deref()
                    .map(|m| format!(" --model {}", shell_quote(m)))
                    .unwrap_or_default();
                format!("{binary} --dangerously-skip-permissions{model_part} -- {quoted}")
            }
            "aider" => {
                let model_part = self.model.as_deref()
                    .map(|m| format!(" --model {}", shell_quote(m)))
                    .unwrap_or_default();
                format!("{binary}{model_part} --message {quoted}")
            }
            _ => {
                let model_part = self.model.as_deref()
                    .map(|m| format!(" --model {}", shell_quote(m)))
                    .unwrap_or_default();
                format!("{binary}{model_part} -p {quoted}")
            }
        }
    }
}

fn harness_binary(harness: &str) -> &str {
    match harness {
        "claude-code" => "claude",
        "codex"       => "codex",
        "aider"       => "aider",
        "opencode"    => "opencode",
        other         => other,
    }
}


fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Brain configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrainConfig {
    pub path: Option<PathBuf>,
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
    /// Defaults to `~/.config/athene/orchestrator`.
    #[serde(default)]
    pub orchestrator_root: Option<PathBuf>,
    /// Agent harness and model for orchestrator sessions.
    #[serde(default)]
    pub orchestrator: AgentConfig,
    /// Agent harness and model for worker sessions spawned by `athene spawn`.
    #[serde(default)]
    pub worker: AgentConfig,
    /// Knowledge base (brain) configuration.
    #[serde(default)]
    pub brain: BrainConfig,
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
            brain:            BrainConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn resolved_brain_path(&self) -> PathBuf {
        if let Some(ref p) = self.brain.path {
            return p.clone();
        }
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("athene")
            .join("brain")
    }

    pub fn resolved_orchestrator_root(&self) -> PathBuf {
        self.orchestrator_root.clone().unwrap_or_else(|| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("athene")
                .join("orchestrator")
        })
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("athene")
            .join("config.toml")
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

    #[test]
    fn interactive_cmd_with_model() {
        let cfg = AgentConfig { harness: "claude-code".into(), model: Some("claude-opus-4-5".into()) };
        assert_eq!(cfg.interactive_cmd(), "claude --model claude-opus-4-5");
    }

    #[test]
    fn worker_cmd_codex() {
        let cfg = AgentConfig { harness: "codex".into(), model: Some("gpt-4o".into()) };
        assert_eq!(cfg.worker_cmd("do the thing"), "codex --model 'gpt-4o' -p 'do the thing'");
    }

    #[test]
    fn worker_cmd_claude_code() {
        let cfg = AgentConfig { harness: "claude-code".into(), model: None };
        assert_eq!(cfg.worker_cmd("Fix the bug"), "claude --dangerously-skip-permissions -- 'Fix the bug'");
    }

    #[test]
    fn worker_cmd_claude_code_with_model() {
        let cfg = AgentConfig { harness: "claude-code".into(), model: Some("claude-opus-4-5".into()) };
        assert_eq!(cfg.worker_cmd("do task"), "claude --dangerously-skip-permissions --model 'claude-opus-4-5' -- 'do task'");
    }

    #[test]
    fn resolved_orchestrator_root_default() {
        let cfg = AppConfig::default();
        assert!(cfg.resolved_orchestrator_root().ends_with("athene/orchestrator"));
    }
}
