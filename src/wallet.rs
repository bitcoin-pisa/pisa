//! The wallet side of the seam.
//!
//! Everything that touches keys or nonces happens inside the wallet, behind
//! [`AggregatingWallet`]. The payjoin side only ever sees public nonces and
//! partial signatures as PSBT fields. [`CoreWallet`] implements the trait
//! over the RPC of a Bitcoin Core that supports BIP 460.

use anyhow::{anyhow, Context, Result};
use bitcoin::{Address, Amount, FeeRate, Network, OutPoint, Psbt, Script, Transaction, Txid};
use payjoin::bitcoin::TxOut;
use payjoin::cisa;
use payjoin::receive::InputPair;
use payjoin_test_utils::corepc_node::vtype::{WalletCreateFundedPsbt, WalletProcessPsbt};
use payjoin_test_utils::corepc_node::Client;
use serde_json::json;
use std::str::FromStr;

/// A wallet that can take part in a BIP 460 full-aggregation group.
///
/// The two methods are the whole contract between the payjoin half and the
/// aggregation half:
///
/// - [`reserve_nonce`](Self::reserve_nonce) runs BIP 459 `NonceGen` for one
///   of the wallet's witness version 2 outputs before the spending
///   transaction exists. The wallet keeps the secret nonce, and must keep it
///   in memory only, use it for at most one transaction, and discard it on
///   restart. It returns the 66-byte public nonce for the PSBT.
/// - [`process_psbt`](Self::process_psbt) is the draft's Signer and Finalizer
///   in one call. For an input that carries mode 0xbd and a public nonce
///   reserved here, and once every input of the group carries a nonce, it
///   adds the partial signature. Once every input of the group carries a
///   partial signature, it verifies them, aggregates, and writes the final
///   witnesses of the whole group.
pub trait AggregatingWallet {
    /// Reserve a nonce for a full-aggregation spend of `outpoint`.
    fn reserve_nonce(&self, outpoint: OutPoint) -> Result<[u8; cisa::PUB_NONCE_LEN]>;

    /// Sign what the wallet can sign and finalize what it can finalize.
    fn process_psbt(&self, psbt: &Psbt) -> Result<Psbt>;

    /// Whether the wallet can spend `script`.
    fn is_mine(&self, script: &Script) -> Result<bool>;
}

/// A Bitcoin Core wallet reached over RPC.
pub struct CoreWallet {
    rpc: Client,
    network: Network,
}

impl CoreWallet {
    pub fn new(rpc: Client, network: Network) -> Self {
        Self { rpc, network }
    }

    pub fn rpc(&self) -> &Client {
        &self.rpc
    }

    /// A funded PSBT paying `outputs`, signed so that it can be broadcast on
    /// its own. This is the fallback transaction BIP 78 requires.
    pub fn create_psbt(&self, outputs: &[(Address, Amount)], fee_rate: FeeRate) -> Result<Psbt> {
        let outputs: Vec<serde_json::Value> = outputs
            .iter()
            .map(|(address, amount)| json!({ address.to_string(): amount.to_btc() }))
            .collect();
        let options = json!({
            "lockUnspents": true,
            "fee_rate": fee_rate.to_sat_per_vb_ceil(),
        });
        let funded = self
            .rpc
            .call::<WalletCreateFundedPsbt>(
                "walletcreatefundedpsbt",
                // rust-bitcoin reads PSBT version 0 only, and Bitcoin Core
                // creates version 2 by default.
                &[
                    json!([]),
                    json!(outputs),
                    json!(null),
                    options,
                    json!(false),
                    json!(null),
                    json!(0),
                ],
            )
            .context("walletcreatefundedpsbt")?;
        self.process_psbt(&Psbt::from_str(&funded.psbt)?)
    }

    /// Whether the node would accept `tx` into its mempool.
    pub fn can_broadcast(&self, tx: &Transaction) -> Result<bool> {
        let results = self.rpc.test_mempool_accept(std::slice::from_ref(tx))?;
        results
            .0
            .first()
            .map(|result| result.allowed)
            .ok_or_else(|| anyhow!("testmempoolaccept returned nothing"))
    }

    pub fn broadcast_tx(&self, tx: &Transaction) -> Result<Txid> {
        Ok(self.rpc.send_raw_transaction(tx)?.txid()?)
    }

    pub fn get_new_address(&self) -> Result<Address> {
        let address = self
            .rpc
            .get_new_address(
                None,
                Some(payjoin_test_utils::corepc_node::AddressType::Bech32m),
            )?
            .into_model()?
            .0;
        Ok(address.require_network(self.network)?)
    }

