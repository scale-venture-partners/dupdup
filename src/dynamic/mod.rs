//! Loaders for the dynamic (execution-based) redundancy tier. Each loader
//! turns a different data source into the same `redundancy::TestCoverage`
//! shape, so `redundancy::analyze` can run over either — but never both at
//! once, since a coverage line-index and a mutant-id index mean different
//! things at the same bit position.

/// Loads `coverage json --show-contexts` output.
pub mod coverage;
/// Loads a pytest `--junitxml` report, for the coverage-tier context check.
pub mod junit;
/// Loads a test-to-killed-mutant kill matrix (input contract only — no
/// upstream producer exists yet).
pub mod killmatrix;

/// Folds what each test asserts into its requirement set.
pub mod assertions;
