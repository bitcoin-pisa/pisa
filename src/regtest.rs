//! A complete sender and receiver round trip on a private regtest node.
//!
//! The node comes from `BITCOIND_EXE` and must validate BIP 460. The payjoin
//! directory and OHTTP relay run in-process. Both wallets hold witness
//! version 2 coins under a `cisa()` descriptor.

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::{Amount, FeeRate, Network, Psbt, Transaction};
use payjoin::cisa;
use payjoin::persist::OptionalTransitionOutcome;
use payjoin::receive::v2::{PayjoinProposal, Receiver, ReceiverBuilder, UncheckedOriginalPayload};
use payjoin::send::v2::SenderBuilder;
use payjoin::{ImplementationError, Request, Uri};
use payjoin_test_utils::corepc_node::{self, Node};
use payjoin_test_utils::{InMemoryPersister, TestServices};
use serde_json::json;

use crate::sender::declare_fullagg;
use crate::wallet::{AggregatingWallet, CoreWallet};

/// A test-only extended private key, the one Bitcoin Core's own wallet tests
/// use. Regtest coins only.
const TPRV: &str = "tprv8ZgxMBicQKsPd7Uf69XL1XwhmjHopUGep8GuEiJDZmbQz6o58LninorQAfcKZWARbtRtfnLcJ5MQ2AtHcQJCCRUcMRvmDUjyEmNUWwx8UbK";

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
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(run())
    }
}

