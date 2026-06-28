use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub port:      u16,
    pub font_size: f32,
}

impl Default for AppConfig {
    fn default() -> Self { Self { port: 8080, font_size: 13.0 } }
}

impl AppConfig {
    fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("athene").join("config.toml")
    }

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
        let cfg = AppConfig { port: 9090, font_size: 14.0 };
        fs::write(&path, toml::to_string(&cfg).unwrap()).unwrap();
        let loaded: AppConfig = toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.port, 9090);
    }
}
