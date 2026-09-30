# Transparent PIR ledger design notes

Status: design input for Phases 2–6 of the
[preparatory refactor](transparent-pir-preparatory-refactor.md). Review of the
Phase 1 stack worked through recovery, activation, and qualification scenarios
before any code exercised them. Phase 1 ships only the configuration, policy,
provenance, and authority-snapshot surface. The decisions below are recorded so
that later phases start from them. They are not implemented contracts. Each
phase should confirm or revise them alongside the code that uses them.

## Phase 3 outcome

Phase 3 implements candidate recovery and settles these notes as follows.

- **Implemented as written:**
  - opaque identifiers and lineage;
  - supersession and accepted-revision matching;
  - event identities and spend attribution;
  - unsupported coverage;
  - pending pages, recorded with their target and scripts;
  - resuming from durable pages and coverage.
- **Revised:**
  - **Recovery bound.** Every watched script is required from the account
    birthday. This is Roman's call, and it deviates from the architecture,
    which accepts a birthday only as a justified transparent bound.
    Completeness is computed from the current birthday, so lowering it makes the
    account incomplete without a hook.
  - **Watch-set generation.** It is replaced by per-address checks. A commit
    must name only addresses the account still watches. New addresses are
    uncovered rather than invalidating work. Coordinators repeat while the
    watch set changes or a commit reports `window_grew`.
  - **Pending-page policy context.** Pages do not store a policy generation.
    A policy transition deletes them, and a commit checks the generation it
    captured.
  - **Commit destination.** Every Phase 3 commit is a candidate commit. Account
    lifecycle arrives with promotion.
- **Left open for Phase 4**, settled below: integrity quarantine and trust
  epochs; qualification and privileged verification; promotion blockers beyond
  the candidate blockers; history completeness.

## Phase 4 outcome

Phase 4 implements activation for the library and settles these notes as
follows.

- **Implemented:**
  - integrity quarantine of the source and every account holding its evidence,
    in the rejecting transaction, removing their pending pages;
  - store-held qualification bound to exact revisions (a test and development
    hook only, so production can never promote);
  - promotion requiring every revision that contributed coverage or events to be
    qualified, and active commits refusing unqualified revisions;
  - the commit destination by account lifecycle (candidate or active), captured
    in the watch set and checked at commit;
  - promotion blockers for pending pages, unresolved spends, unsupported
    ranges, incomplete coverage including window growth, an underivable window,
    quarantine, unqualified revisions, legacy discrepancies, and a scanned
    chain behind the tip.
- **Revised:**
  - **Epochs.** Not implemented. With no way to clear a quarantine or requalify
    a source, the per-account quarantine epoch and per-source trust epoch would
    guard nothing. They arrive with privileged verification.
  - **Pending-page context.** Pages still record only their target and
    scripts. A transition deletes them, and a lifecycle change makes a commit
    stale, so the extra context has nothing left to protect.
  - **Publication lag.** No separate blocker: a revision's publication bounds
    its anchor, and coverage short of the local target is `IncompleteCoverage`.
- **Still open:**
  - privileged verification: clearing quarantine, requalifying, and explaining
    legacy discrepancies (Phase 6 or later);
  - history completeness (Phase 5, settled below);
  - the Vizor transition fence: stopping and draining in-flight public lookups
    before a transition to `PrivateRequired` commits.

## Phase 5 outcome

Phase 5 implements the library's history read, and settles the history note
under "Readiness and blockers" as follows.

- **Implemented:** one entry per account, transaction, and supported pool, so
  an undiscovered effect is explicit. Unknown, zero, and not-applicable fees
  are distinct. Public discovery is reported as its own completeness state.
- **Revised:** completeness is derived on every read from stored facts. There
  is no stored completion marker. Unmined shielded effects require scanning
  through the tip and a spend link for every known owned nullifier in the
  stored payload; finding a funding note after ingestion does not itself
  reconcile its unmined spender.
- **Still open:**
  - per-output recipient completeness for external payments, which needs the
    payload capability in the architecture's capability gate;
  - a per-account shielded bound for accounts born after the wallet birthday;
  - upgrading an incoming-viewing-key account to a full viewing key does not
    recompute nullifiers or rescan, so spends that scanning missed stay
    unlinked, in balances as in history. Completeness trusts the current key,
    as balances do.

## Phase 6 outcome

Phase 6 qualifies the library against a block-derived oracle, pre-ledger
upgrade fixtures, failure injection, and repair, and settles these notes as
follows.

- **Implemented:** the qualification suite, and one fix: rewinds,
  re-attribution, and qualification refuse a wallet that requires a newer
  reader, as every other ledger read and write already did.
