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

/// Every `block` (indented suite) in `fn_node`, top-level and nested, except
/// inside a nested `def`. Used to compare statements that are direct
/// siblings — i.e. truly adjacent in the source, with no other statement of
/// any kind between them, not just no other `assert`.
fn blocks<'a>(fn_node: Node<'a>) -> Vec<Node<'a>> {
    let mut out = Vec::new();
    collect_excluding_nested_defs(fn_node, "block", true, &mut out);
    out
}

/// `mock.assert_called_once_with(...)`, unittest's `self.assertEqual(...)`,
/// a shared local helper like `_assert_round_trip(...)`, and
/// `pytest.fail(...)`/`self.fail(...)` are all assertion mechanisms in
/// their own right — a call whose name (after stripping a leading
/// underscore, the usual "private helper" convention) starts with `assert`
/// or is exactly `fail` ends the test on its own, the same as a bare
/// `assert` statement.
fn has_assertion_method_call(fn_node: Node, source: &[u8]) -> bool {
    let mut calls = Vec::new();
    collect_excluding_nested_defs(fn_node, "call", true, &mut calls);
    calls.iter().any(|call| {
        let Some(func) = call.child_by_field_name("function") else {
            return false;
        };
        let name_node = match func.kind() {
            "attribute" => func.child_by_field_name("attribute"),
            "identifier" => Some(func),
            _ => None,
        };
        name_node
            .and_then(|n| n.utf8_text(source).ok())
            .map(|name| {
                let trimmed = name.trim_start_matches('_');
                (trimmed.starts_with("assert") && trimmed.len() > "assert".len()) || trimmed == "fail"
            })
            .unwrap_or(false)
    })
}

/// `pytest.skip(...)` reports the test as skipped rather than passed —
/// unlike a silent `pass` stub, it already tells anyone reading test output
/// that nothing was actually checked, so it isn't flagged the same way.
fn has_pytest_skip_call(fn_node: Node, source: &[u8]) -> bool {
    let mut calls = Vec::new();
    collect_excluding_nested_defs(fn_node, "call", true, &mut calls);
    calls.iter().any(|call| {
        call.child_by_field_name("function")
            .filter(|f| f.kind() == "attribute")
            .and_then(|f| f.child_by_field_name("attribute"))
            .and_then(|attr| attr.utf8_text(source).ok())
            .map(|name| name == "skip")
            .unwrap_or(false)
    })
}

/// True if any node in `node`'s subtree (`node` included) is a `call` — used
/// to keep the tautological-assert check to operands with no side effects,
/// since a repeated call can return a different value each time.
fn contains_call(node: Node) -> bool {
    if node.kind() == "call" {
        return true;
    }
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).any(contains_call);
    result
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

/// A test with no `assert`, no `mock`/unittest-style `.assert*(...)` call,
/// and no `pytest.raises`/`assertRaises` context passes no matter what the
/// code under test does.
pub fn check_no_assertions(fn_node: Node, source: &[u8]) -> Option<Violation> {
    if !assert_statements(fn_node).is_empty() {
        return None;
    }
    if has_assertion_method_call(fn_node, source) {
        return None;
    }
    if has_pytest_skip_call(fn_node, source) {
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

/// Two back-to-back `assert` statements — direct siblings in the same
/// block, no other statement between them — with identical (whitespace-
/// normalized) text. The same assertion re-checked after an intervening
/// statement is usually re-verifying an invariant held across a state
/// change, not a copy-paste duplicate, so only true adjacency counts.
pub fn check_duplicate_assert(fn_node: Node, source: &[u8]) -> Vec<Violation> {
    let mut out = Vec::new();
    for block in blocks(fn_node) {
        let mut cursor = block.walk();
        let children: Vec<Node> = block.children(&mut cursor).collect();
        for pair in children.windows(2) {
            let (prev, cur) = (&pair[0], &pair[1]);
            if prev.kind() != "assert_statement" || cur.kind() != "assert_statement" {
                continue;
            }
            let prev_text = normalize(prev.utf8_text(source).unwrap_or(""));
            let cur_text = normalize(cur.utf8_text(source).unwrap_or(""));
            if prev_text == cur_text {
                out.push(Violation {
                    file: String::new(),
                    line: cur.start_position().row + 1,
                    severity: Severity::Warn,
                    rule: "DUPLICATE_ASSERT",
                    test_name: String::new(),
                    message: format!(
                        "repeats the assertion on line {} with nothing in between",
                        prev.start_position().row + 1
                    ),
                });
            }
        }
    }
    out
}

/// `assert x == x` — both operands of `==` are the same expression, so the
/// assertion is true independent of the code under test. Operands that
/// contain a call are excluded: `assert f(x) == f(x)` calls `f` twice and
/// is testing that `f` is deterministic, not a tautology.
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
        if contains_call(children[0]) || contains_call(children[2]) {
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
