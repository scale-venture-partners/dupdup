//! Static, tree-sitter-based rules over Python test source (`dupdup check`).
//! No test execution — this module only parses and walks the AST.

/// The individual rule checks (`NO_ASSERTIONS`, `DUPLICATE_ASSERT`,
/// `TAUTOLOGICAL_ASSERT`) plus the `Violation`/`Severity` types they emit.
pub mod rules;

/// The index of function names that assert, used to see through helpers.
pub mod helpers;

pub use helpers::AssertingHelpers;

use crate::config::Config;
use rules::Violation;
use tree_sitter::{Node, Parser, Tree};

/// Parses `source` as Python.
///
/// # Errors
/// Returns an error if the tree-sitter-python grammar can't be loaded.
pub fn parse(source: &str) -> anyhow::Result<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&tree_sitter_python::LANGUAGE.into())?;
    parser
        .parse(source, None)
        .ok_or_else(|| anyhow::anyhow!("failed to parse Python source"))
}

/// Pytest's default discovery convention (`python_functions = test_*`).
/// Methods on unittest-style `Test*` classes are picked up too. A function
/// nested inside another function is never itself discovered by pytest
/// regardless of its name — e.g. a fixture's inner route handler that
/// happens to be called `test_endpoint` — so this doesn't descend into a
/// function body once it's found one, matched or not.
#[must_use]
pub fn find_test_functions<'a>(root: Node<'a>, source: &'a [u8]) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    collect_test_functions(root, source, &mut out);
    out
}

fn collect_test_functions<'a>(node: Node<'a>, source: &'a [u8], out: &mut Vec<Node<'a>>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_definition" {
            if let Some(name_node) = child.child_by_field_name("name") {
                if name_node
                    .utf8_text(source)
                    .map(|n| n.starts_with("test_"))
                    .unwrap_or(false)
                {
                    out.push(child);
                }
            }
            continue;
        }
        collect_test_functions(child, source, out);
    }
}

/// The name of the `def` node `fn_node` points at.
#[must_use]
pub fn test_name<'a>(fn_node: Node<'a>, source: &'a [u8]) -> &'a str {
    fn_node
        .child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .unwrap_or("<unknown>")
}

/// A `# dupdup: ignore[RULE]` (or bare `# dupdup: ignore`) comment on the
/// violation's own line or the line above suppresses it — the same
/// placement windbag uses, so the two tools stay muscle-memory compatible.
fn is_suppressed(lines: &[&str], line_1indexed: usize, rule: &str) -> bool {
    let check = |text: &str| {
        text.contains("dupdup: ignore")
            && (!text.contains('[') || text.contains(&format!("[{rule}]")))
    };
    let idx = line_1indexed.saturating_sub(1);
    lines.get(idx).is_some_and(|l| check(l))
        || idx
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .is_some_and(|l| check(l))
}

/// [`check_file_with_helpers`] with an index built from `source` alone, so
/// a helper defined in the same file is seen through but one in another
/// file isn't.
///
/// # Errors
/// Returns an error if `source` can't be parsed as Python.
pub fn check_file(path: &str, source: &str, config: &Config) -> anyhow::Result<Vec<Violation>> {
    let helpers = AssertingHelpers::build([source], &config.no_assertions.helpers);
    check_file_with_helpers(path, source, config, &helpers)
}

