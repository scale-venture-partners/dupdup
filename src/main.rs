use clap::{Parser, Subcommand};
use dupdup::config::Config;
use dupdup::static_lint::rules::Severity;
use dupdup::{dynamic, git, output, redundancy, static_lint};
use output::AnalyzeReport;
use regex::Regex;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "dupdup",
    version,
    about = "Finds duplicative, redundant, and useless tests"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Static AST rules over test source — no execution, pre-commit friendly
    Check {
        /// Explicit files to check (bypasses git discovery entirely)
        paths: Vec<PathBuf>,
        /// Check files staged for commit
        #[arg(long)]
        staged: bool,
        /// Check every git-tracked file in the repo
        #[arg(long)]
        all: bool,
        /// Emit machine-readable JSON instead of human-readable text
        #[arg(long)]
        json: bool,
        /// Path to config file
        #[arg(long, default_value = "dupdup.toml")]
        config: PathBuf,
    },
    /// Redundancy analysis over a coverage run and/or a mutation kill-matrix.
    /// Requires executing the test suite first — this is not a linter.
    Analyze {
        /// Output of `coverage json --show-contexts` after a
        /// `pytest --cov=<pkg> --cov-context=test` run
        #[arg(long)]
        coverage_json: Option<PathBuf>,
        /// pytest `--junitxml` output from the same run as --coverage-json.
        /// Cross-checked against the test count coverage-json actually
        /// recorded contexts for, so a run where Python's `sysmon` coverage
        /// core silently dropped contexts fails loudly instead of analyzing
        /// partial data.
        #[arg(long)]
        junit_xml: Option<PathBuf>,
        /// Test-to-killed-mutant JSON (see `dupdup::dynamic::killmatrix` for
        /// the schema). No upstream tool produces this yet.
        #[arg(long)]
        kill_matrix_json: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Write a default dupdup.toml to the current directory
    Init,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Check {
            paths,
            staged,
            all,
            json,
            config,
        } => run_check(&paths, staged, all, json, &config),
        Commands::Analyze {
            coverage_json,
            junit_xml,
            kill_matrix_json,
            json,
        } => run_analyze(
            coverage_json.as_deref(),
            junit_xml.as_deref(),
            kill_matrix_json.as_deref(),
            json,
        ),
        Commands::Init => run_init(),
    }
}

const DEFAULT_CONFIG_TEMPLATE: &str = r"[dupdup]
# Glob patterns (relative to repo root) to skip entirely.
exclude = []

[dupdup.no_assertions]
enabled = true

[dupdup.duplicate_assert]
enabled = true

[dupdup.tautological_assert]
enabled = true
";

fn run_init() -> anyhow::Result<()> {
    let path = PathBuf::from("dupdup.toml");
    if path.exists() {
        anyhow::bail!("dupdup.toml already exists");
    }
    std::fs::write(&path, DEFAULT_CONFIG_TEMPLATE)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn is_python_test_file(path: &Path) -> bool {
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => {
            name.ends_with(".py") && (name.starts_with("test_") || name.ends_with("_test.py"))
        }
        None => false,
    }
}

