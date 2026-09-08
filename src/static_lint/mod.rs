pub mod rules;

use crate::config::Config;
use rules::Violation;
use tree_sitter::{Node, Parser, Tree};

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

pub fn check_file(path: &str, source: &str, config: &Config) -> anyhow::Result<Vec<Violation>> {
    let tree = parse(source)?;
    let source_bytes = source.as_bytes();
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();

    for fn_node in find_test_functions(tree.root_node(), source_bytes) {
        let name = test_name(fn_node, source_bytes).to_string();
        let mut found = Vec::new();

        if config.no_assertions.enabled {
            found.extend(rules::check_no_assertions(fn_node, source_bytes));
        }
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

    out.retain(|v| !is_suppressed(&lines, v.line, v.rule));
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
        let source = "def test_never_fails():\n    compute_something()\n";
        let mut config = Config::default();
        config.no_assertions.enabled = false;
        let violations = check_file("f.py", source, &config).unwrap();
        assert!(violations.is_empty());
    }

    #[test]
    fn an_enabled_rule_still_fires_when_others_are_disabled() {
        let source = "def test_never_fails():\n    compute_something()\n";
        let mut config = Config::default();
        config.duplicate_assert.enabled = false;
        config.tautological_assert.enabled = false;
        let violations = check_file("f.py", source, &config).unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, "NO_ASSERTIONS");
    }
}