/// Runs every enabled static rule over each `test_*` function in `source`,
/// tags each violation with `path`, and drops any suppressed by a
/// `# dupdup: ignore[RULE]` comment. `helpers` says which called functions
/// count as assertions.
///
/// # Errors
/// Returns an error if `source` can't be parsed as Python.
pub fn check_file_with_helpers(
    path: &str,
    source: &str,
    config: &Config,
    helpers: &AssertingHelpers,
) -> anyhow::Result<Vec<Violation>> {
    let tree = parse(source)?;
    let source_bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();

    for fn_node in find_test_functions(tree.root_node(), source_bytes) {
        let name = test_name(fn_node, source_bytes).to_string();
        let mut found = Vec::new();

        found.extend(
            rules::check_no_assertions(fn_node, source_bytes, helpers).filter(|v| {
                if v.rule == "NO_ASSERTIONS" {
                    config.no_assertions.enabled
                } else {
                    config.no_assertions_smoke.enabled
                }
            }),
        );
        if config.duplicate_assert.enabled {
            found.extend(rules::check_duplicate_assert(fn_node, source_bytes));
        }
        if config.tautological_assert.enabled {
            found.extend(rules::check_tautological_assert(fn_node, source_bytes));
        }

        for v in &mut found {
            v.file = path.to_string();
            v.test_name.clone_from(&name);
        }
        out.extend(found);
    }

    // `NO_ASSERTIONS_SMOKE` split off `NO_ASSERTIONS`, so an existing
    // `ignore[NO_ASSERTIONS]` keeps suppressing it.
    out.retain(|v| {
        let smoke = v.rule == "NO_ASSERTIONS_SMOKE";
        !(is_suppressed(&lines, v.line, v.rule)
            || (smoke && is_suppressed(&lines, v.line, "NO_ASSERTIONS")))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_suppressed_matches_bracketed_rule_on_the_same_line() {
        let lines = ["def test_x():  # dupdup: ignore[NO_ASSERTIONS]"];
        assert!(is_suppressed(&lines, 1, "NO_ASSERTIONS"));
    }

    #[test]
    fn is_suppressed_does_not_match_a_different_bracketed_rule() {
        let lines = ["def test_x():  # dupdup: ignore[DUPLICATE_ASSERT]"];
        assert!(!is_suppressed(&lines, 1, "NO_ASSERTIONS"));
    }

    #[test]
    fn is_suppressed_bare_ignore_matches_any_rule() {
        let lines = ["def test_x():  # dupdup: ignore"];
        assert!(is_suppressed(&lines, 1, "NO_ASSERTIONS"));
        assert!(is_suppressed(&lines, 1, "TAUTOLOGICAL_ASSERT"));
    }

    #[test]
    fn is_suppressed_checks_the_line_above() {
        let lines = ["# dupdup: ignore[NO_ASSERTIONS]", "def test_x():"];
        assert!(is_suppressed(&lines, 2, "NO_ASSERTIONS"));
    }

    #[test]
    fn is_suppressed_does_not_reach_two_lines_up() {
        let lines = ["# dupdup: ignore[NO_ASSERTIONS]", "", "def test_x():"];
        assert!(!is_suppressed(&lines, 3, "NO_ASSERTIONS"));
    }

    #[test]
    fn is_suppressed_false_with_no_ignore_comment() {
        let lines = ["def test_x():"];
        assert!(!is_suppressed(&lines, 1, "NO_ASSERTIONS"));
    }

    #[test]
    fn a_disabled_rule_is_not_reported() {
        let source = "def test_never_fails():\n    pass\n";
        let mut config = Config::default();
        config.no_assertions.enabled = false;
        let violations = check_file("f.py", source, &config).unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn an_enabled_rule_still_fires_when_others_are_disabled() {
        let source = "def test_never_fails():\n    pass\n";
        let mut config = Config::default();
        config.duplicate_assert.enabled = false;
        config.tautological_assert.enabled = false;
        let violations = check_file("f.py", source, &config).unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "NO_ASSERTIONS");
    }

    #[test]
    fn a_call_only_body_is_a_smoke_warning_not_an_error() {
        let source = "def test_does_not_raise():\n    cache.set('k', 'v')\n";
        let violations = check_file("f.py", source, &Config::default()).unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "NO_ASSERTIONS_SMOKE");
        assert_eq!(violations[0].severity, rules::Severity::Warn);
    }

    #[test]
    fn the_smoke_rule_can_be_disabled_alone() {
        let source =
            "def test_does_not_raise():\n    cache.set('k', 'v')\n\ndef test_stub():\n    pass\n";
        let mut config = Config::default();
        config.no_assertions_smoke.enabled = false;
        let violations = check_file("f.py", source, &config).unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "NO_ASSERTIONS");
    }

    #[test]
    fn a_helper_in_the_same_file_counts_as_an_assertion() {
        let source = "def check_total(o):\n    assert o.total == 10\n\ndef test_a():\n    check_total(make())\n";
        assert!(check_file("f.py", source, &Config::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_helper_in_another_file_counts_when_it_is_indexed() {
        let helper_src = "def verify(h):\n    assert h\n";
        let test_src = "def test_a():\n    verify(make())\n";
        let config = Config::default();
        let without = check_file("t.py", test_src, &config).unwrap();
        assert_eq!(without.len(), 1);
        let helpers = AssertingHelpers::build([helper_src, test_src], &[]);
        assert!(check_file_with_helpers("t.py", test_src, &config, &helpers)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_configured_helper_name_counts_as_an_assertion() {
        let source = "def test_a():\n    external_check(make())\n";
        let mut config = Config::default();
        config.no_assertions.helpers = vec!["external_check".to_string()];
        assert!(check_file("t.py", source, &config).unwrap().is_empty());
    }
}
