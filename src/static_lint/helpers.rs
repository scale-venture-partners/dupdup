//! Which function names assert something, directly or through another
//! asserting function, so a test that delegates to `check_total(order)` isn't
//! mistaken for a test with no assertion.

use super::{parse, rules};
use std::collections::{HashMap, HashSet};
use tree_sitter::Node;

/// Names of functions known to assert. Matching is by unqualified name, so
/// two unrelated functions sharing a name are treated as one.
#[derive(Debug, Default, Clone)]
pub struct AssertingHelpers {
    names: HashSet<String>,
}

#[derive(Default)]
struct Entry {
    direct: bool,
    calls: Vec<String>,
}

impl AssertingHelpers {
    /// Indexes every function in `sources` (any file, not just test files,
    /// so helpers in `conftest.py` or a shared module count). A function
    /// asserts if it does directly or calls one that does; this resolves to
    /// a fixed point, so a helper that delegates to a helper is covered.
    /// Names starting with `test_` are never helpers. A source that fails
    /// to parse is skipped.
    #[must_use]
    pub fn build<'a>(sources: impl IntoIterator<Item = &'a str>, extra: &[String]) -> Self {
        let mut entries: HashMap<String, Entry> = HashMap::new();
        for source in sources {
            let Ok(tree) = parse(source) else { continue };
            collect(tree.root_node(), source.as_bytes(), &mut entries);
        }

        let mut names: HashSet<String> = extra.iter().cloned().collect();
        names.extend(
            entries
                .iter()
                .filter(|(_, e)| e.direct)
                .map(|(n, _)| n.clone()),
        );
        loop {
            let before = names.len();
            for (name, entry) in &entries {
                if !names.contains(name) && entry.calls.iter().any(|c| names.contains(c)) {
                    names.insert(name.clone());
                }
            }
            if names.len() == before {
                break;
            }
        }
        Self { names }
    }

    /// Whether a call to `name` counts as an assertion.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

/// Indexes every tracked `.py` file in the git repo at `root`, plus the
/// `extra` names.
///
/// # Errors
/// Returns an error if `root` isn't a git repository.
pub fn index_repo(root: &std::path::Path, extra: &[String]) -> anyhow::Result<AssertingHelpers> {
    // Helpers can live in any tracked Python file (conftest.py, a shared
    // module), not only test files, so index every one of them.
    let sources: Vec<String> = crate::git::all_files(root)?
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "py"))
        .filter_map(|p| std::fs::read_to_string(root.join(p)).ok())
        .collect();
    Ok(AssertingHelpers::build(
        sources.iter().map(String::as_str),
        extra,
    ))
}

fn collect(node: Node, source: &[u8], entries: &mut HashMap<String, Entry>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "function_definition" {
            if let Some(name) = child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .filter(|n| !n.starts_with("test_"))
            {
                let entry = entries.entry(name.to_string()).or_default();
                entry.direct |= rules::asserts_directly(child, source);
                entry.calls.extend(rules::called_names(child, source));
            }
        }
        collect(child, source, entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(src: &str) -> AssertingHelpers {
        AssertingHelpers::build([src], &[])
    }

    #[test]
    fn a_function_with_an_assert_is_a_helper() {
        let h = build("def check(x):\n    assert x\n");
        assert!(h.contains("check"));
    }

    #[test]
    fn delegation_to_a_helper_makes_a_helper() {
        let h = build("def inner(x):\n    assert x\n\ndef outer(x):\n    inner(x)\n");
        assert!(h.contains("outer"));
    }

    #[test]
    fn a_function_that_only_calls_unknown_things_is_not_a_helper() {
        let h = build("def setup(x):\n    make(x)\n");
        assert!(!h.contains("setup"));
    }

    #[test]
    fn test_functions_are_never_helpers() {
        let h = build("def test_a():\n    assert 1\n");
        assert!(!h.contains("test_a"));
    }

    #[test]
    fn a_pytest_raises_context_makes_a_helper() {
        let h = build("def refuses(x):\n    with pytest.raises(ValueError):\n        f(x)\n");
        assert!(h.contains("refuses"));
    }

    #[test]
    fn extra_names_are_helpers() {
        let h = AssertingHelpers::build(["def f():\n    pass\n"], &["verify".to_string()]);
        assert!(h.contains("verify"));
    }
}
