//! The sender's part: declare which inputs join the group before the
//! original PSBT leaves.

use anyhow::Result;
use bitcoin::Psbt;
use payjoin::cisa;

use crate::wallet::AggregatingWallet;

/// Mark every witness version 2 input of `psbt` that `wallet` can sign for
/// as a member of the full-aggregation group, with a freshly reserved nonce.
///
/// The PSBT is the signed fallback transaction. Its inputs keep their
/// opted-out witnesses, since the receiver may broadcast that transaction
/// as it is. The fields added here describe the payjoin transaction instead.
///
/// Returns how many inputs were declared.
pub fn declare_fullagg(psbt: &mut Psbt, wallet: &impl AggregatingWallet) -> Result<usize> {
    let mut declared = 0;
    for (txin, input) in psbt.unsigned_tx.input.iter().zip(psbt.inputs.iter_mut()) {
        let Some(utxo) = &input.witness_utxo else {
            continue;
        };
        if !cisa::is_witness_v2_keypath(&utxo.script_pubkey)
            || !wallet.is_mine(&utxo.script_pubkey)?
        {
            continue;
        }
        let nonce = wallet.reserve_nonce(txin.previous_output)?;
        cisa::set_fullagg(input, nonce);
        declared += 1;
    }
    Ok(declared)
}

#[cfg(test)]
mod tests {
    use bitcoin::{ScriptBuf, WitnessProgram, WitnessVersion};

    use super::*;
    use crate::wallet::tests::{psbt_spending, FakeWallet};

    fn witness_program(version: WitnessVersion, program: u8) -> ScriptBuf {
        ScriptBuf::new_witness_program(&WitnessProgram::new(version, &[program; 32]).unwrap())
    }

    #[test]
    fn declares_own_witness_v2_inputs_only() {
        let mine = witness_program(WitnessVersion::V2, 1);
        let theirs = witness_program(WitnessVersion::V2, 2);
        let p2tr = witness_program(WitnessVersion::V1, 3);
        let wallet = FakeWallet::owning([mine.clone(), p2tr.clone()]);

        let mut psbt = psbt_spending([mine, theirs, p2tr]);
        assert_eq!(declare_fullagg(&mut psbt, &wallet).unwrap(), 1);

        let expected = wallet.nonce_for(psbt.unsigned_tx.input[0].previous_output);
        assert!(cisa::is_fullagg(&psbt.inputs[0]));
        assert_eq!(
            cisa::fullagg_pub_nonce(&psbt.inputs[0]),
            Some(&expected[..])
        );
        assert!(cisa::fields(&psbt.inputs[1]).is_empty(), "not ours");
        assert!(cisa::fields(&psbt.inputs[2]).is_empty(), "not witness v2");
        assert_eq!(wallet.reservations(), 1);
    }
}
