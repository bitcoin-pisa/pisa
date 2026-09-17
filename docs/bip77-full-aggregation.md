# Async payjoin with full signature aggregation

A profile of BIP 77 for two-party transactions whose witness version 2 inputs
form one BIP 460 full-aggregation group.

Status: draft, tested by the prototype in this repository. License: CC0-1.0.

## Summary

A BIP 77 payjoin already has the two messages that BIP 459 full aggregation
needs. The sender's first message carries its public nonce. The receiver's
reply carries the receiver's public nonce and partial signature. The sender
then signs, aggregates and broadcasts. No message is added and no message
grows beyond a few dozen bytes per input.

The PSBT fields are the ones defined in the draft "CISA Fields for PSBT" by
Fabian Jahr. This document does not allocate fields. It says who sets which
field when, what each side checks, and which BIP 78 rules change for
aggregated inputs.

## Fields

Per-input, no key data, as in the draft:

| Type | Name | Value |
|------|------|-------|
| 0x21 | PSBT_IN_CISA_MODE | one byte, 0xbd for full aggregation |
| 0x23 | PSBT_IN_CISA_FULLAGG_PUB_NONCE | 66 bytes |
| 0x24 | PSBT_IN_CISA_FULLAGG_PARTIAL_SIG | 32 bytes |

The half-aggregation signature field 0x22 is not used by this profile.

## Roles

The sender is the stateful signer. It reserves a nonce before the session and
holds the secret half until the proposal arrives. The receiver is the last
signer. It reserves its nonce and produces its partial signature within one
step and keeps no nonce state between messages.

## Flow

### Sender, before sending the original PSBT

1. Build and sign the fallback transaction as BIP 78 requires. Every input is
   signed as an opted-out BIP 341 key path spend, so the fallback is
   broadcastable on its own.
2. For each own witness version 2 input that should be aggregated, reserve a
   nonce for that outpoint. In Bitcoin Core this is `reservecisanonce`.
3. On each such input of the original PSBT, set PSBT_IN_CISA_MODE to 0xbd and
   PSBT_IN_CISA_FULLAGG_PUB_NONCE to the reserved public nonce. The input
   keeps its fallback witness. The fields describe the payjoin transaction,
   not the fallback.
4. Send the original PSBT as usual.

The sender does not strip these two fields when it strips the other optional
fields from the original PSBT.

### Receiver, on the original PSBT

1. Run the BIP 78 receiver checks unchanged. The fallback must be
   broadcastable.
2. Keep the mode and public nonce of every sender input that carries them.
3. Clear the sender's fallback signatures, as BIP 78 requires.
4. Contribute inputs. For each own witness version 2 input that should join
   the group, reserve a nonce, then set mode 0xbd and the public nonce on that
   input.
5. Sign. With every public nonce of the group present, the wallet produces the
   partial signature for each receiver input in one call and stores it in
   PSBT_IN_CISA_FULLAGG_PARTIAL_SIG.
6. Do not finalize the aggregated inputs. They are complete, not finalized.
   The receiver finalizes any non-aggregated input of its own as before.
7. Send the proposal. On every input, the proposal keeps mode, public nonce
   and partial signature alongside the fields BIP 78 already allows.

### Sender, on the proposal

1. Run the BIP 78 sender checks with two changes. A receiver input with mode
   0xbd passes the "finalized" check when it carries a public nonce and a
   partial signature. Every sender input must carry exactly the aggregation
   fields the sender put on it.
2. For the fee checks, the weight of a witness version 2 key path input is 166
   weight units as a group member and 231 weight units as the group's final
   input.
3. Sign. The wallet finds the reserved nonce for each own input, produces the
   partial signatures, verifies every partial signature of the group,
   aggregates, and writes the final witnesses for all group inputs.
4. Broadcast.

## Rules

- Aggregated inputs sign with SIGHASH_DEFAULT. The sender rejects any other
  sighash type on its own inputs, and this profile extends that to the
  receiver's aggregated inputs.
- The group's final input is the one with the highest index. Either party may
  own it. Which party pays for the extra 65 bytes of witness on that input is
  a fee question outside this profile.
- A sender input whose aggregation fields differ between original PSBT and
  proposal invalidates the session. The sender must not reuse the reserved
  nonce for a corrected proposal. It reserves a new one and starts a new
  session.
- A wallet restart on the sender side loses the reservation. The sender then
  cannot sign the proposal, and the fallback transaction is the outcome of the
  session.
- The receiver's reservation lives only for the duration of steps 4 to 5
  above. A receiver that persists its session between those steps and restarts
  produces a proposal without a partial signature, which the sender rejects.
- The PSBTs are version 0.

## What each side can and cannot verify

The receiver cannot verify anything about the sender's public nonce beyond
its length. A sender input that carries a malformed nonce is left unsigned by
the receiver's wallet, so the sender gets back a proposal it rejects.

The sender's wallet verifies the receiver's partial signature with
PartialSigVerify before aggregating, so a malformed receiver signature fails
at the sender before broadcast. The receiver's protection against a bad sender
is the same as in BIP 78: it never broadcasts the payjoin transaction itself
and holds a valid fallback.

## Out of scope

Half aggregation, silent payments, hardware signers on the sender side, and
more than two parties.

## What the implementation changed

The profile above was written before the prototype and corrected by it. The
first draft is in the repository history. These are the differences.

**The payjoin library was in the way in four places, not zero.** The draft
assumed the fields would pass through a BIP 78 implementation as unknown
PSBT fields. In the payjoin crate they did not. The sender strips every
unknown field from the original PSBT before sending. The receiver rebuilds
the proposal from a fixed list of allowed fields, dropping the sender's nonce
and its own. The sender requires every receiver input to be finalized. And
weight prediction has no case for witness version 2, which broke both the
receiver's input contribution and the sender's fee checks. All four are
changed on the `cisa-psbt-carriage` branch of the crate, each with a
test, and the profile's flow now names where each rule is enforced.

**The sender checks its own fields on the way back.** The draft only said the
sender keeps its fields. A receiver could have altered the sender's mode or
nonce in the proposal. The sender now rejects a proposal whose aggregation
fields on its own inputs differ from what it sent, and the rule above says
so.

**The receiver reserves its nonce when it picks the coin, not when it
signs.** The draft described the receiver as running NonceGen inside the
signing step. Bitcoin Core signs a full-aggregation input in one call only
if the input already carries a nonce, so the receiver reserves the nonce
when it builds the input and signs with it a few steps later. The receiver
is still the last signer and still holds no nonce across a message, but the
reservation does live across the receiver's own typestate transitions. The
rule about a receiver restart between those steps is new.

**The receiver does not pre-check the sender's nonce.** The draft had the
receiver discard the mode of a sender input without a well-formed nonce. The
prototype leaves that to the wallet, which refuses to sign the group, and to
the sender, which rejects the resulting proposal. This is a weaker outcome
for the sender, which learns about its own mistake one round trip later, and
a receiver may add the check.

**PSBT version 0.** Bitcoin Core now creates version 2 PSBTs by default and
the Rust side reads version 0 only. The profile says version 0 until that
changes.

**The weights are as predicted.** The mined transaction weighs 783 weight
units: 40 for the transaction frame, 344 for two outputs, 2 for the segwit
marker, 166 for the member input and 231 for the final input.
