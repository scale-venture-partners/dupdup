//! `dupdup.toml` config: which static rules run, and which paths to skip.

use serde::Deserialize;
use std::path::Path;

/// Settings for the `NO_ASSERTIONS` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NoAssertionsConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
    /// Extra function names to treat as asserting helpers, for helpers the
    /// static index can't see (defined outside the repo, or generated).
    pub helpers: Vec<String>,
}

impl Default for NoAssertionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            helpers: Vec::new(),
        }
    }
}

/// Settings for the `NO_ASSERTIONS_SMOKE` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NoAssertionsSmokeConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
}

impl Default for NoAssertionsSmokeConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Settings for the `DUPLICATE_ASSERT` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DuplicateAssertConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
}

impl Default for DuplicateAssertConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Settings for the `TAUTOLOGICAL_ASSERT` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TautologicalAssertConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
}

impl Default for TautologicalAssertConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Settings for the `CLONE_TEST` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CloneTestConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
}

impl Default for CloneTestConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Settings for the `WEAK_ASSERTIONS` rule.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct WeakAssertionsConfig {
    /// Whether this rule runs at all.
    pub enabled: bool,
}

impl Default for WeakAssertionsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// The full `[dupdup]` table of `dupdup.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Glob patterns (relative to repo root) to skip entirely.
    pub exclude: Vec<String>,
    /// `NO_ASSERTIONS` rule settings.
    #[serde(rename = "no_assertions")]
    pub no_assertions: NoAssertionsConfig,
    /// `NO_ASSERTIONS_SMOKE` rule settings.
    #[serde(rename = "no_assertions_smoke")]
    pub no_assertions_smoke: NoAssertionsSmokeConfig,
    /// `DUPLICATE_ASSERT` rule settings.
    #[serde(rename = "duplicate_assert")]
    pub duplicate_assert: DuplicateAssertConfig,
    /// `TAUTOLOGICAL_ASSERT` rule settings.
    #[serde(rename = "tautological_assert")]
    pub tautological_assert: TautologicalAssertConfig,
    /// `WEAK_ASSERTIONS` rule settings.
    #[serde(rename = "weak_assertions")]
    pub weak_assertions: WeakAssertionsConfig,
    /// `CLONE_TEST` rule settings.
    #[serde(rename = "clone_test")]
    pub clone_test: CloneTestConfig,
}

#[derive(Debug, Default, Deserialize)]
struct RawConfig {
    #[serde(default)]
    dupdup: Config,
}

impl Config {
    /// Loads `dupdup.toml` from `path`, or the all-enabled default if it
    /// doesn't exist.
    ///
    /// # Errors
    /// Returns an error if `path` exists but isn't valid TOML.
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