- **Revised:**
  - **Rollback.** The schema migrator accepts a database carrying migrations it
    does not know, so the reader version is the only guard between an older
    build and newer ledger state. Supported rollback targets are builds with
    the fix whose reader version meets the wallet's requirement; earlier
    builds rewind without checking it.
- **Still open:**
  - privileged verification and trust and quarantine epochs, which need real
    source verification (next stage);
  - verifying coverage anchors against local blocks when reading, as defense
    in depth against a build that rewinds without clipping coverage.

## Recovery sources and revisions

- **Opaque identifiers.** Sources, revisions, and pages are opaque byte strings,
  bounded in length, and compared bytewise only.
- **Lineage.** Each revision carries a lineage that strictly increases with
  each replacement. Keep lineage within `i64::MAX` so SQLite can store it and
  order it numerically.
- **Supersession.** A provisional revision is superseded once the store accepts
  a newer revision of the same source. Its coverage, pages, and event
  observations are removed; an event remains only if another active revision
  observed it. A sealed revision is never superseded.
  Its commits stay acceptable after later revisions, including resumed pages and
  recovery for newly added accounts.
- **Accepted revision.** Store the complete accepted revision per source:
  identity, lineage, sealed status, and publication anchor. A same-lineage commit
  is a retry only if all of these match exactly.
- **Publication anchors are not chain evidence.** A publisher's asserted height
  and hash are kept separate from locally accepted chain points (`ChainPoint`),
  and never substitute for them.

## Commits and events

- **Identities.** A receive is identified by txid and output index. A spend is
  identified by spending txid and input index; its spent outpoint is checked
  content, not part of the identity.
- **Spend attribution.** A spend carries the watched script of the output it
  consumes. This attributes a spend recovered before its output to an account,
  and is checked against the output when that arrives.
- **Unsupported coverage.** A commit can record script ranges its source cannot
  cover, bound to the source revision and to the run's target. An open-ended
  range therefore stays bounded after restart. Unsupported coverage blocks the
  account until another source covers it.
- **Pending pages.**
  - A pending page records the watched scripts that opened it, so it blocks only
    those accounts.
  - It also records the captured context: policy generation, target, lifecycle,
    and source trust epoch.
  - A restarted coordinator must be able to enumerate outstanding pages.
- **Resuming bounded recovery.** Needs per-script coverage gaps, or a durable
  next-work cursor, so already covered scripts are not replayed after restart.
- **Commit destination.** Coordinators need each account's lifecycle (legacy
  public, candidate, active) to choose a commit's destination before any I/O.

## Integrity, trust, and qualification

- **Integrity rejection.** An integrity rejection refuses every submitted fact
  and durably quarantines the source and the affected accounts, in the same
  transaction. It removes their pending pages. Authority stays blocked across
  restarts.
- **Epochs.**
  - A quarantine advances a per-account quarantine epoch and a per-source trust
    epoch. Re-verification advances the trust epoch again.
  - Runs capture both epochs before I/O, so work started before a quarantine is
    never accepted after revalidation.
- **Qualification.**
  - Qualification is store-held and binds to exact verified revisions. Each
    revision is qualified separately, and fixture sources are never qualified.
  - Promotion requires every revision that contributed coverage to be qualified.
  - Active commits must also reject or isolate events from unqualified
    revisions.
- **Privileged verification.** Qualifying a revision, clearing a quarantine, and
  advancing the trust epoch need a privileged verification operation separate
  from commits. It is added with real source verification, after this plan.

## Readiness and blockers

- **Promotion blockers.** Beyond what Phase 1 reports, promotion needs blockers
  for:
  - publication lag;
  - pending pages;
  - unresolved spends;
  - unsupported history;
  - integrity failure;
  - an unexplained legacy discrepancy (not an integrity failure);
  - an unqualified source;
  - incomplete coverage, including scripts added by window expansion;
  - an unstable watch window.
- **History completeness.** Report it per account and per supported pool, with
  an entry for every pair. That way an undiscovered effect stays explicit rather
  than implied absent. Unknown, zero, and not-applicable details remain distinct.

## Privacy enforcement still to complete

- **Phase 2.** Withhold or privately route queued transparent follow-on work,
  such as parent-transaction retrieval, under `PrivateRequired`, together with
  the other Enhance/Status routing and dispatch guards. Durable policy
  generations bind outstanding discovery work to the policy that produced it, so
  switching a handle's mode cannot leave dispatchable public requests.
- **Consolidated handle policy.** Handles carry three privacy modes today:
  status, enhancement, and transparent. They derive from one user setting and
  should converge on one resolved handle policy.
- **`confirmations_until_spendable`.** `u32::MAX` currently signals "not
  spendable under current transparent authority". Prefer an explicit
  representation when this API is next revised.
