# dupdup

Finds duplicative, redundant, and useless tests. The counterpart to a
mutation-testing tool that grows a suite (more tests to kill more mutants):
dupdup prunes it.

It has two independent tiers, because the techniques that actually find
redundant tests split cleanly along one line — whether they need to execute
the suite:

## Install

With a Rust toolchain, from a checkout:

```bash
cargo install --path .
```

That puts `dupdup` in `~/.cargo/bin`, which must be on your `PATH`.

Without Rust: this repo also builds as a Python package via
[maturin](https://www.maturin.rs)'s `bindings = "bin"` mode, which just
wraps the compiled binary in a wheel — there's no Python code here, and
`import dupdup` doesn't work. From a checkout with a Rust toolchain and
[uv](https://docs.astral.sh/uv/):

```bash
uvx maturin build --release
uvx --from target/wheels/dupdup-*.whl dupdup check --all
```

No package is published yet, so this only works from a local build for now
— `uvx dupdup` (pulling from an index) isn't available.

## `dupdup check` — static, no execution

Fast, tree-sitter-based AST rules over Python test files. Pre-commit
friendly, same shape as [windbag](https://github.com/scale-venture-partners/windbag).

| Rule | Severity | Flags |
|---|---|---|
| `NO_ASSERTIONS` | error | A test with no `assert`, no `mock.assert_*(...)`/`self.assertX(...)`/shared `_assert_helper(...)`/`pytest.fail(...)` call, no `pytest.skip(...)` call, and no `pytest.raises`/`assertRaises` context — it can't fail (or self-report as skipped) regardless of what the code does. |
| `TAUTOLOGICAL_ASSERT` | error | `assert x == x` where neither side contains a call — both sides are the same expression, so the assertion is true independent of the code under test. `assert f(x) == f(x)` is excluded: that's testing that `f` is deterministic, not a tautology. |
| `DUPLICATE_ASSERT` | warn | Two `assert` statements that are true source-adjacent siblings (nothing else between them) with identical (whitespace-normalized) text. Not flagged if anything sits between them — re-checking an unchanged invariant after a further action is normal test structure, not duplication. |

Calibrated against a real ~130-file production Python test suite: the first
pass (before the exemptions above existed) produced 76 findings; 61 of those
were false positives from exactly the patterns the exemptions now cover —
Mock/unittest/local-helper assertion calls, `f(x) == f(x)` determinism
checks, and re-verifying an invariant after a state-changing action several
lines later. The remaining 15 are almost all one pattern this tier doesn't
yet resolve: a test whose only check is that a call doesn't raise (e.g.
verifying exception-swallowing/resilience code) has no assertion of any
kind and is indistinguishable, to a static rule, from a test that simply
forgot to assert something.

```bash
dupdup init             # write dupdup.toml with defaults
dupdup check --staged   # check what's about to be committed
dupdup check --all      # check every git-tracked test file
dupdup check --json --staged
```

A `# dupdup: ignore[RULE]` comment on the flagged line (or the line above)
suppresses it, same placement as windbag.

Only pytest's own discovery convention is checked: functions named `test_*`,
in files named `test_*.py` or `*_test.py` for `--staged`/`--all` (an
explicit path is checked regardless of its name), and never a function
nested inside another function — pytest doesn't discover those either,
regardless of name.

## `dupdup analyze` — dynamic, requires running the suite

Coverage- and mutation-based redundancy analysis, driven by one shared
engine (`src/redundancy`) over `(test_id, requirement_bitset)` pairs. A
requirement is a covered source line for the coverage source, or a killed
mutant id for the mutation source — the two are never mixed into one
analysis, since a bit position means something different in each space.

Four relations, per source:

- `COVERS_NOTHING` — a test covers zero requirements (zero lines for the
  coverage source, zero killed mutants for the mutation source). For the
  mutation source this is the single most actionable finding: a test that
  kills no mutant at all is redundant regardless of what else is in the
  suite. Reported once, not once per other test — an empty set is trivially
  a "subset" of everything, so without this check it would otherwise show
  up as a flood of `SUBSUMED_COVERAGE` findings against every peer.
- `DUPLICATE_COVERAGE` — two tests cover exactly the same requirements.
- `SUBSUMED_COVERAGE` — a test's coverage is a strict subset of another
  single test's.
- `CONTRIBUTES_NOTHING_UNIQUE` — a test's coverage is fully reproduced by
  the union of every other test, even though no single other test subsumes
  it alone (the "god test" case, in reverse).

Tests that only differ by a pytest `parametrize` id (`test_foo[1]` vs.
`test_foo[2]`) are exempted from these relations against each other —
sharing coverage is the point of parametrizing, not evidence of redundancy.
That exemption is scoped to the sibling pair itself: `test_foo[1]` can
still be flagged against an unrelated test, e.g. `test_bar`.

**These relations are computed per test independently.** If two tests each
happen to be reproducible by the rest of the suite, that does not mean both
are safe to delete together — removing both could drop real coverage. This
is the same caveat as classic test-suite-minimization literature: computing
a jointly minimal safe-to-remove set is a harder (NP-complete in general)
set-cover problem that dupdup doesn't attempt. Treat findings as leads to
review, not a script to run.

### Coverage source

```bash
COVERAGE_CORE=ctrace python -m pytest --cov=mypkg --cov-context=test --junitxml=junit.xml
python -m coverage json --show-contexts -o coverage.json
dupdup analyze --coverage-json coverage.json --junit-xml junit.xml
```

`COVERAGE_CORE=ctrace` is required on Python 3.12+: `coverage.py`'s default
`sysmon` core silently drops dynamic-context data for some tests instead of
erroring. `--junit-xml` lets dupdup cross-check the number of tests that
actually ran (passed + failed, per the junit report) against the number it
found context data for in `coverage.json`, and refuse to analyze on a
mismatch rather than silently reporting on a partial suite. Passing
`--coverage-json` without `--junit-xml` still works, with a warning that
this check is skipped.

### Mutation source

```bash
dupdup analyze --kill-matrix-json kills.json
```

where `kills.json` is `{"tests": {"<test id>": ["<mutant id>", ...]}}` — a
test-to-killed-mutants map, i.e. a mutation-testing kill matrix with the
tests attributed per mutant, not just an overall survive/kill count.

**No producer for this exists yet.** mutmut only records whether a mutant
was killed at all, not which test did it — its own maintainers flag this
gap (`# TODO: we should be able to get information on which tests killed
mutants ... those that kill zero mutants are redundant!` in
`mutmut/__main__.py`). This loader defines the input contract so the
redundancy engine already has a consumer once a collector exists (either an
upstream mutmut feature, or a small pytest-cov-context-per-mutant-run
collector built alongside dupdup) — analyzing coverage alone doesn't need
it.

## Why two tiers, not one

The literature splits the same way: `pytest_deduplicate`'s own
duplicate/superset/subset detection, classic coverage-based suite
minimization, and mutation-subsumption pruning are all necessarily dynamic
— they need a real run. Empty tests, tautological/duplicate asserts, and
(planned) structural clone detection for parametrize candidates are static
— pure AST pattern-matching, same as windbag's comment rules. Folding both
into one command would mean either making `check` slow (defeating the
pre-commit use case) or making `analyze` pretend it doesn't need a real
run (masking a failure). They're built to compose instead: run `check` on
every commit, run `analyze` in CI against a real coverage/mutation run.

## Planned, not yet built

- Structural clone detection for parametrize candidates (fingerprint
  AST-normalized test bodies, flag near-identical siblings that differ only
  in literals).
- Mock-only-assertion detection (every assertion in a test traces back to a
  `Mock()`/`patch()` object the test itself configured — a different
  question from whether a `mock.assert_*()` call counts as an assertion at
  all, which `NO_ASSERTIONS` already handles).
- A `NO_ASSERTIONS` exemption for a body whose only checks are that a call
  doesn't raise — the single largest remaining false-positive class found
  calibrating against a real suite (see above), still unresolved because a
  safe heuristic (e.g. requiring a `side_effect=`-injecting `patch(...)` in
  scope) hasn't been validated against enough real cases yet.
- A kill-matrix collector, once the mutation-tier producer question above is
  resolved.

## Development

```bash
cargo test               # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs all three on every push and pull request.
