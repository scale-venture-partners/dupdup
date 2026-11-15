//! A merged `parametrize` for each group of tests `CLONE_TEST` finds.

use super::{find_test_functions, parse, rules, test_name};
use std::collections::BTreeMap;
use tree_sitter::Node;

/// The most differing literals a suggestion will turn into parameters.
const MAX_PARAMS: usize = 6;

/// A suggested rewrite of one clone group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// The line of the group's first test, where `CLONE_TEST` is reported.
    pub line: usize,
    /// The suggested replacement, ready to paste.
    pub text: String,
}

struct Literal {
    start: usize,
    end: usize,
    text: String,
}

/// The string and number literals in a test body, in source order, skipping a
/// leading docstring. These are the positions `clone_shape` masks.
fn literals(fn_node: Node, source: &[u8]) -> Vec<Literal> {
    fn walk(node: Node, source: &[u8], out: &mut Vec<Literal>) {
        match node.kind() {
            "comment" => {}
            "string" | "integer" | "float" | "concatenated_string" => out.push(Literal {
                start: node.start_byte(),
                end: node.end_byte(),
                text: node.utf8_text(source).unwrap_or("").to_string(),
            }),
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk(child, source, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    let Some(body) = fn_node.child_by_field_name("body") else {
        return out;
    };
    let mut cursor = body.walk();
    let mut statements: Vec<Node> = body.children(&mut cursor).collect();
    if statements.first().is_some_and(|s| {
        s.kind() == "expression_statement" && s.child(0).is_some_and(|c| c.kind() == "string")
    }) {
        statements.remove(0);
    }
    for s in statements {
        walk(s, source, &mut out);
    }
    out
}

struct Member<'a> {
    name: String,
    line: usize,
    /// The definition with its decorators.
    whole: Node<'a>,
    function: Node<'a>,
    literals: Vec<Literal>,
}

/// Suggestions for every clone group in `source`, in source order.
///
/// # Errors
/// Returns an error if `source` can't be parsed as Python.
pub fn suggestions(source: &str) -> anyhow::Result<Vec<Suggestion>> {
    let tree = parse(source)?;
    let bytes = source.as_bytes();
    let mut groups: BTreeMap<String, Vec<Member>> = BTreeMap::new();
    for function in find_test_functions(tree.root_node(), bytes) {
        let Some(shape) = rules::clone_shape(function, bytes) else {
            continue;
        };
        let whole = function
            .parent()
            .filter(|p| p.kind() == "decorated_definition")
            .unwrap_or(function);
        groups.entry(shape).or_default().push(Member {
            name: test_name(function, bytes).to_string(),
            line: function.start_position().row + 1,
            whole,
            function,
            literals: literals(function, bytes),
        });
    }
    let mut out: Vec<Suggestion> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort_by_key(|m| m.line);
            Suggestion {
                line: g[0].line,
                text: render(&g, source),
            }
        })
        .collect();
    out.sort_by_key(|s| s.line);
    Ok(out)
}

