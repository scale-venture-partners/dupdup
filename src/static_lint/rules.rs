use serde::Serialize;
use tree_sitter::Node;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warn,
}

#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    pub file: String,
    pub line: usize,
    pub severity: Severity,
    pub rule: &'static str,
    pub test_name: String,
    pub message: String,
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `assert_statement` nodes anywhere in `fn_node`'s body, except inside a
/// nested `def` — a helper defined inside a test has its own assertion
/// surface, distinct from the test's.
fn assert_statements<'a>(fn_node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    collect_excluding_nested_defs(fn_node, "assert_statement", true, &mut out);
    out
}

fn with_statements<'a>(fn_node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    collect_excluding_nested_defs(fn_node, "with_statement", true, &mut out);
    out
}

fn collect_excluding_nested_defs<'a>(
    node: Node<'a>,
    kind: &str,
    is_root: bool,
    out: &mut Vec<Node<'a>>,
) {
    if !is_root && node.kind() == "function_definition" {
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == kind {
            out.push(child);
        }
        collect_excluding_nested_defs(child, kind, false, out);
    }
}

/// A test with no `assert` and no `pytest.raises`/`assertRaises` context
/// passes no matter what the code under test does.
pub fn check_no_assertions(fn_node: Node, source: &[u8]) -> Option<Violation> {
    if !assert_statements(fn_node).is_empty() {
        return None;
    }
    let raises = with_statements(fn_node)
        .iter()
        .any(|w| w.utf8_text(source).unwrap_or("").contains("raises"));
    if raises {
        return None;
    }
    Some(Violation {
        file: String::new(),
        line: fn_node.start_position().row + 1,
        severity: Severity::Error,
        rule: "NO_ASSERTIONS",
        test_name: String::new(),
        message: "no assert statement and no raises-context — this test can never fail".into(),
    })
}

/// Two `assert` statements in the same test with identical (whitespace-
/// normalized) text — the second one checks nothing the first didn't.
pub fn check_duplicate_assert(fn_node: Node, source: &[u8]) -> Vec<Violation> {
    let asserts = assert_statements(fn_node);
    let mut seen: Vec<(String, usize)> = Vec::new();
    let mut out = Vec::new();
    for a in &asserts {
        let text = normalize(a.utf8_text(source).unwrap_or(""));
        match seen.iter().find(|(t, _)| *t == text) {
            Some((_, first_line)) => out.push(Violation {
                file: String::new(),
                line: a.start_position().row + 1,
                severity: Severity::Warn,
                rule: "DUPLICATE_ASSERT",
                test_name: String::new(),
                message: format!("repeats the assertion already made on line {first_line}"),
            }),
            None => seen.push((text, a.start_position().row + 1)),
        }
    }
    out
}

/// `assert x == x` — both operands of `==` are the same expression, so the
/// assertion is true independent of the code under test.
pub fn check_tautological_assert(fn_node: Node, source: &[u8]) -> Vec<Violation> {
    let mut out = Vec::new();
    for a in assert_statements(fn_node) {
        let Some(test_expr) = a.child(1) else {
            continue;
        };
        if test_expr.kind() != "comparison_operator" {
            continue;
        }
        let mut cursor = test_expr.walk();
        let children: Vec<Node> = test_expr.children(&mut cursor).collect();
        // A chained comparison (`a == b == c`) has more than 3 children;
        // only the single-operator shape is checked here.
        if children.len() != 3 {
            continue;
        }
        if children[1].utf8_text(source).unwrap_or("") != "==" {
            continue;
        }
        let left = normalize(children[0].utf8_text(source).unwrap_or(""));
        let right = normalize(children[2].utf8_text(source).unwrap_or(""));
        if !left.is_empty() && left == right {
            out.push(Violation {
                file: String::new(),
                line: a.start_position().row + 1,
                severity: Severity::Error,
                rule: "TAUTOLOGICAL_ASSERT",
                test_name: String::new(),
                message: format!("both sides of `==` are `{left}` — always true"),
            });
        }
    }
    out
}
