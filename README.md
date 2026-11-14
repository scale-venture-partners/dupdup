# dupdup

> **Experimental / work in progress.** dupdup is not production ready. Rules,
> flags, and output formats may change without notice, and the dynamic tier's
> coverage-based findings have a high false-positive rate (see the known
> limitations below). Treat results as leads to review, not as changes to apply.

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
| `NO_ASSERTIONS` | error | A test with no call at all (`pass`, a docstring, `...`) and no assertion — it can never fail. A call to a function that itself asserts (found by indexing every tracked `.py` file, resolved transitively) counts as an assertion, as do `assert`, `mock.assert_*(...)`/`self.assertX(...)`, `pytest.fail(...)`, `pytest.skip(...)` and `pytest.raises`/`assertRaises`. |
| `NO_ASSERTIONS_SMOKE` | warn | A test that makes calls but asserts nothing. It can still fail by raising, so this is a smoke test, not a stub. Disable with `[dupdup.no_assertions_smoke] enabled = false`. An existing `ignore[NO_ASSERTIONS]` comment suppresses it too. |
| `TAUTOLOGICAL_ASSERT` | error | `assert x == x` where neither side contains a call — both sides are the same expression, so the assertion is true independent of the code under test. `assert f(x) == f(x)` is excluded: that's testing that `f` is deterministic, not a tautology. |
| `DUPLICATE_ASSERT` | warn | Two `assert` statements that are true source-adjacent siblings (nothing else between them) with identical (whitespace-normalized) text. Not flagged when the assert contains a call: repeating it re-invokes the call, which checks idempotence or caching rather than duplicating a check. Not flagged if anything sits between them — re-checking an unchanged invariant after a further action is normal test structure, not duplication. |

Calibrated against a real ~130-file production Python test suite: the first
pass (before the exemptions above existed) produced 76 findings; 61 of those
were false positives from exactly the patterns the exemptions now cover —
Mock/unittest/local-helper assertion calls, `f(x) == f(x)` determinism
checks, and re-verifying an invariant after a state-changing action several
lines later. The remaining 15 were almost all tests whose only check is
that a call doesn't raise; those are now `NO_ASSERTIONS_SMOKE` warnings
instead of errors, since a static rule can't tell them from a test that
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

