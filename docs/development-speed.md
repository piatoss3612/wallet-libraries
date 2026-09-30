# Development speed evidence

Baseline: main `b0e2acf8489881cd99f7f7a07ba77d75e24608fd`, 2026-09-30.
Sources: [runbook observations](https://github.com/p0mvn/ai-runbook/pull/3)
and [representative verification run](https://github.com/zakura-core/wallet-libraries/actions/runs/36704124417).

The recent successful-run sample had 41 runs and approximately 27-minute median
elapsed time. The representative test job spent 354/333/378/140/177 seconds in
its sequential groups; compilation accounted for roughly 13 minutes. These
historical measurements do not establish savings from the new workflow.
Reported one-second tool calls are not total build-lock wait durations.

| Blocker | Change and acceptance | Evidence/status |
| --- | --- | --- |
| Build ownership | Concurrent processes get separate reusable directories; killed owners release OS leases | Python multiprocessing regressions pass; child commands inherit the lease descriptor |
| Empty focused checks | Listing must select at least one test | Positive and empty-selection regressions pass |
| Changing source | Validate HEAD and source-input digest before/after | A simulated changing-input run is invalidated |
| CI serialization | Six parallel feature configurations, caches, final result, PR cancellation | Two superseded revisions cancelled; all six lanes, doctests and repository checks passed at `7f5b6798e34d` |
| Protobuf side effects | Explicit check/write generation; ordinary builds consume checked-in bindings | Pinned comparison passed; deliberate stale binding rejected in 4.95 seconds; ordinary builds with/without protoc leave it unchanged |
| PIR real-time waiting | Private monotonic clock advances during dispatch | Both feature configurations passed CI; 13 cover tests passed locally in 5.74 seconds; three warm commands had a 6.31-second median |
| Fixture friction | Transaction-bound operations and reader-version injection helpers; shared immutable key preparation | Commit/rollback and future-reader refusal/reset regressions pass; three warm focused commands had a 3.07-second median |
| Formatting permissions | Formatting is read-only with `--check`; no compilation or Cargo target lock | Recovered operational event is a rustfmt diff failure, misclassified as permissions; current formatting check passes |
| Iteration profile | Optional profile in Cargo configuration; final test profile preserved | Three interleaved semantic edits forced SQLite recompilation: test median 7.14 seconds, iteration 5.46 seconds; retained opt-in only |

## Measured validation

[Full verification at `7f5b6798e34d`](https://github.com/zakura-core/wallet-libraries/actions/runs/36714784321)
passed all applicable checks. Elapsed time from run creation to completion was
10 minutes 42 seconds; from the first job starting to the final `tests` result
it was 9 minutes 15 seconds. The historical median includes different revisions
and runner conditions, so this is an observed improvement rather than a
controlled estimate of cache savings. Subsequent current-head measurements
belong in the [implementation PR](https://github.com/zakura-core/wallet-libraries/pull/72).

Local measurements used Rust 1.98.0 on the existing Apple Silicon developer
machine. An empty iteration-profile cache needed 319.46 seconds for its first
focused check. Reusing dependencies in a new review worktree required 76.57
seconds for the test profile and 55.86 seconds for iteration; path changes can
still rebuild workspace crates. Three interleaved changes to the future reader
version forced SQLite recompilation in each profile. Test samples were
7.34/7.14/6.63 seconds, and iteration samples were 5.73/5.35/5.46 seconds. This
supports using iteration for this focused workload; it does not establish a
benefit for proving, migration suites, or every kind of source edit.

The unchanged warm cover-test commands took 4.80/6.36/6.31 seconds; unchanged
fixture checks took 3.07/2.74/3.20 seconds. The original 64.84-second report
command is not the same measured workload and is not a direct before/after
comparison. Fixture helpers address construction and diagnostic friction; no
aggregate fixture-speed improvement is claimed. Shared immutable key preparation
retains independent mutable databases and real migration paths.

Deliberately stale protobuf output failed comparison in 4.95 seconds. Ordinary
builds with the pinned protoc present and with an invalid PROTOC path succeeded
without changing the binding; the latter took 12.75 seconds. Source changes
invalidated early local checks, so their passing test output is excluded from
acceptance evidence. Use the current PR head's final `tests` result before merge.

Remaining limits: these samples do not establish a sustained CI median, registry
access can still serialize downloads, and bypassing the wrapper bypasses target
ownership. Fresh external consumers deliberately retain fresh lock resolution.
The expensive full feature suites remain necessary final validation, now in
parallel rather than repeated between edits.

The shared CI monitor initially accepted two Socket checks before wallet Actions
registered. It was re-armed after the expected jobs appeared. Always require
this workflow's final `tests` check for the current head; an unrelated completed
check is not wallet validation. The shared monitor implementation is outside
this repository's scope.
