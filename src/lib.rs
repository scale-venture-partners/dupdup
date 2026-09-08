//! Finds duplicative, redundant, and useless tests: a static AST-based
//! linter (`dupdup check`) plus a dynamic coverage/mutation-based
//! redundancy engine (`dupdup analyze`).
#![warn(missing_docs)]

/// `dupdup.toml` config: which static rules run, and which paths to skip.
pub mod config;
/// Loaders for the dynamic (execution-based) redundancy tier.
pub mod dynamic;
/// Git plumbing for discovering staged/tracked files.
pub mod git;
/// A self-contained mutation-testing kill-matrix collector.
pub mod mutate;
/// Human-readable and JSON rendering of findings.
pub mod output;
/// The shared bitset engine both dynamic-tier loaders feed into.
pub mod redundancy;
/// Static, tree-sitter-based rules over Python test source.
pub mod static_lint;
