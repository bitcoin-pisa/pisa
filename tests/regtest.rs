//! The end-to-end gate: a BIP 77 payjoin between two witness version 2
//! wallets whose inputs form one BIP 460 full-aggregation group, mined on a
//! regtest node that validates BIP 460.
//!
//! Needs `BITCOIND_EXE` to point at such a node, so it is ignored by default:
//!
//! ```sh
//! BITCOIND_EXE=/path/to/bitcoind cargo test --test regtest -- --include-ignored
//! ```

use pisa::regtest::RoundTrip;

#[test]
#[ignore = "needs BITCOIND_EXE pointing at a BIP 460 build of Bitcoin Core"]
fn fullagg_payjoin_round_trip() {
    let outcome = RoundTrip::run().expect("round trip");

    // Both parties' inputs are in the group: one empty member witness and one
    // 65-byte final witness carrying the aggregate signature and the marker.
    let mut sizes: Vec<Vec<usize>> = outcome
        .transaction
        .input
        .iter()
        .map(|input| input.witness.iter().map(<[u8]>::len).collect())
        .collect();
    sizes.sort();
    assert_eq!(sizes, vec![vec![0], vec![65]]);
    assert_eq!(outcome.transaction.input.len(), 2);

    // The node accepted it into a block, so its consensus rules verified the
    // aggregate signature.
    assert_eq!(outcome.confirmations, 1);
}
