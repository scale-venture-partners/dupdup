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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_returns_all_rules_enabled_by_default() {
        let config = Config::load(Path::new("/nonexistent/dupdup.toml")).unwrap();
        assert!(config.no_assertions.enabled);
        assert!(config.duplicate_assert.enabled);
        assert!(config.tautological_assert.enabled);
        assert!(config.exclude.is_empty());
    }

    #[test]
    fn a_rule_can_be_disabled_without_specifying_the_others() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"[dupdup.no_assertions]\nenabled = false\n").unwrap();
        let config = Config::load(f.path()).unwrap();
        assert!(!config.no_assertions.enabled);
        assert!(config.duplicate_assert.enabled);
        assert!(config.tautological_assert.enabled);
    }

    #[test]
    fn exclude_patterns_load_from_the_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"[dupdup]\nexclude = [\"vendor/*\"]\n").unwrap();
        let config = Config::load(f.path()).unwrap();
        assert_eq!(config.exclude, vec!["vendor/*"]);
    }

    #[test]
    fn malformed_toml_is_an_error_not_a_silent_default() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut f, b"not valid toml [[[").unwrap();
        assert!(Config::load(f.path()).is_err());
    }
}
