# Private recovery of mixed transparent/Ironwood shieldings

A wallet that recovers a transparent-to-Ironwood shielding by private queries only
(`PrivateRequired`, private Ironwood enhancement) knows:

- its own effects: the transparent outputs it spent (transparent PIR spend events) and the
  Ironwood note it received (compact scanning);
- the Enhance PIR record for its received action: the authenticated memo, the service's
  transparent shape flags, and the whole-transaction fee;
- the qualified transparent metadata on its spend events: the whole-transaction fee, the
  transparent input count, and whether any shielded component exists.

## What is now recovered (no wire change)

- The received memo, authenticated by decrypting the scanned note, even though the
  transaction's transparent details stay unsupported (route 2).
- The whole-transaction fee into `transactions.fee`, only when it agrees with every known fee,
  expiry, and displayed expiry. A disagreeing response is rejected without effect.
- Memo work for route-2 transactions, which is requeued on rescans and policy transitions and,
  for existing wallets, by the `ironwood_unsupported_memo_retry` migration.

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
- the metadata's exact fee equals the stored Enhance PIR fee;
- the account spent only transparent funds and received only shielded outputs, with no recorded
  outputs to others;
- spent = received + fee.

The movement is final; the fee stays the whole transaction's (`FeeState::Unknown`) and no
aggregate payment is inferred. Anything else stays `Provisional`: another transparent funder, an
external payment, missing, unknown or disagreeing fees.
