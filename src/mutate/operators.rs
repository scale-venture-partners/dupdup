//! Mutant generation: a small, fixed set of point mutations found by
//! walking the same tree-sitter AST `static_lint` uses. Each mutation
//! operator maps one specific token to one specific replacement — never
//! "all other possibilities" — to keep the mutant count, and therefore the
//! number of test runs, bounded and predictable.

use std::ops::RangeInclusive;
use tree_sitter::Node;

/// One point mutation: replacing `source[byte_start..byte_end]` with
/// `replacement` produces this mutant's source text.
pub struct Mutant {
    /// A stable, human-readable id: `<file>:<line>:<operator>:<index>`.
    pub id: String,
    /// The 1-indexed lines of the enclosing statement, used to look up
    /// which tests could plausibly exercise this mutant. This is
    /// deliberately the *statement's* span, not just the line the mutated
    /// token sits on: a multi-line condition's operator can sit on a line
    /// the line-coverage tracer never records against, while the
    /// statement's own start line always is. The trade-off is that a
    /// mutant inside a compound statement's header (`if`/`while`/`for`)
    /// may pull in candidate tests from that statement's whole body, not
    /// just its condition — a wider net than strictly necessary, never a
    /// narrower one.
    pub candidate_lines: RangeInclusive<usize>,
    byte_start: usize,
    byte_end: usize,
    replacement: String,
}

impl Mutant {
    /// Splices this mutant's replacement into `source`, which must be the
    /// exact text `find_mutants` was called with.
    #[must_use]
    pub fn apply(&self, source: &str) -> String {
        let mut out = String::with_capacity(source.len());
        out.push_str(&source[..self.byte_start]);
        out.push_str(&self.replacement);
        out.push_str(&source[self.byte_end..]);
        out
    }
}

const COMPARISON_FLIPS: &[(&str, &str)] = &[
    ("==", "!="),
    ("!=", "=="),
    ("<", ">="),
    ("<=", ">"),
    (">", "<="),
    (">=", "<"),
];

const ARITHMETIC_FLIPS: &[(&str, &str)] = &[("+", "-"), ("-", "+"), ("*", "/"), ("/", "*")];

const BOOLEAN_CONNECTIVE_FLIPS: &[(&str, &str)] = &[("and", "or"), ("or", "and")];

