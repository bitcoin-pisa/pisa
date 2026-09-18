//! The receiver's part: contribute inputs that join the group.

use anyhow::Result;
use bitcoin::{OutPoint, TxOut};
use payjoin::bitcoin::{psbt, TxIn};
use payjoin::cisa;
use payjoin::receive::InputPair;

use crate::cost;
use crate::wallet::AggregatingWallet;

/// How the receiver spends a witness version 2 coin it contributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spend {
    /// As a member of the full-aggregation group.
    Aggregated,
    /// On its own, with a signature of its own. BIP 460 lets an input opt
    /// out of aggregation, and the demo needs that to measure what the same
    /// transaction costs without it.
    Separate,
}

/// A receiver input for `outpoint`. A witness version 2 coin spent
/// [`Aggregated`](Spend::Aggregated) joins the group with a nonce reserved
/// now; any other coin is contributed as it is.
///
/// The receiver signs in the same step in which it builds the proposal, so
/// the reservation lives only until then. That is what makes the receiver
/// the stateless party of the two.
pub fn input_pair(
    wallet: &impl AggregatingWallet,
    outpoint: OutPoint,
    txout: TxOut,
    spend: Spend,
) -> Result<InputPair> {
    let mut psbtin = psbt::Input {
        witness_utxo: Some(txout.clone()),
        ..Default::default()
    };
    let mut weight = None;
    if cisa::is_witness_v2_keypath(&txout.script_pubkey) {
        match spend {
            Spend::Aggregated => {
                cisa::set_fullagg(&mut psbtin, wallet.reserve_nonce(outpoint)?);
            }
            // An unsigned witness version 2 input has no weight the payjoin
            // crate can predict unless it is in a group, where its witness is
            // one empty element. Outside a group it costs a key path spend.
            Spend::Separate => weight = Some(cost::KEY_PATH_INPUT),
        }
    }
    let txin = TxIn {
        previous_output: outpoint,
        ..Default::default()
    };
    Ok(InputPair::new(txin, psbtin, weight)?)
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;
    use bitcoin::{Amount, ScriptBuf, Txid, WitnessProgram, WitnessVersion};

    use super::*;
    use crate::wallet::tests::FakeWallet;

    fn witness_v2(program: u8) -> ScriptBuf {
        ScriptBuf::new_witness_program(
            &WitnessProgram::new(WitnessVersion::V2, &[program; 32]).unwrap(),
        )
    }

    fn coin(script: ScriptBuf) -> (OutPoint, TxOut) {
        let outpoint = OutPoint {
            txid: Txid::all_zeros(),
            vout: 3,
        };
        let txout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: script,
        };
        (outpoint, txout)
    }

    #[test]
    fn witness_v2_coin_joins_the_group() {
        let script = witness_v2(1);
        let wallet = FakeWallet::owning([script.clone()]);
        let (outpoint, txout) = coin(script);

        // An unsigned witness v2 input has a known weight only as a member of
        // the group, so building the pair without an explicit weight succeeds
        // only if the input was declared for aggregation.
        let pair = input_pair(&wallet, outpoint, txout, Spend::Aggregated).unwrap();
        assert_eq!(pair.outpoint(), outpoint);
        assert_eq!(wallet.reservations(), 1);
    }

    #[test]
    fn opting_out_reserves_no_nonce() {
        let script = witness_v2(1);
        let wallet = FakeWallet::owning([script.clone()]);
        let (outpoint, txout) = coin(script);

        let pair = input_pair(&wallet, outpoint, txout, Spend::Separate).unwrap();
        assert_eq!(pair.outpoint(), outpoint);
        assert_eq!(wallet.reservations(), 0);
    }

    #[test]
    fn other_coins_are_contributed_as_they_are() {
        let wallet = FakeWallet::owning([]);
        let (outpoint, txout) = coin(ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::all_zeros()));

        let pair = input_pair(&wallet, outpoint, txout, Spend::Aggregated).unwrap();
        assert_eq!(pair.outpoint(), outpoint);
        assert_eq!(wallet.reservations(), 0);
    }
}
