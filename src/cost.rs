//! What the pieces of a transaction weigh.
//!
//! The demo compares an exchange's deposit and withdrawal flow with and
//! without payjoin, and with and without aggregation. Two of the three sizes
//! come from a mined transaction. The third is a transaction pair that the
//! demo does not build, so it is computed here from BIP 141 and BIP 341
//! serialization: every witness version 2 output in this crate pays to a
//! 32-byte program and is spent through the key path, which is the same
//! shape as a taproot key path spend except for the witness.

use bitcoin::Weight;

/// Everything in a transaction that is neither an input nor an output:
/// version, both counts, locktime, and the segwit marker and flag. 10.5 vB.
pub const OVERHEAD: Weight = Weight::from_wu(42);

/// A key path spend with a 64-byte `SIGHASH_DEFAULT` signature, which is
/// what both a taproot input and a witness version 2 input that opts out of
/// aggregation cost. 57.5 vB.
pub const KEY_PATH_INPUT: Weight = Weight::from_wu(230);

/// A member of a full-aggregation group. Its witness is one empty element:
/// the group's signature sits on the group's last input. 41.5 vB.
pub const GROUP_MEMBER_INPUT: Weight = Weight::from_wu(166);

/// The last input of a full-aggregation group, whose witness carries the
/// 64-byte aggregate signature and the BIP 460 marker byte. 57.75 vB.
pub const GROUP_LAST_INPUT: Weight = Weight::from_wu(231);

/// An output paying a 32-byte witness program. 43 vB.
pub const OUTPUT: Weight = Weight::from_wu(172);

/// What the same money movement costs an exchange that does not use payjoin:
/// the customer's deposit as its own transaction, and a later transaction
/// that spends the deposit together with `top_up` of the exchange's own
/// coins to pay `withdrawals` customers and its own change.
///
/// This is the shape an exchange has today, so every input is a key path
/// spend: the batch's inputs are all the exchange's own and could form a
/// group between themselves, but an exchange that aggregated them would not
/// be the baseline the other two rows are measured against.
pub fn without_payjoin(withdrawals: usize, top_up: usize) -> Weight {
    let deposit = OVERHEAD + KEY_PATH_INPUT + OUTPUT * 2;
    let batch = OVERHEAD + KEY_PATH_INPUT * (top_up as u64 + 1) + OUTPUT * (withdrawals as u64 + 1);
    deposit + batch
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vB the constants are documented as. `Weight::to_vbytes_ceil`
    /// rounds, so the halves are checked in weight units.
    #[test]
    fn constants_match_bip_341_arithmetic() {
        assert_eq!(OVERHEAD.to_wu(), 42, "10.5 vB");
        assert_eq!(KEY_PATH_INPUT.to_wu(), 230, "57.5 vB");
        assert_eq!(GROUP_MEMBER_INPUT.to_wu(), 166, "41.5 vB");
        assert_eq!(GROUP_LAST_INPUT.to_wu(), 231, "57.75 vB");
        assert_eq!(OUTPUT.to_wu(), 172, "43 vB");

        // A group of n inputs replaces n key path inputs with n-1 members and
        // one last input, which is where the demo's saving comes from.
        let saving = KEY_PATH_INPUT - GROUP_MEMBER_INPUT;
        assert_eq!(saving.to_wu(), 64, "one signature per input but the last");
    }

    #[test]
    fn two_transactions_without_payjoin() {
        // Deposit: 10.5 + 57.5 + 2 * 43 = 154 vB.
        // Batch:   10.5 + 6 * 57.5 + 5 * 43 = 570.5 vB.
        assert_eq!(without_payjoin(4, 5).to_wu(), 616 + 2282);
    }
}
