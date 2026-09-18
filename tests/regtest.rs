//! The end-to-end gate: BIP 77 payjoins between witness version 2 wallets
//! whose inputs form one BIP 460 full-aggregation group, mined on a regtest
//! node that validates BIP 460.
//!
//! Needs `BITCOIND_EXE` to point at such a node, so these are ignored by
//! default:
//!
//! ```sh
//! BITCOIND_EXE=/path/to/bitcoind cargo test --test regtest -- --include-ignored
//! ```

use pisa::regtest::{CutThrough, Exchange, RoundTrip};

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

#[test]
#[ignore = "needs BITCOIND_EXE pointing at a BIP 460 build of Bitcoin Core"]
fn cut_through_pays_withdrawals_out_of_a_deposit() {
    let exchange = Exchange::default();
    let outcome = CutThrough::run(exchange).expect("cut-through");

    let aggregated = &outcome.aggregated.transaction;
    // One input from the customer and one per top-up, all in one group, so
    // every input but the group's last carries an empty witness.
    assert_eq!(aggregated.input.len(), exchange.top_up + 1);
    let mut sizes: Vec<Vec<usize>> = aggregated
        .input
        .iter()
        .map(|input| input.witness.iter().map(<[u8]>::len).collect())
        .collect();
    sizes.sort();
    let mut expected = vec![vec![0]; exchange.top_up];
    expected.push(vec![65]);
    assert_eq!(sizes, expected);

    // The withdrawals, the exchange's change and the customer's change. The
    // deposit output the customer paid to is gone: that is the cut-through.
    assert_eq!(aggregated.output.len(), exchange.withdrawals + 2);
    assert_eq!(outcome.aggregated.confirmations, 1);

    // The same transaction with each input signed on its own, for the
    // comparison the demo prints. Aggregation drops a 64-byte signature from
    // every input but the group's last, and costs one weight unit back on
    // that last one for the BIP 460 marker byte its witness carries.
    let separate = &outcome.separate.transaction;
    assert_eq!(separate.input.len(), aggregated.input.len());
    assert_eq!(separate.output.len(), aggregated.output.len());
    assert_eq!(outcome.separate.confirmations, 1);
    assert_eq!(
        (separate.weight() - aggregated.weight()).to_wu(),
        64 * exchange.top_up as u64 - 1,
    );

    // Both are cheaper than the deposit and the batch withdrawal an exchange
    // without payjoin would have to make.
    assert!(separate.weight() < outcome.without_payjoin);
    assert!(aggregated.weight() < separate.weight());

    // BIP 78 has the customer pay for the transaction it would have made
    // alone and the receiver for what it added, so both shares are non-zero
    // and together they are the fee.
    assert!(outcome.aggregated.customer_fee > bitcoin::Amount::ZERO);
    assert!(outcome.aggregated.exchange_fee > bitcoin::Amount::ZERO);
    assert_eq!(
        outcome.aggregated.customer_fee + outcome.aggregated.exchange_fee,
        outcome.aggregated.fee,
    );
}
