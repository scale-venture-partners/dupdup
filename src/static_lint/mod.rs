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

/// The assertion fingerprint ([`rules::assertion_fingerprint`]) of every test
/// in `source`, keyed by its pytest path within the file: `test_x` for a
/// module-level function, `TestC::test_x` for a method.
///
/// # Errors
/// Returns an error if `source` can't be parsed as Python.
pub fn assertion_fingerprints(
    source: &str,
    helpers: &AssertingHelpers,
) -> anyhow::Result<std::collections::HashMap<String, rules::Fingerprint>> {
    let tree = parse(source)?;
    let mut out = std::collections::HashMap::new();
    collect_fingerprints(tree.root_node(), source.as_bytes(), "", helpers, &mut out);
    Ok(out)
}

fn collect_fingerprints(
    node: Node,
    source: &[u8],
    prefix: &str,
    helpers: &AssertingHelpers,
    out: &mut std::collections::HashMap<String, rules::Fingerprint>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                let name = test_name(child, source);
                if name.starts_with("test_") {
                    out.insert(
                        format!("{prefix}{name}"),
                        rules::assertion_fingerprint(child, source, helpers),
                    );
                }
            }
            "class_definition" => {
                let name = child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .unwrap_or("");
                collect_fingerprints(child, source, &format!("{prefix}{name}::"), helpers, out);
            }
            _ => collect_fingerprints(child, source, prefix, helpers, out),
        }
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
    let mut shapes: std::collections::BTreeMap<String, Vec<(String, usize)>> = Default::default();

    for fn_node in find_test_functions(tree.root_node(), source_bytes) {
        let name = test_name(fn_node, source_bytes).to_string();
        let mut found = Vec::new();

        if config.clone_test.enabled {
            if let Some(shape) = rules::clone_shape(fn_node, source_bytes) {
                shapes
                    .entry(shape)
                    .or_default()
                    .push((name.clone(), fn_node.start_position().row + 1));
            }
        }

        found.extend(
            rules::check_no_assertions(fn_node, source_bytes, helpers).filter(|v| {
                if v.rule == "NO_ASSERTIONS" {
                    config.no_assertions.enabled
                } else {
                    config.no_assertions_smoke.enabled
                }
            }),
        );
        if config.mock_only_assert.enabled {
            found.extend(rules::check_mock_only_assert(fn_node, source_bytes));
        }
        if config.weak_assertions.enabled {
            found.extend(rules::check_weak_assertions(fn_node, source_bytes, helpers));
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

    for mut group in shapes.into_values().filter(|g| g.len() > 1) {
        group.sort_by_key(|(_, line)| *line);
        let (first, line) = group[0].clone();
        let others: Vec<&str> = group[1..].iter().map(|(n, _)| n.as_str()).collect();
        out.push(Violation {
            file: path.to_string(),
            line,
            severity: rules::Severity::Warn,
            rule: "CLONE_TEST",
            test_name: first,
            message: format!(
                "same body as {} other test(s) apart from literals ({}) — a parametrize candidate",
                others.len(),
                others.join(", ")
            ),
        });
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

    #[test]
    fn fingerprints_are_keyed_by_class_qualified_name() {
        let source = "def test_a():\n    assert x == 1\n\nclass TestC:\n    @pytest.mark.slow\n    def test_b(self):\n        assert y\n        mock.assert_called_once_with(1)\n";
        let fps = assertion_fingerprints(source, &AssertingHelpers::default()).unwrap();
        assert_eq!(fps["test_a"].assertions, vec!["assert x == 1"]);
        assert_eq!(
            fps["TestC::test_b"].assertions,
            vec!["assert y", "mock . assert_called_once_with ( 1 )"]
        );
    }

    #[test]
    fn different_literals_make_different_fingerprints() {
        let a = assertion_fingerprints(
            "def test_a():\n    assert f() == 1\n",
            &AssertingHelpers::default(),
        )
        .unwrap();
        let b = assertion_fingerprints(
            "def test_a():\n    assert f() == 2\n",
            &AssertingHelpers::default(),
        )
        .unwrap();
        assert_ne!(a["test_a"].assertions, b["test_a"].assertions);
    }

    #[test]
    fn a_raises_context_and_a_helper_call_are_part_of_the_fingerprint() {
        let source = "def check(x):\n    assert x\n\ndef test_a():\n    with pytest.raises(ValueError):\n        f()\n    check(1)\n";
        let helpers = AssertingHelpers::build([source], &[]);
        let fps = assertion_fingerprints(source, &helpers).unwrap();
        let a = &fps["test_a"].assertions;
        assert_eq!(a.len(), 2);
        assert!(a
            .iter()
            .any(|s| s.contains("raises") && s.contains("f ( )")));
        assert!(a.iter().any(|s| s == "check ( 1 )"));
        assert_eq!(fps["test_a"].calls, vec!["check", "f", "raises"]);
    }

    #[test]
    fn tests_that_differ_only_in_literals_are_one_clone_group() {
        let source = "def test_a():\n    r = f('x', 1)\n    assert r == 2\n\ndef test_b():\n    r = f('y', 3)\n    assert r == 4\n\ndef test_c():\n    r = g('y', 3)\n    assert r == 4\n";
        let v = check_file("f.py", source, &Config::default()).unwrap();
        let clones: Vec<_> = v.iter().filter(|v| v.rule == "CLONE_TEST").collect();
        assert_eq!(clones.len(), 1);
        assert_eq!(clones[0].test_name, "test_a");
        assert!(clones[0].message.contains("test_b") && !clones[0].message.contains("test_c"));
    }

    #[test]
    fn a_parametrized_test_is_not_a_clone() {
        let source = "@pytest.mark.parametrize('x', [1])\ndef test_a(x):\n    r = f(x, 1)\n    assert r == 2\n\n@pytest.mark.parametrize('x', [1])\ndef test_b(x):\n    r = f(x, 1)\n    assert r == 2\n";
        let v = check_file("f.py", source, &Config::default()).unwrap();
        assert!(v.iter().all(|v| v.rule != "CLONE_TEST"));
    }

    #[test]
    fn a_tiny_body_is_not_a_clone() {
        let source = "def test_a():\n    assert f(1)\n\ndef test_b():\n    assert f(2)\n";
        let v = check_file("f.py", source, &Config::default()).unwrap();
        assert!(v.iter().all(|v| v.rule != "CLONE_TEST"));
    }

    #[test]
    fn the_clone_rule_can_be_disabled_and_suppressed() {
        let source = "def test_a():  # dupdup: ignore[CLONE_TEST]\n    r = f('x', 1)\n    assert r == 2\n\ndef test_b():\n    r = f('y', 3)\n    assert r == 4\n";
        let v = check_file("f.py", source, &Config::default()).unwrap();
        assert!(v.iter().all(|v| v.rule != "CLONE_TEST"));
        let mut config = Config::default();
        config.clone_test.enabled = false;
        let plain = source.replace("  # dupdup: ignore[CLONE_TEST]", "");
        let v = check_file("f.py", &plain, &config).unwrap();
        assert!(v.iter().all(|v| v.rule != "CLONE_TEST"));
    }

    #[test]
    fn a_docstring_does_not_make_two_bodies_differ() {
        let source = "def test_a():\n    \"one\"\n    r = f('x', 1)\n    assert r == 2\n\ndef test_b():\n    \"two\"\n    r = f('y', 3)\n    assert r == 4\n";
        let v = check_file("f.py", source, &Config::default()).unwrap();
        assert_eq!(v.iter().filter(|v| v.rule == "CLONE_TEST").count(), 1);
    }

    fn weak(source: &str) -> usize {
        check_file("f.py", source, &Config::default())
            .unwrap()
            .iter()
            .filter(|v| v.rule == "WEAK_ASSERTIONS")
            .count()
    }

    #[test]
    fn weak_shapes_alone_are_flagged() {
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert r is not None\n"),
            1
        );
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert isinstance(r, dict)\n"),
            1
        );
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert len(r) > 0\n"),
            1
        );
        assert_eq!(weak("def test_a():\n    r = f()\n    assert r\n"), 1);
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert r is not None\n    assert isinstance(r, dict)\n"),
            1
        );
    }

    #[test]
    fn one_strong_assert_clears_the_test() {
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert r is not None\n    assert r['k'] == 2\n"),
            0
        );
    }

    #[test]
    fn a_mock_assertion_raises_or_helper_is_a_strong_check() {
        assert_eq!(
            weak("def test_a():\n    r = f()\n    assert r\n    m.assert_called_once_with(1)\n"),
            0
        );
        assert_eq!(
            weak("def test_a():\n    with pytest.raises(E):\n        f()\n    assert g\n"),
            0
        );
        let source = "def check(x):\n    assert x == 1\n\ndef test_a():\n    r = f()\n    assert r\n    check(r)\n";
        assert_eq!(weak(source), 0);
    }

    #[test]
    fn equality_and_comparison_asserts_are_not_weak() {
        assert_eq!(weak("def test_a():\n    assert f() == 1\n"), 0);
        assert_eq!(weak("def test_a():\n    assert f() is None\n"), 0);
        assert_eq!(weak("def test_a():\n    assert len(f()) == 2\n"), 0);
    }

    fn mock_only(source: &str) -> usize {
        check_file("f.py", source, &Config::default())
            .unwrap()
            .iter()
            .filter(|v| v.rule == "MOCK_ONLY_ASSERT")
            .count()
    }

    #[test]
    fn calling_your_own_mock_and_asserting_on_it_is_flagged() {
        assert_eq!(
            mock_only(
                "def test_a():\n    m = Mock()\n    m(1)\n    m.assert_called_once_with(1)\n"
            ),
            1
        );
        assert_eq!(
            mock_only("async def test_a():\n    m = AsyncMock()\n    await m(1)\n    assert m.call_count == 1\n"),
            1
        );
        assert_eq!(
            mock_only("def test_a():\n    with patch('x.y') as m:\n        m(1)\n    m.assert_called_once()\n"),
            1
        );
    }

    #[test]
    fn code_under_test_calling_the_mock_is_not_flagged() {
        assert_eq!(
            mock_only(
                "def test_a():\n    m = Mock()\n    f(m)\n    m.assert_called_once_with(1)\n"
            ),
            0
        );
    }

    #[test]
    fn a_real_assertion_alongside_clears_it() {
        assert_eq!(
            mock_only("def test_a():\n    m = Mock(return_value=2)\n    r = m(1)\n    m.assert_called_once()\n    assert r == 2\n"),
            0
        );
        assert_eq!(
            mock_only("def test_a():\n    m = Mock()\n    m(1)\n    m.assert_called_once()\n    mock_b.assert_called()\n"),
            0
        );
    }
}