async fn run() -> Result<RoundTrip> {
    let node = start_node()?;
    tracing::info!("regtest node started from BITCOIND_EXE, BIP 460 active");
    let funder = node.create_wallet("funder")?;
    let mine_to = funder.new_address()?;
    node.client.generate_to_address(101, &mine_to)?;

    let sender = cisa_wallet(&node, "sender", 0)?;
    let receiver = cisa_wallet(&node, "receiver", 1)?;
    for wallet in [&sender, &receiver] {
        let address = wallet.get_new_address()?;
        funder.send_to_address(&address, Amount::from_btc(10.0)?)?;
        tracing::info!("funded {address} (witness v2, cisa() descriptor) with 10 BTC");
    }
    node.client.generate_to_address(1, &mine_to)?;

    let services = TestServices::initialize().await.map_err(|e| anyhow!(e))?;
    services
        .wait_for_services_ready()
        .await
        .map_err(|e| anyhow!(e))?;
    let agent = http_agent(&services)?;
    let relay = services.ohttp_relay_url();
    tracing::info!(
        "payjoin directory {} and OHTTP relay {relay} running in-process",
        services.directory_url()
    );
    let recv_persister = InMemoryPersister::default();
    let send_persister = InMemoryPersister::default();

    // Receiver: open a session and publish its URI.
    let ohttp_keys = services.fetch_ohttp_keys().await?;
    let session = ReceiverBuilder::new(
        receiver.get_new_address()?,
        services.directory_url().as_str(),
        ohttp_keys,
    )?
    .build()
    .save(&recv_persister)?;
    let pj_uri = Uri::from_str(&session.pj_uri().to_string())
        .map_err(|e| anyhow!("{e}"))?
        .assume_checked()
        .check_pj_supported()
        .map_err(|e| anyhow!("{e}"))?;
    tracing::info!("receiver: BIP 77 session open, URI {pj_uri}");

    // Sender: sign the fallback, then declare its input for aggregation and
    // post the original PSBT.
    let mut original = sender.create_psbt(
        &[(pj_uri.address().clone(), Amount::ONE_BTC)],
        FeeRate::from_sat_per_vb_u32(2),
    )?;
    tracing::info!(
        "sender: fallback transaction signed, {} input(s), pays 1 BTC",
        original.inputs.len()
    );
    let declared = declare_fullagg(&mut original, &sender)?;
    if declared == 0 {
        bail!("the sender's original PSBT has no witness v2 input to aggregate");
    }
    tracing::info!("sender: {declared} input(s) declared for full aggregation");
    tracing::info!("sender: original PSBT {}", describe_inputs(&original));
    let req_ctx = SenderBuilder::new(original, pj_uri)
        .build_recommended(FeeRate::BROADCAST_MIN)?
        .save(&send_persister)?;
    let (
        Request {
            url,
            body,
            content_type,
            ..
        },
        post_ctx,
    ) = req_ctx.create_v2_post_request(relay.as_str())?;
    let response = agent
        .post(url)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await?;
    if !response.status().is_success() {
        bail!("posting the original PSBT failed: {}", response.status());
    }
    let send_ctx = req_ctx
        .process_response(&response.bytes().await?, post_ctx)
        .save(&send_persister)?;
    tracing::info!("sender: original PSBT posted to the directory through the relay");

    // Receiver: fetch the original PSBT, build, sign and post the proposal.
    let (req, ctx) = session.create_poll_request(relay.as_str())?;
    let response = agent
        .post(req.url)
        .header("Content-Type", req.content_type)
        .body(req.body)
        .send()
        .await?;
    let original = match session
        .process_response(response.bytes().await?.to_vec().as_slice(), ctx)
        .save(&recv_persister)?
    {
        OptionalTransitionOutcome::Progress(original) => original,
        OptionalTransitionOutcome::Stasis(_) => bail!("the directory returned no original PSBT"),
    };
    tracing::info!("receiver: original PSBT fetched, BIP 78 checks passed");
    let proposal = build_proposal(&receiver, original, &recv_persister)?;
    tracing::info!("receiver: proposal {}", describe_inputs(proposal.psbt()));
    let (req, ctx) = proposal.create_post_request(relay.as_str())?;
    let response = agent
        .post(req.url)
        .header("Content-Type", req.content_type)
        .body(req.body)
        .send()
        .await?;
    proposal
        .process_response(&response.bytes().await?, ctx)
        .save(&recv_persister)?;
    tracing::info!("receiver: proposal posted, session done, no nonce state kept");

    // Sender: fetch the proposal, check it, sign, aggregate, broadcast.
    let (
        Request {
            url,
            body,
            content_type,
            ..
        },
        poll_ctx,
    ) = send_ctx.create_poll_request(relay.as_str())?;
    let response = agent
        .post(url)
        .header("Content-Type", content_type)
        .body(body)
        .send()
        .await?;
    let checked = match send_ctx
        .process_response(&response.bytes().await?, poll_ctx)
        .save(&send_persister)?
    {
        OptionalTransitionOutcome::Progress(psbt) => psbt,
        OptionalTransitionOutcome::Stasis(_) => bail!("the directory returned no proposal"),
    };
    tracing::info!("sender: proposal fetched, own nonce and mode fields intact");
    let signed = sender.process_psbt(&checked)?;
    tracing::info!("sender: signed with the reserved secret nonce and aggregated the group");
    let finalized = sender.rpc().finalize_psbt(&signed)?;
    let Some(finalized) = finalized.psbt else {
        bail!("the proposal did not finalize")
    };
    let transaction = Psbt::from_str(&finalized)?.extract_tx()?;
    for (index, input) in transaction.input.iter().enumerate() {
        let sizes: Vec<usize> = input.witness.iter().map(<[u8]>::len).collect();
        tracing::info!("final witness of input {index}: element sizes {sizes:?}");
    }
    let txid = sender.broadcast_tx(&transaction)?;
    tracing::info!("sender: broadcast {txid}");

    node.client.generate_to_address(1, &mine_to)?;
    let mined = node
        .client
        .get_raw_transaction_verbose(txid)?
        .into_model()?;
    tracing::info!(
        "node: mined with {} confirmation(s), aggregate signature verified by consensus",
        mined.confirmations.unwrap_or_default()
    );
    Ok(RoundTrip {
        transaction: mined.transaction,
        confirmations: mined.confirmations.unwrap_or_default() as u32,
    })
}

/// One line per input naming the aggregation fields it carries, so a log of
/// the round trip shows what each message added.
fn describe_inputs(psbt: &Psbt) -> String {
    let inputs: Vec<String> = psbt
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let mut fields = Vec::new();
            if let Some(mode) = cisa::mode(input) {
                fields.push(format!("mode 0x{mode:02x}"));
            }
            if let Some(nonce) = cisa::fullagg_pub_nonce(input) {
                fields.push(format!("pubnonce {} bytes", nonce.len()));
            }
            if let Some(sig) = cisa::fullagg_partial_sig(input) {
                fields.push(format!("partial sig {} bytes", sig.len()));
            }
            if fields.is_empty() {
                format!("input {index}: no aggregation fields")
            } else {
                format!("input {index}: {}", fields.join(", "))
            }
        })
        .collect();
    format!("has {} input(s); {}", psbt.inputs.len(), inputs.join("; "))
}

