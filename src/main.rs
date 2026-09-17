//! Run the regtest round trip and print the resulting transaction.

use pisa::regtest::RoundTrip;

fn main() -> anyhow::Result<()> {
    let outcome = RoundTrip::run()?;
    println!("txid {}", outcome.transaction.compute_txid());
    println!(
        "{}",
        bitcoin::consensus::encode::serialize_hex(&outcome.transaction)
    );
    Ok(())
}
