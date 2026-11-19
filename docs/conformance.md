# RunCMake conformance

The scoreboard measures a staged `cmake-rust` executable against Kitware's
unmodified RunCMake drivers on Linux x86-64 with Ninja. The reference is a clean
Kitware CMake 4.1.0 checkout at `2a2c2e0b2658f7c07e00fcf649882fd5578c2fc1` and
a Ninja build directory configured with `BUILD_TESTING=ON`. Use a reference
build with its test helper executables built for the broadest host coverage.

## Run the harness

Requirements include Python 3.11+, Ninja, the native compiler tools used to
configure the reference build, and strace with `--kill-on-exit`. The runner
prefers Kitware `ctest` and `cpack` beside the oracle executable, falls back
to host executables, and records their versions. Use `--ctest` and `--cpack`
to select them explicitly. Kitware CTest supplies the inventory and can run
tests for mixed suites that use it as a helper.

```sh
kitware=/path/to/kitware
oracle_build=/path/to/kitware-build
evidence=/path/to/conformance-evidence

KITWARE_SOURCE="$kitware" scripts/conformance/stage-candidate.sh \
  /path/to/rust-cmake "$evidence/staged-candidate"

scripts/conformance/run.py --kitware "$kitware" --oracle-build "$oracle_build" \
  --candidate "$evidence/staged-candidate/bin/cmake" \
  --out "$evidence/baseline" --oracle-cache "$evidence/oracle-cache" \
  --official --repeat 2
```

Staging copies the executable and links `share/cmake-4.1/Modules` and
`share/cmake-4.1/Templates` to the pinned checkout. Use fresh directories for
the stage and for each scored run. The default paths for `--kitware` and
`--oracle-build` point at the supplied validation trees under
`/tmp/cmake-rust-validation.hSLgEZHR`. The runner does not build Rust or CMake.

Omit `--official` for development. A development run defaults to one
repetition and accepts repeatable exact suite selectors such as
`--suite RunCMake.CommandLine`. There is no per-case selector. Use
`--no-audit` to avoid strace overhead in development. All runs use up to six
suite workers by default and set `CMAKE_BUILD_PARALLEL_LEVEL=2`; change the
worker count with `-j`. The default timeout is 240 seconds for each
suite, executable, and repetition. Set it with `--timeout`.

`--oracle-cache` reuses oracle logs by suite and repetition. Its key covers the
oracle SHA-256, checkout, build configuration, available registered helpers,
inventory, harness and baseline hashes, tool versions and hashes, host, audit
configuration, and timeout. Cached logs are copied into the new evidence
directory with the original commands and environment. Use a fresh cache after
changes to host dependencies beyond those recorded in the key.

To check the harness itself, stage a separate copy of `$oracle_build/bin/cmake`
and run with `--self-test`, supplying the staged copy as `--candidate`. The
runner checks that its bytes match the oracle and labels the result as a
harness self-test. Select about ten suites to keep this calibration quick.

```sh
scripts/conformance/run.py --compare \
  "$evidence/baseline/report.json" "$evidence/next/report.json"
```

Comparison uses suite name, upstream case label, and occurrence index. It lists
pass-to-nonpass regressions and newly passing cases common to both
denominators, with a separate count of additions and removals in coverage.

## What counts

- The inventory comes from `ctest --show-only=json-v1`. Every registered
  `RunCMake.*` suite is listed. Upstream CTest registration already applies
  build-time platform and dependency conditions.
- Each in-scope registration runs the upstream driver using the oracle's
  `cmake -P` and sets `CMAKE_COMMAND` to the executable under test. Each
  side and repetition has a temporary build tree, home, and temp directory.
  The runner preserves paths relative to the registered build tree and leaves
  `RunCMake_TEST_FILTER` unset.
- A case is an upstream `PASSED` or `FAILED` result announcement, including
  its variant description. The driver trace identifies announcements from the
  RunCMake implementation and its test helpers, and captures case output.
  Repeated labels are indexed in emission order. Upstream checks which emit
  no case verdict appear in suite-level results and logs.
- A scored case passed on the oracle in every repetition under the lane's
  generator scope. A candidate pass requires matching upstream verdicts
  across repetitions and, for audited runs, a valid execution audit. Missing
  results, failed checks, candidate nondeterminism, and unverifiable candidate
  execution count against the score. Repetition checks compare verdicts and
  presence. A change in text that leaves the verdict unchanged does not
  change the score.
- strace records subprocess execs. The candidate suite must execute the
  staged binary and must not successfully execute another CMake beyond
  the one exact oracle driver invocation. The audit recognizes canonical
  paths and CMake executable names. Unparsed successful exec records
  invalidate the audit. It is a suite-level process provenance check.

## Coverage buckets

| Bucket | Why it is separate |
| --- | --- |
| Out-of-scope standalone tool | `CPack*`, `CTest*`, `ctest_*`, GUI suites, `WorkingDirectory`, and the CPack/CTest preset suites primarily exercise separate executables. The report lists each excluded registration and its justification. In mixed suites, direct standalone scripts invoked through `RunCTest.cmake` and CTest's `--build-and-test` command are counted separately. CTest checks on artifacts generated by the candidate remain in scope. |
| Other generator | The registered driver starts with `-G Ninja`. The oracle driver's generator overrides and explicit requests for other known generators are recorded separately, including commands in audited subprocess traces. Builds reusing a recorded case working directory inherit its generator scope. Unregistered platform and generator branches stay outside the inventory. |
| Oracle failure | Oracle failures, timeouts, audit issues, missing helpers, and suites with no emitted case verdicts are exposed separately. In a partially failing suite, cases that consistently pass the oracle still count. Oracle nondeterminism is recorded separately at the case level and excluded from the denominator. |
| Unexpected case or unselected suite | Candidate-only verdicts have no reference result. In a development subset, other registered suites remain visible as unselected. |

The JSON report includes hashes, versions, workspace Git HEAD, original CTest
inventory, run commands and environments, per-case observations, and normalized
diagnostic counts. `scoreboard.md` gives the overall and per-suite pass rates
and the 25 most frequent failure reasons. See `logs/<suite>/<side>-<repetition>/`
for the driver streams, execution trace, and case logs. The histogram uses the
first substantive candidate diagnostic when available and falls back to the
upstream assertion or the outcome recorded by the harness. Paths, timings, and
addresses are normalized in the histogram. Candidate failures are data and
still produce a successful harness exit after the evidence is written.
