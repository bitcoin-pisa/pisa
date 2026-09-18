//! Run a regtest scenario, narrate each step, and print what it produced.

use anyhow::{bail, Context, Result};
use bitcoin::{Amount, FeeRate, Transaction, Weight};
use pisa::regtest::{CutThrough, Exchange, RoundTrip};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
Usage: pisa-regtest [options]

  --cut-through        an exchange pays its pending withdrawals out of a
                       customer's deposit, instead of a plain payment
  --withdrawals <n>    withdrawals the exchange folds in (default 4)
  --top-up <n>         coins of its own the exchange adds (default 5)
  --fee-rate <n>       sat/vB the customer builds its deposit at (default 10)
  -h, --help           print this

Set PISA_KEEP_NODE to leave the regtest node running afterwards, so that a
block explorer can be pointed at it.";

enum Command {
    Pay,
    CutThrough(Exchange),
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "pisa=info".into()))
        .with_target(false)
        .without_time()
        .init();

    match parse(std::env::args().skip(1))? {
        None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(Command::Pay) => pay(),
        Some(Command::CutThrough(exchange)) => cut_through(exchange),
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Option<Command>> {
    let mut args = args.peekable();
    let mut cut_through = false;
    let mut exchange = Exchange::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| -> Result<u64> {
            args.next()
                .with_context(|| format!("{name} needs a number"))?
                .parse()
                .with_context(|| format!("{name} needs a number"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--cut-through" => cut_through = true,
            "--withdrawals" => exchange.withdrawals = value(&arg)? as usize,
            "--top-up" => exchange.top_up = value(&arg)? as usize,
            "--fee-rate" => {
                exchange.fee_rate =
                    FeeRate::from_sat_per_vb(value(&arg)?).context("--fee-rate is too large")?
            }
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    Ok(Some(if cut_through {
        Command::CutThrough(exchange)
    } else {
        Command::Pay
    }))
}

fn pay() -> Result<()> {
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

    keep_node(&outcome.node, None);
    Ok(())
}

fn cut_through(exchange: Exchange) -> Result<()> {
    let outcome = CutThrough::run(exchange)?;
    let tx = &outcome.aggregated.transaction;
    let aggregated = tx.weight();
    let separate = outcome.separate.transaction.weight();

    println!();
    println!(
        "{} deposit paying {} withdrawals of {}, topped up from {} exchange coins",
        exchange.deposit, exchange.withdrawals, exchange.withdrawal, exchange.top_up
    );
    println!();
    println!("txid          {}", tx.compute_txid());
    println!(
        "inputs        {}, of which {} carry an empty witness",
        tx.input.len(),
        unsigned_witnesses(tx)
    );
    println!(
        "outputs       {}: {} withdrawals, the exchange's change, the customer's change",
        tx.output.len(),
        exchange.withdrawals
    );
    println!("confirmations {}", outcome.aggregated.confirmations);
    println!();
    println!(
        "fee paid by the customer  {} sat, what its deposit alone would have cost plus its offer",
        outcome.aggregated.customer_fee.to_sat()
    );
    println!(
        "fee paid by the exchange  {} sat, for the inputs and the outputs it added",
        outcome.aggregated.exchange_fee.to_sat()
    );
    println!();

    let rate = exchange.fee_rate;
    println!(
        "{:48}{:>5}{:>18}",
        "",
        "vB",
        format!("fee at {} sat/vB", rate.to_sat_per_vb_ceil())
    );
    row(
        "no payjoin: deposit, then a batch",
        "computed",
        outcome.without_payjoin,
        rate,
    );
    row("payjoin, every input signed", "measured", separate, rate);
    row(
        "payjoin, one aggregate signature",
        "measured",
        aggregated,
        rate,
    );
    println!();
    println!(
        "  cutting through saves {} vB, aggregating the inputs another {} vB, {}% in all",
        vb(outcome.without_payjoin) - vb(separate),
        vb(separate) - vb(aggregated),
        100 * (vb(outcome.without_payjoin) - vb(aggregated)) / vb(outcome.without_payjoin)
    );

    keep_node(&outcome.node, Some(&tx.compute_txid().to_string()));
    Ok(())
}

fn row(label: &str, source: &str, weight: Weight, rate: FeeRate) {
    let fee = rate.fee_vb(vb(weight)).unwrap_or(Amount::ZERO);
    println!(
        "  {label:<36}{source:<10}{:>5}{:>18}",
        vb(weight),
        fee.to_sat()
    );
}

fn vb(weight: Weight) -> u64 {
    weight.to_vbytes_ceil()
}

/// Inputs whose witness carries no signature of its own. A member of a
/// full-aggregation group has one witness element and that element is empty,
/// so counting elements is not enough.
fn unsigned_witnesses(tx: &Transaction) -> usize {
    tx.input
        .iter()
        .filter(|input| input.witness.iter().all(<[u8]>::is_empty))
        .count()
}

/// With `PISA_KEEP_NODE` set the node stays up so that an explorer can be
/// pointed at it. Interrupt the process to stop it.
fn keep_node(node: &payjoin_test_utils::corepc_node::Node, txid: Option<&str>) {
    if std::env::var_os("PISA_KEEP_NODE").is_none() {
        return;
    }
    println!();
    if let Some(txid) = txid {
        println!("explore     /tx/{txid}");
    }
    println!("node rpc    {}", node.rpc_url());
    println!("node cookie {}", node.params.cookie_file.display());
    println!("node kept running, interrupt to stop");
    loop {
        std::thread::park();
    }
}
