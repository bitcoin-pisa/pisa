//! A complete sender and receiver round trip on a private regtest node.

use anyhow::{bail, Result};
use bitcoin::Transaction;

/// What the round trip produced.
pub struct RoundTrip {
    /// The payjoin transaction as mined.
    pub transaction: Transaction,
    /// Confirmations of the transaction as seen by the node.
    pub confirmations: u32,
}

impl RoundTrip {
    /// Run the round trip from nothing: start a node, fund two wallets, pay.
    pub fn run() -> Result<Self> {
        bail!("not implemented")
    }
}
