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
| CI serialization | Six parallel feature configurations, caches, final result, PR cancellation | Live CI validation and timing pending |
| Protobuf side effects | Explicit check/write generation; ordinary builds consume checked-in bindings | Pinned protoc 34.1 comparison passed in 20.85 seconds on this machine |
| PIR real-time waiting | Private monotonic clock advances during dispatch | Two tests previously shared the 31-second delay; focused Rust validation pending |
| Fixture friction | Transaction-bound operations and reader-version injection helpers; shared immutable key preparation | Focused Rust validation pending |
| Formatting permissions | Formatting is read-only with `--check`; no compilation or Cargo target lock | Current check passes; historical environment rejection not reproduced |
| Iteration profile | Optional profile in Cargo configuration; final test profile preserved | Repeated measurements pending; no default profile change |

Completion requires the corresponding checks to pass on the implementation
revision. CI cancellation and sustained cache savings need real GitHub runs.
Do not infer these from configuration inspection or local unit tests.