fn start_node() -> Result<Node> {
    let exe = corepc_node::exe_path().context("BITCOIND_EXE must name a BIP 460 build")?;
    let mut conf = corepc_node::Conf::default();
    conf.args.push("-txindex");
    Node::with_conf(exe, &conf)
}

/// A wallet whose active bech32m descriptor is a `cisa()` descriptor under
/// `account`, so that new addresses are witness version 2.
fn cisa_wallet(node: &Node, name: &str, account: u32) -> Result<CoreWallet> {
    let rpc = node.create_wallet(name)?;
    let descriptor = format!("cisa({TPRV}/86h/1h/{account}h/<0;1>/*)");
    let info: serde_json::Value = rpc.call("getdescriptorinfo", &[json!(descriptor)])?;
    let checksum = info["checksum"]
        .as_str()
        .ok_or_else(|| anyhow!("no checksum"))?;
    let descriptor = format!("{descriptor}#{checksum}");
    let imported: serde_json::Value = rpc.call(
        "importdescriptors",
        &[json!([{ "desc": descriptor, "active": true, "timestamp": "now" }])],
    )?;
    if imported[0]["success"] != json!(true) {
        bail!("importing the descriptor failed: {imported}");
    }
    Ok(CoreWallet::new(rpc, Network::Regtest))
}

/// The typestate callbacks take a boxed error, not an `anyhow` one.
fn implementation_error(e: anyhow::Error) -> ImplementationError {
    ImplementationError::from(e.to_string().as_str())
}

fn http_agent(services: &TestServices) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .add_root_certificate(reqwest::Certificate::from_der(&services.cert())?)
        .build()?)
}

/// The receiver's checks and contribution, as in BIP 78, with its witness
/// version 2 coin joining the full-aggregation group.
fn build_proposal(
    receiver: &CoreWallet,
    original: Receiver<UncheckedOriginalPayload>,
    persister: &InMemoryPersister<payjoin::receive::v2::SessionEvent>,
) -> Result<Receiver<PayjoinProposal>> {
    let proposal = original
        .check_broadcast_suitability(None, |tx| {
            receiver.can_broadcast(tx).map_err(implementation_error)
        })
        .save(persister)?;
    let proposal = proposal
        .check_inputs_not_owned(&mut |outpoint| {
            let txout = receiver
                .get_txout(*outpoint)
                .map_err(implementation_error)?;
            receiver
                .is_mine(&txout.script_pubkey)
                .map_err(implementation_error)
        })
        .save(persister)?;
    let proposal = proposal
        .check_no_inputs_seen_before(&mut |_| Ok(false))
        .save(persister)?
        .identify_receiver_outputs(&mut |script| {
            receiver.is_mine(script).map_err(implementation_error)
        })
        .save(persister)?;
    let proposal = proposal.commit_outputs().save(persister)?;

    let candidates = receiver.list_unspent()?;
    let selected = proposal
        .try_preserving_privacy(candidates)
        .map_err(|e| anyhow!("coin selection failed: {e:?}"))?;
    let proposal = proposal
        .contribute_inputs(vec![selected])
        .map_err(|e| anyhow!("contributing inputs failed: {e:?}"))?
        .commit_inputs()
        .save(persister)?;
    let proposal = proposal
        .apply_fee_range(
            Some(FeeRate::BROADCAST_MIN),
            Some(FeeRate::from_sat_per_vb_u32(2)),
        )
        .save(persister)?;

    // The wallet signs the receiver's input in one call: every nonce of the
    // group is in the PSBT already, the sender's from the original PSBT and
    // the receiver's from the reservation made when the input was chosen.
    let proposal = proposal
        .finalize_proposal(|psbt: &Psbt| receiver.process_psbt(psbt).map_err(implementation_error))
        .save(persister)?;
    Ok(proposal)
}
