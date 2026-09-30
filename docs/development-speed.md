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
| CI serialization | Six parallel feature configurations, caches, final result, PR cancellation | First superseded run was cancelled after a push; final CI and timing pending |
| Protobuf side effects | Explicit check/write generation; ordinary builds consume checked-in bindings | Pinned comparison passed; deliberate stale binding rejected in 4.95 seconds; ordinary builds with/without protoc leave it unchanged |
| PIR real-time waiting | Private monotonic clock advances during dispatch | Two tests previously shared the 31-second delay; 13 cover tests executed in 3.81 seconds, but source changes invalidated that run; final checks pending |
| Fixture friction | Transaction-bound operations and reader-version injection helpers; shared immutable key preparation | Focused Rust validation pending |
| Formatting permissions | Formatting is read-only with `--check`; no compilation or Cargo target lock | Recovered operational event is a rustfmt diff failure, misclassified as permissions; current formatting check passes |
| Iteration profile | Optional profile in Cargo configuration; final test profile preserved | Repeated measurements pending; no default profile change |

Completion requires the corresponding checks to pass on the implementation
revision. CI cancellation and sustained cache savings need real GitHub runs.
Do not infer these from configuration inspection or local unit tests.

The shared CI monitor initially accepted two Socket checks before wallet Actions
registered. It was re-armed after the expected jobs appeared. Always require
this workflow's final `tests` check for the current head; an unrelated completed
check is not wallet validation. The shared monitor implementation is outside
this repository's scope.
