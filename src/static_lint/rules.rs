//! The static rules `check_file` runs, and the `Violation`/`Severity` types
//! they emit.

use serde::Serialize;
use tree_sitter::Node;

/// `Error` fails `dupdup check`'s exit code; `Warn` is reported but doesn't.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Blocks the check.
    Error,
    /// Reported but non-blocking.
    Warn,
}

/// One static-rule finding.
#[derive(Debug, Clone, Serialize)]
pub struct Violation {
    /// The file the violation was found in.
    pub file: String,
    /// 1-indexed line the violation is anchored to.
    pub line: usize,
    /// Whether this violation blocks the check.
    pub severity: Severity,
    /// The rule name, e.g. `"NO_ASSERTIONS"`.
    pub rule: &'static str,
    /// The test function the violation is in.
    pub test_name: String,
    /// Human-readable explanation of the violation.
    pub message: String,
}

fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `assert_statement` nodes anywhere in `fn_node`'s body, except inside a
/// nested `def` — a helper defined inside a test has its own assertion
/// surface, distinct from the test's.
fn assert_statements(fn_node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    collect_excluding_nested_defs(fn_node, "assert_statement", true, &mut out);
    out
}

fn with_statements(fn_node: Node<'_>) -> Vec<Node<'_>> {
    let mut out = Vec::new();
    collect_excluding_nested_defs(fn_node, "with_statement", true, &mut out);
    out
}

/// Every `block` (indented suite) in `fn_node`, top-level and nested, except
/// inside a nested `def`. Used to compare statements that are direct
/// siblings — i.e. truly adjacent in the source, with no other statement of
/// any kind between them, not just no other `assert`.
fn blocks(fn_node: Node<'_>) -> Vec<Node<'_>> {
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
            .is_some_and(|name| {
                let trimmed = name.trim_start_matches('_');
                (trimmed.starts_with("assert") && trimmed.len() > "assert".len())
                    || trimmed == "fail"
            })
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
            .is_some_and(|name| name == "skip")
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

/// True if the function asserts something itself: an `assert` statement, an
/// assertion-style call (`mock.assert_*`, `self.assertX`, `pytest.fail`), or a
/// `pytest.raises`/`assertRaises` context. A `pytest.skip` call is not
/// included — it is exempt from `NO_ASSERTIONS` but doesn't make a helper
/// an asserting one.
#[must_use]
pub fn asserts_directly(fn_node: Node, source: &[u8]) -> bool {
    !assert_statements(fn_node).is_empty()
        || has_assertion_method_call(fn_node, source)
        || has_raises_context(fn_node, source)
}

fn has_raises_context(fn_node: Node, source: &[u8]) -> bool {
    with_statements(fn_node)
        .iter()
        .any(|w| w.utf8_text(source).unwrap_or("").contains("raises"))
}

/// The final identifier of every call in the function body, outside nested
/// defs: `a.b.c(x)` yields `c`, `f(x)` yields `f`.
#[must_use]
pub fn called_names(fn_node: Node, source: &[u8]) -> Vec<String> {
    let mut calls = Vec::new();
    collect_excluding_nested_defs(fn_node, "call", true, &mut calls);
    calls
        .iter()
        .filter_map(|call| {
            let func = call.child_by_field_name("function")?;
            let name_node = match func.kind() {
                "attribute" => func.child_by_field_name("attribute")?,
                "identifier" => func,
                _ => return None,
            };
            name_node.utf8_text(source).ok().map(str::to_string)
        })
        .collect()
}

/// A whitespace-insensitive rendering of `node`: its leaf tokens joined by
/// a single space, comments dropped. Literals are kept, so `x == 1` and
/// `x == 2` stay different.
fn token_text(node: Node, source: &[u8]) -> String {
    fn walk(node: Node, source: &[u8], out: &mut Vec<String>) {
        if node.kind() == "comment" {
            return;
        }
        if node.child_count() == 0 || node.kind() == "string" {
            if let Ok(text) = node.utf8_text(source) {
                out.push(text.to_string());
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, source, out);
        }
    }
    let mut tokens = Vec::new();
    walk(node, source, &mut tokens);
    tokens.join(" ")
}

/// The body of a test with every string and number replaced by one
/// placeholder, as a token string: two tests with equal shapes run the same
/// code and differ only in literal values. `None` for a test that is already
/// `parametrize`d, has no body beyond a docstring, or is too short to be worth
/// merging.
#[must_use]
pub fn clone_shape(fn_node: Node, source: &[u8]) -> Option<String> {
    const MIN_TOKENS: usize = 8;
    if let Some(parent) = fn_node.parent() {
        if parent.kind() == "decorated_definition"
            && parent
                .utf8_text(source)
                .is_ok_and(|t| t.contains("parametrize"))
        {
            return None;
        }
    }
    fn walk(node: Node, source: &[u8], out: &mut Vec<String>) {
        match node.kind() {
            "comment" => {}
            "string" | "integer" | "float" | "concatenated_string" => out.push("L".into()),
            _ if node.child_count() == 0 => {
                if let Ok(text) = node.utf8_text(source) {
                    out.push(text.to_string());
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk(child, source, out);
                }
            }
        }
    }
    let body = fn_node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    let mut statements: Vec<Node> = body.children(&mut cursor).collect();
    if statements.first().is_some_and(|s| {
        s.kind() == "expression_statement" && s.child(0).is_some_and(|c| c.kind() == "string")
    }) {
        statements.remove(0);
    }
    let mut tokens = Vec::new();
    for s in statements {
        walk(s, source, &mut tokens);
    }
    (tokens.len() >= MIN_TOKENS).then(|| tokens.join(" "))
}

/// Whether an `assert` expression can't tell a right result from a wrong one:
/// `x is not None`, `isinstance(...)`, `len(x) > 0`, or a bare name,
/// attribute or subscript tested for truthiness.
fn is_weak_assertion(assert_node: Node, source: &[u8]) -> bool {
    let Some(expr) = assert_node.named_child(0) else {
        return false;
    };
    if matches!(expr.kind(), "identifier" | "attribute" | "subscript") {
        return true;
    }
    let text = normalize(expr.utf8_text(source).unwrap_or(""));
    text.ends_with(" is not None")
        || (text.starts_with("isinstance(") && text.ends_with(')'))
        || (text.starts_with("len(")
            && [") > 0", ") >= 1", ") != 0"]
                .iter()
                .any(|s| text.ends_with(s)))
}

/// A test whose every `assert` is weak (see [`is_weak_assertion`]) and that has
/// no other way of checking a result (assertion-style call, asserting helper,
/// `pytest.raises`): it passes for most broken implementations.
#[must_use]
pub fn check_weak_assertions(
    fn_node: Node,
    source: &[u8],
    helpers: &super::helpers::AssertingHelpers,
) -> Option<Violation> {
    let asserts = assert_statements(fn_node);
    if asserts.is_empty() || !asserts.iter().all(|a| is_weak_assertion(*a, source)) {
        return None;
    }
    if has_assertion_method_call(fn_node, source)
        || has_raises_context(fn_node, source)
        || called_names(fn_node, source)
            .iter()
            .any(|n| helpers.contains(n))
    {
        return None;
    }
    Some(Violation {
        file: String::new(),
        line: fn_node.start_position().row + 1,
        severity: Severity::Warn,
        rule: "WEAK_ASSERTIONS",
        test_name: String::new(),
        message: "every assertion is a None, type, length or truthiness check; it passes for most wrong results"
            .into(),
    })
}

/// Names bound in the test to a mock the test created itself: `m = Mock()`,
/// `m = AsyncMock(...)`, `m = MagicMock(...)`, `m = create_autospec(...)`, or
/// `with patch(...) as m`.
fn local_mock_names(fn_node: Node, source: &[u8]) -> Vec<String> {
    const FACTORIES: [&str; 4] = ["Mock(", "MagicMock(", "AsyncMock(", "create_autospec("];
    let mut names = Vec::new();
    let mut stack = vec![fn_node];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "assignment" => {
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                if let (Some(l), Some(r)) = (left, right) {
                    let rt = r.utf8_text(source).unwrap_or("");
                    let rt = rt.trim_start_matches("await ");
                    let tail = rt.rsplit('.').next().unwrap_or(rt);
                    if l.kind() == "identifier"
                        && (FACTORIES
                            .iter()
                            .any(|f| rt.starts_with(f) || tail.starts_with(f)))
                    {
                        if let Ok(n) = l.utf8_text(source) {
                            names.push(n.to_string());
                        }
                    }
                }
            }
            "as_pattern" => {
                let call = node.child(0).and_then(|c| c.utf8_text(source).ok());
                let alias = node.named_child(node.named_child_count().saturating_sub(1));
                if let (Some(call), Some(alias)) = (call, alias) {
                    if call.contains("patch") {
                        if let Ok(n) = alias.utf8_text(source) {
                            names.push(n.trim().to_string());
                        }
                    }
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for c in node.children(&mut cursor) {
            if c.kind() != "function_definition" {
                stack.push(c);
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// A test that calls a mock it created itself and then asserts only on mocks:
/// it checks its own wiring, not the code under test.
#[must_use]
pub fn check_mock_only_assert(fn_node: Node, source: &[u8]) -> Option<Violation> {
    let mocks = local_mock_names(fn_node, source);
    if mocks.is_empty() {
        return None;
    }
    let is_mock = |n: &str| mocks.iter().any(|m| m == n);

    let mut calls = Vec::new();
    collect_excluding_nested_defs(fn_node, "call", true, &mut calls);
    let mut called_directly = false;
    let mut other_assertion = false;
    let mut mock_assertion = false;
    for call in &calls {
        let Some(func) = call.child_by_field_name("function") else {
            continue;
        };
        match func.kind() {
            "identifier" => {
                let name = func.utf8_text(source).unwrap_or("");
                if is_mock(name) {
                    called_directly = true;
                } else if name.trim_start_matches('_').starts_with("assert") || name == "fail" {
                    other_assertion = true;
                }
            }
            "attribute" => {
                let object = func
                    .child_by_field_name("object")
                    .and_then(|o| o.utf8_text(source).ok())
                    .unwrap_or("");
                let attr = func
                    .child_by_field_name("attribute")
                    .and_then(|a| a.utf8_text(source).ok())
                    .unwrap_or("");
                let is_assertion = attr.trim_start_matches('_').starts_with("assert");
                if is_assertion && is_mock(object) {
                    mock_assertion = true;
                } else if is_assertion || attr == "fail" {
                    other_assertion = true;
                }
            }
            _ => {}
        }
    }
    for a in assert_statements(fn_node) {
        let text = normalize(a.utf8_text(source).unwrap_or(""));
        let body = text.trim_start_matches("assert ").trim_start();
        let subject: String = body
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if is_mock(&subject) && body[subject.len()..].starts_with('.') {
            mock_assertion = true;
        } else {
            other_assertion = true;
        }
    }
    if has_raises_context(fn_node, source) {
        other_assertion = true;
    }
    (called_directly && mock_assertion && !other_assertion).then(|| Violation {
        file: String::new(),
        line: fn_node.start_position().row + 1,
        severity: Severity::Warn,
        rule: "MOCK_ONLY_ASSERT",
        test_name: String::new(),
        message: "calls a mock it created and asserts only on mocks; it tests its own wiring"
            .into(),
    })
}

/// What a test checks and what it calls.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// What the test checks; see [`assertion_fingerprint`].
    pub assertions: Vec<String>,
    /// The name of every function or method the test calls directly, sorted
    /// and deduplicated. It binds a generic assertion (`result is None`) to
    /// the thing it is asserted about.
    pub calls: Vec<String>,
}

/// What the test checks, as comparable strings: each `assert` statement, each
/// assertion-style call (`mock.assert_*(...)`, `self.assertX(...)`,
/// `pytest.fail(...)`), each call to an asserting helper, and each
/// `pytest.raises(...)` statement, in whitespace-insensitive source form.
/// Sorted and deduplicated. Two tests with different fingerprints check
/// different things, whatever code they happen to run.
#[must_use]
pub fn assertion_fingerprint(
    fn_node: Node,
    source: &[u8],
    helpers: &super::helpers::AssertingHelpers,
) -> Fingerprint {
    let mut out: Vec<String> = assert_statements(fn_node)
        .iter()
        .map(|n| token_text(*n, source))
        .collect();

    let mut calls = Vec::new();
    collect_excluding_nested_defs(fn_node, "call", true, &mut calls);
    for call in calls {
        let Some(func) = call.child_by_field_name("function") else {
            continue;
        };
        let name_node = match func.kind() {
            "attribute" => func.child_by_field_name("attribute"),
            "identifier" => Some(func),
            _ => None,
        };
        let Some(name) = name_node.and_then(|n| n.utf8_text(source).ok()) else {
            continue;
        };
        let trimmed = name.trim_start_matches('_');
        let assertion_call =
            (trimmed.starts_with("assert") && trimmed.len() > "assert".len()) || trimmed == "fail";
        if assertion_call || helpers.contains(name) {
            out.push(token_text(call, source));
        }
    }

    for with in with_statements(fn_node) {
        let mut cursor = with.walk();
        let clause = with
            .children(&mut cursor)
            .find(|c| c.kind() == "with_clause");
        if let Some(clause) = clause {
            if token_text(clause, source).contains("raises") {
                // The whole statement, not just the clause: what is called
                // inside is what is expected to raise.
                out.push(token_text(with, source));
            }
        }
    }

    out.sort();
    out.dedup();
    let mut calls = called_names(fn_node, source);
    calls.sort();
    calls.dedup();
    Fingerprint {
        assertions: out,
        calls,
    }
}

/// A test with no `assert`, no `mock`/unittest-style `.assert*(...)` call,
/// no call to a helper known to assert (`helpers`), and no
/// `pytest.raises`/`assertRaises` context passes no matter what the code
/// under test does.
///
/// A body with no call at all (`pass`, a docstring, `...`) can never fail:
/// `NO_ASSERTIONS`, an error. A body that only makes calls can still fail by
/// raising, so it is the weaker `NO_ASSERTIONS_SMOKE`, a warning.
#[must_use]
pub fn check_no_assertions(
    fn_node: Node,
    source: &[u8],
    helpers: &super::helpers::AssertingHelpers,
) -> Option<Violation> {
    if asserts_directly(fn_node, source) || has_pytest_skip_call(fn_node, source) {
        return None;
    }
    let calls = called_names(fn_node, source);
    if calls.iter().any(|name| helpers.contains(name)) {
        return None;
    }
    let line = fn_node.start_position().row + 1;
    if calls.is_empty() {
        Some(Violation {
            file: String::new(),
            line,
            severity: Severity::Error,
            rule: "NO_ASSERTIONS",
            test_name: String::new(),
            message: "no assert statement and no call — this test can never fail".into(),
        })
    } else {
        Some(Violation {
            file: String::new(),
            line,
            severity: Severity::Warn,
            rule: "NO_ASSERTIONS_SMOKE",
            test_name: String::new(),
            message: "no assertion; this only fails if a call raises".into(),
        })
    }
}

/// Two back-to-back `assert` statements — direct siblings in the same
/// block, no other statement between them — with identical (whitespace-
/// normalized) text. The same assertion re-checked after an intervening
/// statement is usually re-verifying an invariant held across a state
/// change, not a copy-paste duplicate, so only true adjacency counts. An
/// assert containing a call is skipped: repeating it re-invokes the call,
/// which checks idempotence or caching rather than duplicating a check.
#[must_use]
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
            if prev_text == cur_text && !contains_call(*cur) {
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
#[must_use]
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
