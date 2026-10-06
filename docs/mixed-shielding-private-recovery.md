# Private recovery of mixed transparent/Ironwood shieldings

A wallet that recovers a transparent-to-Ironwood shielding by private queries only
(`PrivateRequired`, private Ironwood enhancement) knows:

- its own effects: the transparent outputs it spent (transparent PIR spend events) and the
  Ironwood note it received (compact scanning);
- the Enhance PIR record for its received action: the authenticated memo, the service's
  transparent shape flags, and optionally the whole-transaction fee;
- the qualified transparent metadata on its spend events: the whole-transaction fee, the
  transparent input count, and whether any shielded component exists.

## What is now recovered (no wire change)

- The received memo, authenticated by decrypting the scanned note, even though the
  transaction's transparent details stay unsupported (route 2).
- The whole-transaction fee into `transactions.fee`, only when it agrees with every known fee,
  expiry, and displayed expiry. A disagreeing response is rejected without effect.
- The separate `has_transparent_outputs` assertion, with `NULL` for unknown shape and rejection
  of conflicting assertions. This is trusted service display evidence; only the memo is
  authenticated by note decryption.
- Memo work for route-2 transactions, which is requeued on rescans and policy transitions and,
  for existing wallets, by the `ironwood_unsupported_memo_retry` migration.
- Shape evidence for existing route-2 wallets whose memos are already known, using one
  received-note-bound private metadata query. The additive `ironwood_transparent_output_shape`
  migration leaves old shape evidence unknown and queues that recovery. Public authority
  transitions and reorgs retain their existing dispatch guards; no public fallback is added.

## What the evidence cannot establish

Two shapes have identical evidence. Both spend the account's two transparent inputs (200,000
zatoshis), have no transparent outputs, a 20,000 fee, and two Ironwood actions with the account's
180,000 output at action 1:

1. Pure shielding: action 0 is the builder's padding. `OutputInfo::dummy` is a zero-value
   output with no OVK, so its outgoing ciphertext is encrypted to no key. The Ironwood builder
   uses `default_flags()`, which enables spends, so its dummy spend looks like any spend.
2. Another party spends 100,000 of its own shielded funds into action 0's 100,000 output. The
   pool's net inflow is still 180,000.

Neither action 0 is decryptable or OVK-recoverable by the wallet. The account's effects, the
transparent metadata, and the Enhance PIR record are the same. The test
`foreign_self_balanced_shielded_participation_is_indistinguishable` pins this.

The transparent txid display record (`TransparentDisplayRecord`, wallet-pir `648264bb`) adds the
complete transparent output list and coinbase flag to the same fee, input count, and
shielded-presence facts. Joined with the account's events it fixes the transparent side exactly
(every input, every output), and therefore the net shielded inflow. It does not change the
conclusion above: the remaining ambiguity is entirely inside the shielded bundle. (The adapter
does not fetch display records today.)

Evidence still missing after that join, all hidden by design:

- the values of shielded outputs the wallet cannot decrypt, including zero-value padding;
- whether any shielded spend is real rather than a dummy (spends are enabled in pure shieldings);
- which pools carry the shielded components (`has_shielded_components` is coarse).

A narrow no-wire path would exist only if every relevant shielded output's value could be
authenticated (including zeros) and other pools excluded, or if zero foreign shielded funding
were established independently. Neither is available: servers cannot detect real shielded
spends either.

Sender linkage: a standard shielding's own output is change to the account's internal address,
built with `internal_ovk = None` under `OvkPolicy::Sender`, so no key (Orchard or transparent
OVK) recovers it. The public path links it as sent by the account because it decrypts with the
internal IVK (`AccountInternal`); private recovery has the same fact (an internal-scope received
note in a transaction the account funded), but does not record a `sent_notes` row yet.

## Classification