`--staged`/`--all` read the repository via [`gix`](https://github.com/GitoxideLabs/gitoxide),
a pure-Rust git implementation — dupdup never shells out to a `git` binary,
so it has no runtime dependency on one being installed.

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

`DUPLICATE_COVERAGE`, `SUBSUMED_COVERAGE`, and `CONTRIBUTES_NOTHING_UNIQUE`
only ever compare two tests in the **same test file**, when both test ids
are actual pytest node ids (contain `::`) — the mutation source's kill
matrix isn't guaranteed to key by pytest node id, so an id set that doesn't
look like one falls back to comparing every pair rather than this scoping
silently zeroing out every relation. Calibrating against
a real ~2000-test production suite found that essentially every cross-file
pairing was two tests from unrelated feature areas that happened to share
coverage of common infrastructure (middleware, logging, a shared client) —
not genuine redundancy. Acting on those findings would have meant deleting
the only direct test of whichever behavior got flagged as "subsumed." This
scoping cut total findings by 80% on that suite (17,115 → 3,340) and made
the tool noticeably faster, since same-file filtering skips most pairs
before any bitset comparison runs.

**These relations are computed per test independently.** If two tests each
happen to be reproducible by the rest of the suite, that does not mean both
are safe to delete together — removing both could drop real coverage. This
is the same caveat as classic test-suite-minimization literature: computing
a jointly minimal safe-to-remove set is a harder (NP-complete in general)
set-cover problem that dupdup doesn't attempt. Treat findings as leads to
review, not a script to run.

**Known limitation, still open after same-file scoping: the coverage
source's precision, not just its volume, is limited by what line coverage
can see.** Re-verifying after the same-file fix, a random sample of new
findings was still ~6/7 false positives — same-file scoping removed the
most dangerous failure mode (cross-feature-area false positives) but not
the underlying one: **line coverage has no visibility into what a test
asserted, only what code ran.** Concretely, on that same real suite:

- Two tests calling the same low-branching function with *different*
  assertions about its result show identical coverage — not just
  loop/boundary-condition functions, but any function most of whose logic
  isn't conditional on the input two tests happen to differ on.
- Two tests sharing setup/constructor code, where a "small" test only
  exercises that shared baseline and a "big" test exercises it plus more,
  look like subset/superset even when they're testing unrelated things —
  the same shape as the fixed cross-file case, just within one file.
  Same-file scoping was necessary but wasn't sufficient to catch this.
- A success-path test and its `pytest.raises(...)` failure-path sibling for
  Pydantic-v2-validated input showed identical coverage, because the actual
  branching happens inside pydantic-core's compiled Rust validator —
  invisible to Python line tracing. This is the same root cause as a
  compiled-extension test touching no traceable line at all (see the
  `--junit-xml` section below); here it just doesn't fully vanish, it goes
  invisible to *which* lines varied instead.

None of this is fixable by tightening the scoping further — it needs either
per-branch dynamic-context data (see the SQLite-schema tradeoff below,
still unresolved) or cross-checking the tool's own static-tier AST parsing
of each test's actual assert statements before trusting a coverage-based
relation, which isn't built yet. Treat every `DUPLICATE_COVERAGE`/
`SUBSUMED_COVERAGE`/`CONTRIBUTES_NOTHING_UNIQUE` finding as a lead to
manually check against the real test bodies, not a result to act on
directly.

### Coverage source

```bash
COVERAGE_CORE=ctrace python -m pytest --cov=mypkg --cov-context=test --junitxml=junit.xml
python -m coverage json --show-contexts -o coverage.json
dupdup analyze --coverage-json coverage.json --junit-xml junit.xml
```

`COVERAGE_CORE=ctrace` is recommended on Python 3.12+: `coverage.py`'s
default `sysmon` core silently drops dynamic-context data for some tests
instead of erroring. `--junit-xml` lets dupdup cross-check the number of
tests that actually ran (passed + failed, per the junit report) against the
number it found context data for in `coverage.json`, and refuse to analyze
on a mismatch rather than silently reporting on a partial suite. Passing
`--coverage-json` without `--junit-xml` still works, with a warning that
this check is skipped.

Only the `run` phase of each test counts as its coverage by default.
Lines executed by fixtures (the `setup`/`teardown` phases) aren't something
the test body exercises, and a shared fixture makes unrelated tests look
like subsets of each other. Pass `--include-fixtures` to count them anyway.

With `--junit-xml`, each test that ran but whose own body covered no line
of the measured code is reported as `NO_COVERAGE`, identified by its junit
`classname::name` (e.g. `tests.test_a.TestB::test_c`). That includes tests
with no context data at all and tests whose only coverage came from
fixtures. They are left out of the redundancy relations.

A shortfall isn't always the sysmon bug, though — calibrating against a
real suite found a context-count mismatch caused by tests that genuinely
execute no traceable line in the covered package (a pure static-analysis
check, or a test whose only work happens inside a compiled extension like
Pydantic v2's Rust validation core). That's normal in a large real
codebase, not a bug, so the error message names both possibilities instead
of assuming sysmon, and `--allow-incomplete-context` turns the hard error
into a warning once you've confirmed which cause applies.

### Mutation source

```bash
dupdup analyze --kill-matrix-json kills.json
```

where `kills.json` is `{"tests": {"<test id>": ["<mutant id>", ...]}}` — a
test-to-killed-mutants map, i.e. a mutation-testing kill matrix with the
tests attributed per mutant, not just an overall survive/kill count.

`dupdup mutate` produces `kills.json`. mutmut only records whether a mutant
was killed at all, not which test did it — its own maintainers flag this
gap (`# TODO: we should be able to get information on which tests killed
mutants ... those that kill zero mutants are redundant!` in
`mutmut/__main__.py`) — so this is a self-contained collector, not a wrapper
around mutmut:

```bash
COVERAGE_CORE=ctrace python -m pytest --cov=mypkg --cov-context=test
python -m coverage json --show-contexts -o coverage.json
dupdup mutate mypkg/checkout.py mypkg/pricing.py --coverage-json coverage.json > kills.json
dupdup analyze --kill-matrix-json kills.json
```

Mutants come from walking the same tree-sitter AST `static_lint` uses over
each `--source` file (application code, not test files — spelled exactly
as it appears as a key in `coverage.json`'s own `files` map), applying a
small, fixed set of point mutations: comparison-operator flips
(`==`↔`!=`, `<`↔`>=`, ...; a chained comparison like `a < b < c` is
skipped rather than guessing which operator to flip), `and`↔`or`,
arithmetic-operator flips (`+`↔`-`, `*`↔`/`), boolean-literal flips, and
incrementing a plain base-10 integer literal by one (a hex/octal/binary or
underscore-grouped literal is skipped rather than mutated with a guess at
its meaning). Each is one specific replacement, never "try every
alternative" — this keeps the mutant count, and so the number of test
runs, bounded and predictable rather than combinatorial.

For each mutant, only the tests the coverage report says cover its
enclosing statement are run — not the whole suite — which is what makes a
from-scratch collector tractable at all instead of reimplementing
coverage.py's own line tracing. A mutant whose enclosing statement has no
covering test at all is reported separately as "no coverage", never
folded into "survived": those are different claims about the data, and
conflating them would understate a test's value and could feed a false
redundancy finding back into `analyze`. Every candidate test id the
coverage report names is confirmed to actually resolve under pytest
before any file is touched, so a coverage report that's gone stale
relative to the current test suite fails loudly up front rather than
silently mis-crediting a kill mid-run.

Mutating real source in place is inherently risky if the process is
killed mid-run, so `mutate` refuses to start unless every `--source` file
is clean in git first (its on-disk content matches `HEAD` exactly — commit
or stash first), and keeps an on-disk `<file>.dupdup-orig` backup for the
duration of each file's mutants, deleted only once that file's run
finishes normally. A leftover backup from an interrupted prior run blocks
a new run on that file until it's been inspected and cleared by hand.

A mutant that hangs the process it's applied to (e.g. weakening a loop
bound) is killed after `--timeout-secs` (default 30) with no way to tell
"slow" from "infinite" from the outside. Timing out alone, isolated from
every other candidate, counts as a kill — the hang itself is exactly the
behavioral difference the mutant was meant to expose, and it's
unambiguously that one test's doing. A whole-batch timeout is not drilled
down into per-candidate runs to find out which one hung, since that would
cost up to one more full timeout per candidate for every slow mutant; it's
reported as inconclusive and excluded, same as a batch that fails without
any single candidate reproducing the failure alone (a sign of test-order
coupling, not a clean per-test result).

This runs strictly sequentially — one file, one mutant, one test run at a
time — so it's slow on a large `--source` set; scope it to the modules you
actually want mutation coverage of, not a whole codebase at once.
`--pytest-cmd` is split on plain whitespace with no quoting, so a command
whose own arguments need a space (a quoted path, for instance) doesn't
work here.

Verified end to end against a small real project, not just unit tests —
`collect` itself needs a real pytest run, so it isn't unit-tested beyond
its pure helpers (`backup_path`) and the `RestoreOnDrop` safety net,
which is: mutating a module with 5 comparison/arithmetic/boolean/boundary
mutants plus 2 in genuinely untested branches produced exactly 5 kills,
correctly attributed per-test (including one mutant killed by 3 different
tests, confirmed via the batch-then-drill-down attribution), and exactly
2 "no coverage" results matching coverage.py's own uncovered-line count —
not 7 survivors, which is what a line-granularity (rather than
enclosing-statement) candidate lookup produced before that bug was found
and fixed. The same run also found and fixed a second bug: a stale or
nonexistent test id surfaced as an ambiguous pytest exit code read
mid-mutation rather than a clean error — moved to the up-front
`--collect-only` validation pass instead.

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
- Per-branch dynamic-context data for the coverage source (see the known
  limitation above) — needs a real design decision about whether that means
  parsing `coverage.py`'s internal SQLite schema directly, since the public
  JSON report format doesn't expose it even with branch coverage on
  (confirmed empirically).
- Cross-checking a coverage-based finding against the static tier's own AST
  parsing of both tests' assert statements before reporting it — `analyze`
  already has each flagged test's file path for free (it's the pytest node
  id), so this reuses `static_lint`'s existing parser rather than adding a
  new one. Two same-file tests with identical coverage but assert
  statements that check different things is exactly the failure mode found
  calibrating against a real suite; this is the concrete next step for it,
  not yet built or scoped in detail.
- More mutation operators for `dupdup mutate` (`not` deletion, more
  arithmetic/bitwise operators, string/collection literal mutations) —
  deliberately left out of the initial fixed set rather than trying to
  match a general-purpose mutation tool's full operator catalog.
- Parallel mutant execution for `dupdup mutate` — it runs strictly
  sequentially today, which is simple to reason about (only one mutant is
  ever live on disk) but slow on a large `--source` set.

## Development

```bash
cargo test               # unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI runs all three on every push and pull request.
