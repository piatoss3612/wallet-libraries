# Receiver directory restore sweeps

`sweep` runs a restored wallet's swap-key sweeps. Each swap key recovered from the
seed, a refund key from its funding memo or an incoming lookahead key, is looked up
once in a receiver directory over PIR, and the payments found are imported only
after the wallet checks them against its own chain, through the swap receiving
steps of `zakura-client-sqlite`.

Applications supply:

- the directory's HTTPS origin and a `Transport` for it, and a `NoteSource` for the
  note data of found payments, usually `EnhanceNotes` over Enhance PIR. Both carry
  the application's route policy, timeouts and cancellation; this crate has no HTTP
  client.
- a `WriteLock` that serializes the sweep's wallet writes with the application's
  other writers.
- the network's genesis hash (`MAINNET_GENESIS` on mainnet) and the clock.

A run sweeps only while the wallet's fully scanned tip is its chain tip, and makes
no request when nothing is due. The publication must commit to the genesis hash,
cover history from Ironwood activation, and end at a block the wallet scanned no
more than `MAX_PUBLICATION_LAG` blocks below its tip; otherwise no lookup is sent.
A receiver is only ever sent inside a PIR query, and large jobs download the
directory's row file instead. `EnhanceNotes` opens its session only when a lookup
finds a payment. After each batch the incoming lookahead moves past paid indices
and its new keys are swept too. A key that fails is backed off and reported while
the others go on. Every wallet write is its own transaction, so dropping a run at
any await leaves a consistent queue.

The receiver crates are pinned to a commit of wallet-pir's swap integration branch
until they land on its `main`.

`tests/sweep.rs` serves a directory in process through the receiver service's own
routes and checks that a restored wallet finds and imports a payout, extends its
lookahead past it, repeats no finished sweep, and refuses a publication that is not
on its chain before any lookup.
