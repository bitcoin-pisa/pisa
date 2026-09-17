//! Run the regtest round trip, narrate each step, and print the resulting
//! transaction.

use pisa::regtest::RoundTrip;
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "pisa=info".into()))
        .with_target(false)
        .without_time()
        .init();

    let outcome = RoundTrip::run()?;
    let tx = &outcome.transaction;

    println!();
    println!("txid    {}", tx.compute_txid());
    println!("weight  {} WU, {} vB", tx.weight().to_wu(), tx.vsize());
    for (index, input) in tx.input.iter().enumerate() {
        let sizes: Vec<usize> = input.witness.iter().map(<[u8]>::len).collect();
        println!(
            "input {index} {}:{} witness {sizes:?}",
            input.previous_output.txid, input.previous_output.vout
        );
    }
    for (index, output) in tx.output.iter().enumerate() {
        let kind = match output.script_pubkey.witness_version() {
            Some(version) => format!("witness v{}", version.to_num()),
            None => "non-segwit".to_string(),
        };
        println!("output {index} {} {kind}", output.value);
    }
    println!("confirmations {}", outcome.confirmations);
    println!();
    println!("{}", bitcoin::consensus::encode::serialize_hex(tx));
    Ok(())
}
