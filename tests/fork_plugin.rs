//! The `dupdup_fork` pytest plugin, run against a throwaway project. Skipped
//! when no Python 3.11+ with pytest is available.

use std::path::Path;
use std::process::Command;

const PLUGIN: &str = include_str!("../src/mutate/dupdup_fork.py");

/// A command prefix that runs a Python 3.11+ with pytest: the system Python if
/// it has pytest, otherwise one `uv` provides.
fn python() -> Option<Vec<String>> {
    let candidates: [&[&str]; 3] = [
        &["python3"],
        &["python"],
        &["uv", "run", "--no-project", "--with", "pytest", "python"],
    ];
    for candidate in candidates {
        let ok = Command::new(candidate[0])
            .args(&candidate[1..])
            .args([
                "-c",
                "import sys, pytest; assert sys.version_info >= (3, 11)",
            ])
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            return Some(candidate.iter().map(|s| (*s).to_string()).collect());
        }
    }
    None
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn each_mutant_runs_in_a_fork_and_the_failing_tests_are_its_killers() {
    let Some(python) = python() else {
        eprintln!("skipped: needs Python 3.11+ with pytest");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let rules = "def is_adult(age):\n    return age >= 18\n\n\ndef clamp(x, lo, hi):\n    if x < lo:\n        return lo\n    if x > hi:\n        return hi\n    return x\n";
    write(&root.join("rules.py"), rules);
    write(
        &root.join("test_rules.py"),
        "from rules import clamp, is_adult\n\n\ndef test_adult():\n    assert is_adult(18) is True\n\n\ndef test_minor():\n    assert is_adult(17) is False\n\n\ndef test_clamp_high():\n    assert clamp(11, 0, 10) == 10\n\n\ndef test_clamp_mid():\n    assert clamp(5, 0, 10) == 5\n",
    );
    write(&root.join("dupdup_fork.py"), PLUGIN);
    let at = |needle: &str| rules.find(needle).unwrap();
    let ge = at(">= 18");
    let lt = at("x < lo") + 2;
    let gt = at("x > hi") + 2;
    let jobs = serde_json::json!({"mutants": [
        {"id": "ge_to_gt", "file": root.join("rules.py"), "start": ge, "end": ge + 2,
         "replacement": "> ", "tests": ["test_rules.py::test_adult", "test_rules.py::test_minor"]},
        {"id": "lt_to_le_equivalent", "file": root.join("rules.py"), "start": lt, "end": lt + 1,
         "replacement": "<=", "tests": ["test_rules.py::test_clamp_mid"]},
        {"id": "gt_to_lt", "file": root.join("rules.py"), "start": gt, "end": gt + 1,
         "replacement": "<", "tests": ["test_rules.py::test_clamp_high", "test_rules.py::test_clamp_mid"]},
    ]});
    write(&root.join("jobs.json"), &jobs.to_string());

    let status = Command::new(&python[0])
        .args(&python[1..])
        .current_dir(root)
        .env("PYTHONPATH", root)
        .args([
            "-m",
            "pytest",
            "-q",
            "-p",
            "dupdup_fork",
            "-p",
            "no:cacheprovider",
        ])
        .args(["--dupdup-jobs", "jobs.json", "--dupdup-out", "out.json"])
        .args(["--dupdup-workers", "2", "test_rules.py"])
        .output()
        .unwrap();
    assert!(
        std::fs::metadata(root.join("out.json")).is_ok(),
        "no result: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let results: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(root.join("out.json")).unwrap()).unwrap();
    let by_id = |id: &str| results.iter().find(|r| r["id"] == id).unwrap().clone();

    let ge_to_gt = by_id("ge_to_gt");
    assert_eq!(ge_to_gt["outcome"], "killed");
    assert_eq!(
        ge_to_gt["killers"],
        serde_json::json!(["test_rules.py::test_adult"])
    );
    assert_eq!(by_id("lt_to_le_equivalent")["outcome"], "survived");
    let gt_to_lt = by_id("gt_to_lt");
    assert_eq!(gt_to_lt["outcome"], "killed");
    let killers: Vec<&str> = gt_to_lt["killers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap())
        .collect();
    assert!(killers.contains(&"test_rules.py::test_clamp_high"));
    // No result lists a test twice.
    for r in &results {
        let e = r["evaluated"].as_array().unwrap();
        let unique: std::collections::HashSet<_> = e.iter().collect();
        assert_eq!(unique.len(), e.len(), "{r}");
    }
}