fn glob_to_regex(glob: &str) -> String {
    let mut out = String::from("^");
    for c in glob.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            c if r"\.+()|[]{}^$".contains(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    out
}

fn compile_globs(patterns: &[String]) -> anyhow::Result<Vec<Regex>> {
    patterns
        .iter()
        .map(|p| Regex::new(&glob_to_regex(p)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::from)
}

fn is_excluded(path: &Path, patterns: &[Regex]) -> bool {
    let path_str = path.to_string_lossy();
    patterns.iter().any(|re| re.is_match(&path_str))
}

fn run_check(
    paths: &[PathBuf],
    staged: bool,
    all: bool,
    json: bool,
    config_path: &Path,
) -> anyhow::Result<()> {
    let modes = [!paths.is_empty(), staged, all]
        .iter()
        .filter(|b| **b)
        .count();
    if modes == 0 {
        anyhow::bail!("specify one of: explicit paths, --staged, or --all");
    }
    if modes > 1 {
        anyhow::bail!("--staged, --all, and explicit paths are mutually exclusive");
    }

    let config = Config::load(config_path)?;
    let exclude_patterns = compile_globs(&config.exclude)?;
    let mut violations = Vec::new();

    if paths.is_empty() {
        let cwd = std::env::current_dir()?;
        let root = git::repo_root(&cwd)?;
        let files = if staged {
            git::staged_files(&root)?
        } else {
            git::all_files(&root)?
        };
        for rel_path in files {
            if !is_python_test_file(&rel_path) || is_excluded(&rel_path, &exclude_patterns) {
                continue;
            }
            let source = if staged {
                match git::staged_content(&root, &rel_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            } else {
                match std::fs::read_to_string(root.join(&rel_path)) {
                    Ok(s) => s,
                    Err(_) => continue,
                }
            };
            violations.extend(static_lint::check_file(
                &rel_path.to_string_lossy(),
                &source,
                &config,
            )?);
        }
    } else {
        // An explicit path is checked regardless of pytest's test-file naming
        // convention — the caller named it, so it's in scope. That
        // convention only gates which files --staged/--all bother to parse.
        for path in paths {
            if is_excluded(path, &exclude_patterns) {
                continue;
            }
            let source = std::fs::read_to_string(path)?;
            violations.extend(static_lint::check_file(
                &path.to_string_lossy(),
                &source,
                &config,
            )?);
        }
    }

    if json {
        output::print_check_json(&violations)?;
    } else {
        output::print_check_human(&violations);
    }

    if violations.iter().any(|v| v.severity == Severity::Error) {
        std::process::exit(1);
    }
    Ok(())
}

/// A run under Python 3.12+'s `sysmon` coverage core silently drops dynamic
/// contexts for some tests rather than erroring. `--junit-xml` gives the
/// count of tests that actually executed their body (passed + failed,
/// excluding skipped/errored) so a shortfall against the coverage-json
/// context count is caught here instead of analyzing incomplete data.
fn check_context_completeness(found_test_count: usize, junit_path: &Path) -> anyhow::Result<()> {
    let summary = dynamic::junit::load(junit_path)?;
    if found_test_count < summary.ran {
        anyhow::bail!(
            "coverage-json has dynamic-context data for {found_test_count} test(s), but \
             junit-xml recorded {ran} test(s) that ran ({skipped} skipped, {errored} errored). \
             {missing} test(s) are missing context data — most likely the run used Python's \
             `sysmon` coverage core, which drops dynamic contexts silently. Re-run with \
             `COVERAGE_CORE=ctrace` (or `core = ctrace` in .coveragerc).",
            ran = summary.ran,
            skipped = summary.skipped,
            errored = summary.errored,
            missing = summary.ran.saturating_sub(found_test_count),
        );
    }
    Ok(())
}

fn run_analyze(
    coverage_json: Option<&Path>,
    junit_xml: Option<&Path>,
    kill_matrix_json: Option<&Path>,
    json: bool,
) -> anyhow::Result<()> {
    let mut reports = Vec::new();

    if let Some(cov_path) = coverage_json {
        let loaded = dynamic::coverage::load(cov_path)?;
        match junit_xml {
            Some(junit_path) => check_context_completeness(loaded.tests.len(), junit_path)?,
            None => eprintln!(
                "warning: no --junit-xml given — a run with silently-dropped dynamic contexts \
                 (e.g. Python's sysmon coverage core) can't be detected. Pass --junit-xml to enable that check."
            ),
        }
        reports.push(AnalyzeReport {
            source: "coverage",
            findings: redundancy::analyze(&loaded.tests),
        });
    }

    if let Some(km_path) = kill_matrix_json {
        let loaded = dynamic::killmatrix::load(km_path)?;
        reports.push(AnalyzeReport {
            source: "mutation",
            findings: redundancy::analyze(&loaded.tests),
        });
    }

    if reports.is_empty() {
        anyhow::bail!("nothing to analyze: pass --coverage-json and/or --kill-matrix-json");
    }

    if json {
        output::print_analyze_json(&reports)?;
    } else {
        output::print_analyze_human(&reports);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_python_test_file_matches_pytest_naming_convention() {
        assert!(is_python_test_file(Path::new("test_foo.py")));
        assert!(is_python_test_file(Path::new("foo_test.py")));
        assert!(is_python_test_file(Path::new("dir/test_foo.py")));
        assert!(!is_python_test_file(Path::new("foo.py")));
        assert!(!is_python_test_file(Path::new("test_foo.txt")));
        assert!(!is_python_test_file(Path::new("conftest.py")));
    }

    #[test]
    fn glob_to_regex_translates_star_and_escapes_dots() {
        assert_eq!(glob_to_regex("vendor/*"), r"^vendor/.*$");
        assert_eq!(glob_to_regex("a.b"), r"^a\.b$");
    }

    #[test]
    fn is_excluded_matches_a_compiled_glob() {
        let patterns = compile_globs(&["vendor/*".to_string()]).unwrap();
        assert!(is_excluded(Path::new("vendor/thing.py"), &patterns));
        assert!(!is_excluded(Path::new("src/thing.py"), &patterns));
    }

    #[test]
    fn is_excluded_with_no_patterns_excludes_nothing() {
        let patterns = compile_globs(&[]).unwrap();
        assert!(!is_excluded(Path::new("anything.py"), &patterns));
    }
}