Public history derived from full data has the same blind spot: `payments_accounted` balances the
account's settled effects against the canonical fee and does not prove padding is zero. Private
recovery does not reuse that inference silently. A mixed transaction without full data is
reported as `HistoryClassification::NetReconstructed` only when:

- every owned effect is complete;
- qualified metadata counts exactly the account's published transparent inputs;
- qualified metadata has an exact whole-transaction fee, which agrees with the canonical fee
  if one is stored; a missing canonical fee does not block reconstruction;
- recovered Enhance PIR shape evidence explicitly says no transparent outputs exist;
- the account spent only transparent funds and received only shielded outputs, with no recorded
  outputs to others;
- spent = received + fee.

The movement is final; the fee stays the whole transaction's (`FeeState::Unknown`) and no
aggregate payment is inferred. Anything else stays `Provisional`: another transparent funder, an
external payment, missing, unknown or disagreeing fees.

## Qualified-fee workaround

The mixed Enhance PIR publisher can currently return a memo and shape without a fee. Once that
memo query is complete, no fee recovery work remains, so rebuilding or repeating sync cannot
fill `transactions.fee`. History now uses the exact fee from qualified transparent metadata
for the narrow net-shielding balance above. It does not copy that value into the canonical fee
column. Quarantined or unqualified transparent metadata, incomplete owned effects, a foreign
transparent input, unknown/present transparent outputs, a mismatched canonical fee, or a failed
balance leaves the entry provisional. A recovered memo is still required for complete payment
details. Even when net reconstruction succeeds, `FeeState::Unknown`, `AggregatePayment::Unknown`,
and the pending mixed-details marker remain: the whole fee is not proven to be the account's.

This is a wallet-library history workaround, not a publisher repair or a claim of complete
mixed-transaction reconstruction. A consumer that supports `NetReconstructed` can display the
owned transparent-to-Ironwood net transfer (for example 400,000 zatoshis received), but native
Vizor behavior must be tested after repinning the library.

## Outgoing Activity of a shielded send with transparent outputs

The reverse shape spends the account's Ironwood notes and pays a transparent address: for the
reported mainnet transaction, 107,485,000 zatoshis spent, 107,220,000 returned as Ironwood change,
a 15,000 fee, and 250,000 to a transparent output. A fresh `PrivateRequired` restore holds the
spend and the change (compact scanning) and the Enhance PIR records with the fee and the
`has_transparent_outputs` assertion. Outgoing recovery never runs for a mixed record, and
transparent recovery links no sent output, so nothing records where the 250,000 went. When the
transparent output is the account's own, transparent recovery adds a 250,000 receipt and the net
movement is the fee alone.

History reports two facts for Activity without changing attribution:

- `whole_fee`: the stored fee (here, the Enhance record's) or the exact qualified metadata fee,
  unknown when they disagree. `fee` remains the account's share (`Unknown` here).
- `inferred_outgoing`: owned shielded spent − owned shielded returned − whole fee, 250,000 here.
  It requires a mined transaction without full data whose Enhance record asserts transparent
  outputs, complete shielded effects, a complete transparent effect (private coverage reaches
  the transaction) with no transparent spend by the account, a known and uncontradicted fee, no
  ledger-recorded transparent input in the transaction, no qualified metadata
  counting a transparent input, no other funding wallet account, a positive result, and no
  recorded output the account sent to anyone else (which Activity would show itself).

The value is inferred: it assumes the account paid the whole fee, which a foreign input or
foreign shielded spend could have shared; a fee supplied only by Enhance is trusted service
metadata that note decryption does not authenticate; and the value may include outputs to the
account's own transparent addresses or shielded outputs to others whose outgoing viewing key
was discarded (`an_unrecoverable_shielded_payment_is_part_of_the_inferred_value`). `aggregate_payment`, `payment_details`, the
account's fee, and the `Provisional` classification are unchanged, and no recipient is claimed.
The public restore of the same transaction records the 250,000 output as the account's send;
`activity_outgoing.rs` compares the two.
