//! The receiver's part: contribute inputs that join the group.

use anyhow::Result;
use bitcoin::{OutPoint, TxOut};
use payjoin::bitcoin::{psbt, TxIn};
use payjoin::cisa;
use payjoin::receive::InputPair;

use crate::wallet::AggregatingWallet;

/// A receiver input for `outpoint`. A witness version 2 coin joins the
/// full-aggregation group with a nonce reserved now; any other coin is
/// contributed as it is.
///
/// The receiver signs in the same step in which it builds the proposal, so
/// the reservation lives only until then. That is what makes the receiver
/// the stateless party of the two.
pub fn fullagg_input_pair(
    wallet: &impl AggregatingWallet,
    outpoint: OutPoint,
    txout: TxOut,
) -> Result<InputPair> {
    let mut psbtin = psbt::Input {
        witness_utxo: Some(txout.clone()),
        ..Default::default()
    };
    if cisa::is_witness_v2_keypath(&txout.script_pubkey) {
        cisa::set_fullagg(&mut psbtin, wallet.reserve_nonce(outpoint)?);
    }
    let txin = TxIn {
        previous_output: outpoint,
        ..Default::default()
    };
    Ok(InputPair::new(txin, psbtin, None)?)
}

#[cfg(test)]
mod tests {
    use bitcoin::hashes::Hash;
    use bitcoin::{Amount, ScriptBuf, Txid, WitnessProgram, WitnessVersion};

    use super::*;
    use crate::wallet::tests::FakeWallet;

    #[test]
    fn witness_v2_coin_joins_the_group() {
        let script = ScriptBuf::new_witness_program(
            &WitnessProgram::new(WitnessVersion::V2, &[1; 32]).unwrap(),
        );
        let wallet = FakeWallet::owning([script.clone()]);
        let outpoint = OutPoint {
            txid: Txid::all_zeros(),
            vout: 3,
        };
        let txout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: script,
        };

        // An unsigned witness v2 input has a known weight only as a member of
        // the group, so building the pair without an explicit weight succeeds
        // only if the input was declared for aggregation.
        let pair = fullagg_input_pair(&wallet, outpoint, txout).unwrap();
        assert_eq!(pair.outpoint(), outpoint);
        assert_eq!(wallet.reservations(), 1);
    }

    #[test]
    fn other_coins_are_contributed_as_they_are() {
        let wallet = FakeWallet::owning([]);
        let outpoint = OutPoint {
            txid: Txid::all_zeros(),
            vout: 0,
        };
        let txout = TxOut {
            value: Amount::from_sat(50_000),
            script_pubkey: ScriptBuf::new_p2wpkh(&bitcoin::WPubkeyHash::all_zeros()),
        };

        let pair = fullagg_input_pair(&wallet, outpoint, txout).unwrap();
        assert_eq!(pair.outpoint(), outpoint);
        assert_eq!(wallet.reservations(), 0);
    }
}
