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
/// Methods on unittest-style `Test*` classes are picked up too, since this
/// walk doesn't distinguish nesting depth.
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
        }
        collect_test_functions(child, source, out);
    }
}

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
        text.contains("dupdup: ignore") && (!text.contains('[') || text.contains(&format!("[{rule}]")))
    };
    let idx = line_1indexed.saturating_sub(1);
    lines.get(idx).map(|l| check(l)).unwrap_or(false)
        || idx.checked_sub(1).and_then(|i| lines.get(i)).map(|l| check(l)).unwrap_or(false)
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
            v.test_name = name.clone();
        }
        out.extend(found);
    }

    out.retain(|v| !is_suppressed(&lines, v.line, v.rule));
    Ok(out)
}