fn flip(table: &'static [(&'static str, &'static str)], text: &str) -> Option<&'static str> {
    table
        .iter()
        .find(|(from, _)| *from == text)
        .map(|(_, to)| *to)
}

/// The nearest ancestor of `node` that is itself a direct statement in an
/// enclosing suite (its parent is a `block` or the top-level `module`).
/// This finds "the statement `node` lives in" without having to enumerate
/// every one of Python's statement kinds: whatever kind it is, a Python
/// statement is always a direct child of a `block` or `module` node.
fn enclosing_statement(node: Node<'_>) -> Node<'_> {
    let mut current = node;
    while let Some(parent) = current.parent() {
        if parent.kind() == "block" || parent.kind() == "module" {
            return current;
        }
        current = parent;
    }
    current
}

fn candidate_lines(node: Node<'_>) -> RangeInclusive<usize> {
    let statement = enclosing_statement(node);
    (statement.start_position().row + 1)..=(statement.end_position().row + 1)
}

fn push(out: &mut Vec<Mutant>, file: &str, operator: &str, node: Node<'_>, replacement: String) {
    let line = node.start_position().row + 1;
    let id = format!("{file}:{line}:{operator}:{}", out.len());
    out.push(Mutant {
        id,
        candidate_lines: candidate_lines(node),
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        replacement,
    });
}

fn walk(node: Node<'_>, source: &[u8], file: &str, out: &mut Vec<Mutant>) {
    match node.kind() {
        "comparison_operator" => {
            let mut cursor = node.walk();
            let ops: Vec<Node> = node
                .children_by_field_name("operators", &mut cursor)
                .collect();
            // A chained comparison (`a < b < c`) has more than one operator
            // token; skipped rather than guessing which one to flip.
            if let [op] = ops[..] {
                if let Ok(text) = op.utf8_text(source) {
                    if let Some(replacement) = flip(COMPARISON_FLIPS, text) {
                        push(out, file, "COMPARISON", op, replacement.to_string());
                    }
                }
            }
        }
        "boolean_operator" => {
            if let Some(op) = node.child_by_field_name("operator") {
                if let Ok(text) = op.utf8_text(source) {
                    if let Some(replacement) = flip(BOOLEAN_CONNECTIVE_FLIPS, text) {
                        push(out, file, "BOOLEAN_OPERATOR", op, replacement.to_string());
                    }
                }
            }
        }
        "binary_operator" => {
            if let Some(op) = node.child_by_field_name("operator") {
                if let Ok(text) = op.utf8_text(source) {
                    if let Some(replacement) = flip(ARITHMETIC_FLIPS, text) {
                        push(out, file, "ARITHMETIC", op, replacement.to_string());
                    }
                }
            }
        }
        "true" => push(out, file, "BOOLEAN_LITERAL", node, "False".to_string()),
        "false" => push(out, file, "BOOLEAN_LITERAL", node, "True".to_string()),
        "integer" => {
            // Only a plain base-10 literal parses here: hex/octal/binary
            // prefixes and underscore grouping fail `str::parse` and are
            // skipped rather than mutated with a guess at their meaning.
            if let Ok(text) = node.utf8_text(source) {
                if let Ok(n) = text.parse::<i64>() {
                    push(out, file, "INCREMENT", node, (n + 1).to_string());
                }
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, file, out);
    }
}

/// Finds every mutation site in `source`, one mutant per site. `file` is
/// used only to build a stable, human-readable mutant id — it isn't read
/// from disk.
///
/// # Errors
/// Returns an error if `source` can't be parsed as Python.
pub fn find_mutants(file: &str, source: &str) -> anyhow::Result<Vec<Mutant>> {
    let tree = crate::static_lint::parse(source)?;
    let mut out = Vec::new();
    walk(tree.root_node(), source.as_bytes(), file, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(source: &str) -> Vec<Mutant> {
        find_mutants("f.py", source).unwrap()
    }

    #[test]
    fn flips_a_comparison_operator() {
        let mutants = find("def f(a, b):\n    return a == b\n");
        assert_eq!(mutants.len(), 1);
        assert_eq!(
            mutants[0].apply("def f(a, b):\n    return a == b\n"),
            "def f(a, b):\n    return a != b\n"
        );
    }

    #[test]
    fn skips_a_chained_comparison() {
        let mutants = find("def f(a, b, c):\n    return a < b < c\n");
        assert!(mutants.is_empty());
    }

    #[test]
    fn flips_a_boolean_connective() {
        let source = "def f(a, b):\n    return a and b\n";
        let mutants = find(source);
        assert_eq!(mutants.len(), 1);
        assert_eq!(
            mutants[0].apply(source),
            "def f(a, b):\n    return a or b\n"
        );
    }

    #[test]
    fn flips_an_arithmetic_operator() {
        let source = "def f(a, b):\n    return a + b\n";
        let mutants = find(source);
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].apply(source), "def f(a, b):\n    return a - b\n");
    }

    #[test]
    fn flips_a_boolean_literal() {
        let source = "def f():\n    return True\n";
        let mutants = find(source);
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].apply(source), "def f():\n    return False\n");
    }

    #[test]
    fn increments_a_plain_integer_literal() {
        let source = "def f():\n    return 41\n";
        let mutants = find(source);
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].apply(source), "def f():\n    return 42\n");
    }

    #[test]
    fn skips_a_non_decimal_integer_literal() {
        let mutants = find("def f():\n    return 0x1F\n");
        assert!(mutants.is_empty());
    }

    #[test]
    fn skips_an_underscore_grouped_integer_literal() {
        let mutants = find("def f():\n    return 1_000\n");
        assert!(mutants.is_empty());
    }

    #[test]
    fn candidate_lines_span_a_multiline_if_condition_not_just_the_operator_line() {
        let source = "def f(a, b, c, d):\n    if (a == b\n            and c > d):\n        return 1\n    return 0\n";
        let mutants = find(source);
        // Two comparisons, one boolean connective, two return-value increments.
        assert_eq!(mutants.len(), 5);
        let condition_mutants = mutants.iter().filter(|m| !m.id.contains("INCREMENT"));
        for m in condition_mutants {
            // Never narrower than the enclosing if-statement (line 2..4).
            assert_eq!(*m.candidate_lines.start(), 2);
            assert!(*m.candidate_lines.end() >= 3);
        }
    }

    #[test]
    fn candidate_lines_for_a_single_line_statement_is_just_that_line() {
        let source = "def f(a, b):\n    return a == b\n";
        let mutants = find(source);
        assert_eq!(mutants.len(), 1);
        assert_eq!(mutants[0].candidate_lines, 2..=2);
    }

    #[test]
    fn mutant_ids_are_unique_within_a_file() {
        let source = "def f(a, b):\n    x = a == b\n    y = a != b\n    return x and y\n";
        let mutants = find(source);
        let mut ids: Vec<&str> = mutants.iter().map(|m| m.id.as_str()).collect();
        let unique_count = {
            ids.sort_unstable();
            ids.dedup();
            ids.len()
        };
        assert_eq!(unique_count, mutants.len());
    }
}
