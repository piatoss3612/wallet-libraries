# Private recovery of mixed transparent/Ironwood shieldings

A wallet that recovers a transparent-to-Ironwood shielding by private queries only
(`PrivateRequired`, private Ironwood enhancement) knows:

- its own effects: the transparent outputs it spent (transparent PIR spend events) and the
  Ironwood note it received (compact scanning);
- the qualified transparent metadata on its spend events: the whole-transaction fee, the
  transparent input count, and whether any shielded component exists (transparent publisher);
- the Enhance PIR record for its received action: the authenticated memo, the transaction's
  transparent shape (transparent inputs, transparent outputs), its expiry, and, when the
  publisher supplies one, the whole-transaction fee (Enhance publisher).

## What is recovered (no wire change)

- The received memo, authenticated by decrypting the scanned note, although the transaction's
  transparent details stay unsupported (route 2).
- The record's transparent shape, each flag kept separately, with the mined height it was
  validated at (`ironwood_enhance_routing.transparent_flags{,_height}`). NULL is unknown, never
  "no transparent data". A shape recorded at another height is unknown and is retrieved again.
  A record contradicting the recorded shape is rejected before any write.
- The whole-transaction fee into `transactions.fee`, only when it agrees with every known fee,
  expiry and displayed expiry.
- Route-2 work: memo work, and metadata work bound to the first received version-3 note while the
  fee or a current shape is unknown. A response without a fee keeps the memo and shape and leaves
  the fee retryable at the same position. Work is dispatched only privately and only while public
  authority is absent; it is requeued on rescans, policy transitions back to `PrivateRequired`,
  and, for existing wallets, by the `ironwood_unsupported_memo_retry` and
  `ironwood_unsupported_details_retry` migrations.

## Classification

A mixed transaction without full data is reported as `HistoryClassification::NetReconstructed`
only when:

- every owned effect is complete;
- qualified metadata counts exactly the account's published transparent inputs;
- the metadata's exact fee equals the stored Enhance PIR fee;
- the recorded Enhance PIR shape, at the current placement, has transparent inputs and
  explicitly no transparent outputs;
- the account spent only transparent funds and received only Ironwood outputs, with no recorded
  outputs to others, and no other account of the wallet is known to have funded it;
- spent = received + fee.

The movement is final; the fee stays the whole transaction's (`FeeState::Unknown`) and no
aggregate payment is inferred. Anything else stays `Provisional`: another transparent funder,
transparent outputs (for example a foreign Ironwood input paying an external transparent output,
which leaves the account's own equation unchanged), an unknown or stale shape, funding by another
account, an external payment, missing, unknown or disagreeing fees.

## What the evidence cannot establish

Two shapes have identical evidence. Both spend the account's two transparent inputs (200,000
zatoshis), have no transparent outputs, a 20,000 fee, and two Ironwood actions with the account's
180,000 output at one of them:

1. Pure shielding: the other action is the builder's padding. `OutputInfo::dummy` is a zero-value
   output with no OVK, and the Ironwood builder enables spends, so its dummy spend looks like any
   spend.
2. Another party spends 100,000 of its own Ironwood funds into an equal output of its own. The
   pool's net inflow is still 180,000.

Neither foreign action is decryptable or OVK-recoverable by the wallet; the account's effects, the
transparent metadata and the Enhance PIR records are the same. Both are `NetReconstructed`
(`foreign_self_balanced_shielded_participation_is_indistinguishable`,
`a_hidden_self_balanced_ironwood_pair_remains_a_net_reconstruction`). The transparent txid display
record (wallet-pir `648264bb`) would fix the transparent side exactly, but the remaining ambiguity
is inside the shielded bundle: undecryptable output values (including zero padding), whether a
spend is real, and which pools carry the shielded components.

Sender linkage: a standard shielding's own output is internal change built with no OVK, so no key
recovers it as sent. Private recovery has the same fact as the public path (an internal-scope
receipt in a transaction the account funded) but records no `sent_notes` row for it.

## Publisher evidence gate

The Enhance publisher (wallet-pir `enhance-pir-server` `transaction_metadata`, at the pinned
`648264bb` and at current main) reports a fee only for pure-Ironwood transactions. Every record of
a transaction with transparent data carries no fee. The classification above cross-checks the
transparent publisher's exact fee against the Enhance fee, so with current records a privately
recovered shielding recovers its memo and shape, keeps its fee work queued, and stays
`Provisional`. The qualification in `zakura-pir-transparent`'s `mixed_shielding` test derives both
publishers' records from serialized transactions and pins this outcome; it also shows that a
record carrying the fee completes the classification.

Closing the gate needs one of:

- the Enhance publisher supplying the exact whole-transaction fee for mixed transactions. Its
  server reads raw blocks without previous outputs, so it would need the transparent input values
  (as the transparent publisher resolves them). Validation stays as today: trusted service
  metadata, accepted only when it equals the transparent publisher's exact fee and every known
  fee and expiry. No wire change: the record already has a fee flag and field.
- a product decision to accept the transparent publisher's exact fee alone when the Enhance record
  carries none, giving up the two-service cross-check.

Until then, each such transaction keeps one private metadata query per enhancement pass.