    /// The wallet's spendable coins as receiver inputs, without any
    /// aggregation fields yet.
    pub fn list_unspent(&self) -> Result<Vec<InputPair>> {
        let unspent = self.rpc.list_unspent()?.into_model()?;
        unspent
            .0
            .into_iter()
            .map(|utxo| {
                let txout = TxOut {
                    value: utxo.amount,
                    script_pubkey: utxo.script_pubkey,
                };
                let outpoint = OutPoint {
                    txid: utxo.txid,
                    vout: utxo.vout,
                };
                Ok((outpoint, txout))
            })
            .map(|coin: Result<_>| {
                let (outpoint, txout) = coin?;
                crate::receiver::fullagg_input_pair(self, outpoint, txout)
            })
            .collect()
    }

    /// The transaction output at `outpoint`, from the node's UTXO set.
    pub fn get_txout(&self, outpoint: OutPoint) -> Result<TxOut> {
        let txout = self
            .rpc
            .get_tx_out(outpoint.txid, u64::from(outpoint.vout))?
            .into_model()?;
        Ok(txout.tx_out)
    }
}

impl AggregatingWallet for CoreWallet {
    fn reserve_nonce(&self, outpoint: OutPoint) -> Result<[u8; cisa::PUB_NONCE_LEN]> {
        let reply: serde_json::Value = self
            .rpc
            .call(
                "reservecisanonce",
                &[json!(outpoint.txid), json!(outpoint.vout)],
            )
            .context("reservecisanonce")?;
        let hex = reply["pubnonce"]
            .as_str()
            .ok_or_else(|| anyhow!("no pubnonce in reply"))?;
        let bytes = Vec::<u8>::from_hex(hex)?;
        bytes
            .try_into()
            .map_err(|_| anyhow!("public nonce is not {} bytes", cisa::PUB_NONCE_LEN))
    }

    fn process_psbt(&self, psbt: &Psbt) -> Result<Psbt> {
        let processed = self
            .rpc
            .call::<WalletProcessPsbt>(
                "walletprocesspsbt",
                &[
                    json!(psbt.to_string()),
                    json!(true),
                    json!(null),
                    json!(false),
                ],
            )
            .context("walletprocesspsbt")?;
        Ok(Psbt::from_str(&processed.psbt)?)
    }

    fn is_mine(&self, script: &Script) -> Result<bool> {
        let Ok(address) = Address::from_script(script, self.network) else {
            return Ok(false);
        };
        Ok(self.rpc.get_address_info(&address)?.is_mine)
    }
}

use bitcoin::hex::FromHex;

#[cfg(test)]
pub(crate) mod tests {
    use std::cell::Cell;
    use std::collections::BTreeSet;

    use bitcoin::hashes::Hash;
    use bitcoin::{ScriptBuf, Sequence, Transaction, TxIn, Txid};

    use super::*;

    /// A wallet that owns a fixed set of scripts and hands out one
    /// deterministic public nonce per outpoint. Test only.
    pub(crate) struct FakeWallet {
        owned: BTreeSet<ScriptBuf>,
        reservations: Cell<usize>,
    }

    impl FakeWallet {
        pub(crate) fn owning(scripts: impl IntoIterator<Item = ScriptBuf>) -> Self {
            Self {
                owned: scripts.into_iter().collect(),
                reservations: Cell::new(0),
            }
        }

        pub(crate) fn reservations(&self) -> usize {
            self.reservations.get()
        }

        pub(crate) fn nonce_for(&self, outpoint: OutPoint) -> [u8; cisa::PUB_NONCE_LEN] {
            let mut nonce = [0u8; cisa::PUB_NONCE_LEN];
            nonce[..32].copy_from_slice(outpoint.txid.as_byte_array());
            nonce[32..36].copy_from_slice(&outpoint.vout.to_le_bytes());
            nonce
        }
    }

    impl AggregatingWallet for FakeWallet {
        fn reserve_nonce(&self, outpoint: OutPoint) -> Result<[u8; cisa::PUB_NONCE_LEN]> {
            self.reservations.set(self.reservations.get() + 1);
            Ok(self.nonce_for(outpoint))
        }

        fn process_psbt(&self, psbt: &Psbt) -> Result<Psbt> {
            Ok(psbt.clone())
        }

        fn is_mine(&self, script: &Script) -> Result<bool> {
            Ok(self.owned.contains(script))
        }
    }

    /// An unsigned PSBT with one input per script, each with a witness UTXO.
    pub(crate) fn psbt_spending(scripts: impl IntoIterator<Item = ScriptBuf>) -> Psbt {
        let scripts: Vec<_> = scripts.into_iter().collect();
        let tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: (0..scripts.len())
                .map(|i| TxIn {
                    previous_output: OutPoint {
                        txid: Txid::from_byte_array([i as u8 + 1; 32]),
                        vout: 0,
                    },
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    ..Default::default()
                })
                .collect(),
            output: vec![],
        };
        let mut psbt = Psbt::from_unsigned_tx(tx).unwrap();
        for (input, script) in psbt.inputs.iter_mut().zip(scripts) {
            input.witness_utxo = Some(TxOut {
                value: Amount::from_sat(100_000),
                script_pubkey: script,
            });
        }
        psbt
    }
}