fn render(group: &[Member], source: &str) -> String {
    let first = &group[0];
    let count = first.literals.len();
    let differing: Vec<usize> = (0..count)
        .filter(|&i| {
            group.iter().any(|m| {
                m.literals.get(i).map(|l| &l.text) != first.literals.get(i).map(|l| &l.text)
            })
        })
        .collect();
    if differing.is_empty() {
        let others: Vec<&str> = group[1..].iter().map(|m| m.name.as_str()).collect();
        return format!(
            "{} is identical to {} apart from its name: keep one.",
            first.name,
            others.join(", ")
        );
    }
    if differing.len() > MAX_PARAMS {
        return format!(
            "{} literals differ across these {} tests: too many to turn into parameters usefully.",
            differing.len(),
            group.len()
        );
    }

    let args: Vec<String> = (0..differing.len()).map(|i| format!("arg{i}")).collect();
    let indent = " ".repeat(first.whole.start_position().column);
    let base = first.whole.start_byte();
    let mut text = source[base..first.whole.end_byte()].to_string();

    // Edits, applied from the end so earlier offsets stay valid: the
    // differing literals become argument names, and the arguments are added
    // to the signature.
    let mut edits: Vec<(usize, usize, String)> = differing
        .iter()
        .zip(&args)
        .map(|(&i, arg)| {
            let l = &first.literals[i];
            (l.start - base, l.end - base, arg.clone())
        })
        .collect();
    if let Some(params) = first.function.child_by_field_name("parameters") {
        let close = params.end_byte() - 1 - base;
        let has_params = params.named_child_count() > 0;
        let added = args.join(", ");
        edits.push((
            close,
            close,
            if has_params {
                format!(", {added}")
            } else {
                added
            },
        ));
    }
    edits.sort_by(|a, b| b.0.cmp(&a.0));
    for (s, e, r) in edits {
        text.replace_range(s..e, &r);
    }

    let names = if args.len() == 1 {
        format!("\"{}\"", args[0])
    } else {
        format!(
            "({})",
            args.iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    // pytest rejects duplicate ids, and two tests in different classes can
    // share a name.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let rows: Vec<String> = group
        .iter()
        .map(|m| {
            let base_id = m.name.trim_start_matches("test_").to_string();
            let n = seen.entry(base_id.clone()).or_insert(0);
            *n += 1;
            let id = if *n == 1 {
                base_id
            } else {
                format!("{base_id}_{n}")
            };
            let values: Vec<&str> = differing
                .iter()
                .map(|&i| m.literals.get(i).map_or("None", |l| l.text.as_str()))
                .collect();
            format!(
                "{indent}        pytest.param({}, id=\"{}\"),",
                values.join(", "),
                id
            )
        })
        .collect();
    format!(
        "{indent}@pytest.mark.parametrize(\n{indent}    {names},\n{indent}    [\n{}\n{indent}    ],\n{indent})\n{indent}{text}",
        rows.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "def test_empty():\n    r = f('', 1)\n    assert r == 2\n\ndef test_missing():\n    r = f('x', 3)\n    assert r == 4\n";

    #[test]
    fn a_group_that_differs_in_literals_gets_a_parametrize() {
        let s = suggestions(TWO).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].line, 1);
        let t = &s[0].text;
        assert!(t.contains("@pytest.mark.parametrize("), "{t}");
        assert!(t.contains("(\"arg0\", \"arg1\", \"arg2\")"), "{t}");
        assert!(t.contains("pytest.param('', 1, 2, id=\"empty\")"), "{t}");
        assert!(t.contains("pytest.param('x', 3, 4, id=\"missing\")"), "{t}");
        assert!(t.contains("def test_empty(arg0, arg1, arg2):"), "{t}");
        assert!(
            t.contains("r = f(arg0, arg1)") && t.contains("assert r == arg2"),
            "{t}"
        );
    }

    #[test]
    fn only_the_literals_that_differ_become_parameters() {
        let source = "def test_a():\n    r = f('same', 1)\n    assert r == 2\n\ndef test_b():\n    r = f('same', 5)\n    assert r == 2\n";
        let t = &suggestions(source).unwrap()[0].text;
        assert!(t.contains("\"arg0\""), "{t}");
        assert!(!t.contains("arg1"), "{t}");
        assert!(t.contains("f('same', arg0)"), "{t}");
    }

    #[test]
    fn existing_arguments_and_class_indentation_are_kept() {
        let source = "class TestC:\n    def test_a(self, tmp_path):\n        r = f(tmp_path, 1)\n        assert r == 2\n\n    def test_b(self, tmp_path):\n        r = f(tmp_path, 3)\n        assert r == 2\n";
        let t = &suggestions(source).unwrap()[0].text;
        assert!(t.starts_with("    @pytest.mark.parametrize("), "{t}");
        assert!(t.contains("    def test_a(self, tmp_path, arg0):"), "{t}");
    }

    #[test]
    fn decorators_are_kept_after_the_parametrize() {
        let source = "@pytest.mark.unit\ndef test_a():\n    r = f(1)\n    assert r == 2\n\n@pytest.mark.unit\ndef test_b():\n    r = f(3)\n    assert r == 2\n";
        let t = &suggestions(source).unwrap()[0].text;
        let param = t.find("@pytest.mark.parametrize(").unwrap();
        let unit = t.find("@pytest.mark.unit").unwrap();
        assert!(param < unit, "{t}");
    }

    #[test]
    fn identical_bodies_say_to_keep_one() {
        let source = "def test_a():\n    r = f(1)\n    assert r == 2\n\ndef test_b():\n    r = f(1)\n    assert r == 2\n";
        let t = &suggestions(source).unwrap()[0].text;
        assert!(t.contains("identical") && t.contains("keep one"), "{t}");
    }

    #[test]
    fn too_many_differing_literals_are_not_turned_into_parameters() {
        let a = "def test_a():\n    f(1, 2, 3, 4, 5, 6, 7)\n    assert f(1)\n";
        let b = "def test_b():\n    f(8, 9, 10, 11, 12, 13, 14)\n    assert f(1)\n";
        let t = &suggestions(&format!("{a}\n{b}")).unwrap()[0].text;
        assert!(t.contains("too many"), "{t}");
    }

    #[test]
    fn same_named_tests_get_distinct_ids() {
        let source = "class A:\n    def test_x(self):\n        r = f(1)\n        assert r == 2\n\nclass B:\n    def test_x(self):\n        r = f(3)\n        assert r == 2\n";
        let t = &suggestions(source).unwrap()[0].text;
        assert!(t.contains("id=\"x\"") && t.contains("id=\"x_2\""), "{t}");
    }

    #[test]
    fn a_unique_test_gets_no_suggestion() {
        let source = "def test_a():\n    r = f(1)\n    assert r == 2\n\ndef test_b():\n    r = g(1)\n    assert r == 2\n";
        assert!(suggestions(source).unwrap().is_empty());
    }
}
