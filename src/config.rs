use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NoAssertionsConfig {
    pub enabled: bool,
}

impl Default for NoAssertionsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DuplicateAssertConfig {
    pub enabled: bool,
}

impl Default for DuplicateAssertConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TautologicalAssertConfig {
    pub enabled: bool,
}

impl Default for TautologicalAssertConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Glob patterns (relative to repo root) to skip entirely.
    pub exclude: Vec<String>,
    #[serde(rename = "no_assertions")]
    pub no_assertions: NoAssertionsConfig,
    #[serde(rename = "duplicate_assert")]
    pub duplicate_assert: DuplicateAssertConfig,
    #[serde(rename = "tautological_assert")]
    pub tautological_assert: TautologicalAssertConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    dupdup: Config,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)?;
        let raw: RawConfig = toml::from_str(&text)?;
        Ok(raw.dupdup)
    }
}
