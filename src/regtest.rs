//! Complete sender and receiver round trips on a private regtest node.
//!
//! The node comes from `BITCOIND_EXE` and must validate BIP 460. The payjoin
//! directory and OHTTP relay run in-process. Every wallet holds witness
//! version 2 coins under a `cisa()` descriptor.
//!
//! Two scenarios run here. [`RoundTrip`] is a plain two-party payment where
//! the receiver adds one coin. [`CutThrough`] is an exchange folding its
//! pending withdrawals into a customer's deposit, which is the case where
//! aggregation is worth money: it puts several of the exchange's coins into
//! one transaction, and every coin beyond the first saves a signature.

use std::str::FromStr;

use anyhow::{anyhow, bail, Context, Result};
use bitcoin::{Address, Amount, FeeRate, Network, Psbt, Script, Transaction, TxOut, Weight};
use payjoin::cisa;
use payjoin::persist::OptionalTransitionOutcome;
use payjoin::receive::v2::{
    Initialized, PayjoinProposal, ProvisionalProposal, Receiver, ReceiverBuilder, SessionEvent,
    UncheckedOriginalPayload, WantsOutputs,
};
use payjoin::send::v2::SenderBuilder;
use payjoin::{ImplementationError, OhttpKeys, PjUri, Request, Uri};
use payjoin_test_utils::corepc_node::{self, Client, Node};
use payjoin_test_utils::{InMemoryPersister, TestServices};
use serde_json::json;

use crate::cost;
use crate::receiver::Spend;
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
    /// The node that mined it. It stops when this is dropped, so a caller
    /// that wants to inspect the chain afterwards, with an explorer for
    /// instance, holds on to it.
    pub node: Node,
}

impl RoundTrip {
    /// Run the round trip from nothing: start a node, fund two wallets, pay.
    pub fn run() -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(pay())
    }
}

/// An exchange that pays its pending withdrawals out of a customer's
/// deposit.
///
/// Without payjoin the deposit sits in an exchange UTXO until a later batch
/// spends it. Here the withdrawal outputs go straight into the deposit
/// transaction, and the exchange spends `top_up` coins of its own alongside
/// it. Those coins and the customer's join one full-aggregation group, which
/// is the whole point: a group of one saves nothing.
///
/// At the defaults the withdrawals come to more than the deposit, so the
/// exchange has to spend its own coins whether it wants the group or not.
/// Nothing here requires that; whatever the withdrawals do not consume comes
/// back to the exchange as change.
#[derive(Clone, Copy, Debug)]
pub struct Exchange {
    /// Withdrawals folded into the deposit.
    pub withdrawals: usize,
    /// Coins of its own the exchange spends to cover them.
    pub top_up: usize,
    /// What the customer deposits.
    pub deposit: Amount,
    /// What each withdrawal pays.
    pub withdrawal: Amount,
    /// The fee rate the customer builds its deposit at.
    pub fee_rate: FeeRate,
}

impl Default for Exchange {
    fn default() -> Self {
        Self {
            withdrawals: 4,
            top_up: 5,
            deposit: Amount::ONE_BTC,
            withdrawal: Amount::from_sat(35_000_000),
            fee_rate: FeeRate::from_sat_per_vb_u32(10),
        }
    }
}

impl Exchange {
    /// The fee rate floor the customer asks for in its payjoin request.
    ///
    /// It has to be below the rate the deposit itself was built at. Under
    /// full aggregation the group's last input carries the aggregate
    /// signature and the BIP 460 marker byte, one weight unit more than the
    /// signature the fallback carried, and BIP 78's fee arithmetic leaves
    /// that unit unpaid: the customer covers the transaction it would have
    /// made alone, the exchange covers exactly the weight it adds. A floor
    /// equal to the deposit's fee rate would make the customer reject its
    /// own payjoin over that quarter of a virtual byte.
    fn floor(&self) -> FeeRate {
        let step = FeeRate::BROADCAST_MIN.to_sat_per_kwu();
        FeeRate::from_sat_per_kwu(
            self.fee_rate
                .to_sat_per_kwu()
                .saturating_sub(step)
                .max(step),
        )
    }

    /// What the exchange owes its customers.
    fn owed(&self) -> Amount {
        self.withdrawal * self.withdrawals as u64
    }

    /// What each of the exchange's own coins holds.
    ///
    /// What each of the exchange's own coins holds.
    ///
    /// Together they come to what it owes, which leaves the deposit to pay
    /// the fee and makes the exchange's change roughly the deposit again.
    /// Any split of the same total works; this one holds for whatever
    /// `withdrawals` and `top_up` the run is given, so only the fee is left
    /// for [`validate`](Self::validate) to worry about.
    fn coin(&self) -> Amount {
        self.owed() / self.top_up as u64
    }

    /// An upper bound on the weight the exchange adds to the customer's
    /// transaction, and so on the fee it pays for adding it.
    ///
    /// Its inputs cost a key path spend each when they are not in a group,
    /// which is one of the two runs, so the bound is taken there.
    fn added_weight(&self) -> Weight {
        cost::KEY_PATH_INPUT * self.top_up as u64 + cost::OUTPUT * self.withdrawals as u64
    }

    fn validate(&self) -> Result<()> {
        if self.withdrawals == 0 {
            bail!("the exchange needs at least one withdrawal to pay out");
        }
        if self.top_up == 0 {
            bail!("the exchange needs at least one coin of its own to add");
        }
        let fee = self
            .fee_rate
            .fee_wu(self.added_weight())
            .context("fee rate times added weight overflows")?;
        let available = self.deposit + self.coin() * self.top_up as u64;
        if available < self.owed() + fee {
            bail!(
                "{} withdrawals of {} come to {}, more than a {} deposit and {} coins of {} cover at {} sat/vB",
                self.withdrawals,
                self.withdrawal,
                self.owed(),
                self.deposit,
                self.top_up,
                self.coin(),
                self.fee_rate.to_sat_per_vb_ceil(),
            );
        }
        Ok(())
    }
}

/// One mined payjoin, and what each party paid for it.
pub struct Mined {
    /// The transaction as mined.
    pub transaction: Transaction,
    /// Confirmations as seen by the node.
    pub confirmations: u32,
    /// The whole fee, which is the two shares below.
    pub fee: Amount,
    /// The customer's share of the fee: what its deposit would have cost on
    /// its own, plus what it offered towards the exchange's inputs.
    pub customer_fee: Amount,
    /// The exchange's share: the rest of its inputs, and all of the outputs
    /// it added.
    pub exchange_fee: Amount,
}

/// The cut-through, run twice on one chain so that both payjoin rows of the
/// comparison are measured rather than computed.
pub struct CutThrough {
    /// The scenario both runs were built from.
    pub exchange: Exchange,
    /// The run whose witness version 2 inputs form one group.
    pub aggregated: Mined,
    /// The same transaction with every input signed on its own.
    pub separate: Mined,
    /// What the two transactions an exchange without payjoin would make
    /// weigh. Computed, since the demo does not build them.
    pub without_payjoin: Weight,
    /// The node that mined both, kept for the same reason as
    /// [`RoundTrip::node`].
    pub node: Node,
}

impl CutThrough {
    /// Run the cut-through from nothing.
    pub fn run(exchange: Exchange) -> Result<Self> {
        let runtime = tokio::runtime::Runtime::new()?;
        runtime.block_on(cut_through(exchange))
    }
}

async fn pay() -> Result<RoundTrip> {
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

    let services = Services::start().await?;
    let persister = InMemoryPersister::default();
    let (session, pj_uri) = services.open_session(&receiver, &persister)?;
    tracing::info!("receiver: BIP 77 session open, URI {pj_uri}");

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

    let proposed = services
        .negotiate(
            Round {
                roles: ("sender", "receiver"),
                session,
                persister: &persister,
                original,
                pj_uri,
                floor: FeeRate::BROADCAST_MIN,
                spend: Spend::Aggregated,
                describe: describe_inputs,
            },
            |original, persister| add_one_coin(&receiver, original, persister),
        )
        .await?;

    let transaction = finalize(&sender, &proposed)?;
    let (transaction, confirmations) = broadcast_and_mine(&node, &sender, &mine_to, &transaction)?;
    Ok(RoundTrip {
        transaction,
        confirmations,
        node,
    })
}

async fn cut_through(exchange: Exchange) -> Result<CutThrough> {
    exchange.validate()?;
    let node = start_node()?;
    tracing::info!("regtest node started from BITCOIND_EXE, BIP 460 active");
    let funder = node.create_wallet("funder")?;
    let mine_to = funder.new_address()?;
    node.client.generate_to_address(101, &mine_to)?;
    let customers = CoreWallet::new(node.create_wallet("customers")?, Network::Regtest);
    let chain = Chain {
        node: &node,
        funder: &funder,
        mine_to: &mine_to,
        customers: &customers,
    };

    let services = Services::start().await?;
    let aggregated = deposit(&chain, &services, &exchange, Spend::Aggregated, 0).await?;
    let separate = deposit(&chain, &services, &exchange, Spend::Separate, 2).await?;

    Ok(CutThrough {
        exchange,
        aggregated,
        separate,
        without_payjoin: cost::without_payjoin(exchange.withdrawals, exchange.top_up),
        node,
    })
}

/// The regtest chain and the wallets that are not party to the payjoin: the
/// one that mines and funds, and the one that receives the withdrawals.
struct Chain<'a> {
    node: &'a Node,
    funder: &'a Client,
    mine_to: &'a Address,
    /// Pays out the withdrawals. Its addresses are bech32m, the same
    /// 32-byte witness program shape as everything else here, so that the
    /// measured rows and the computed one weigh their outputs alike.
    customers: &'a CoreWallet,
}

/// One deposit that pays the exchange's pending withdrawals, with the
/// exchange's own coins spent the given way.
///
/// `account` is the first of two unused branches of [`TPRV`], so that the
/// two runs hold different coins and reserve their nonces independently.
async fn deposit(
    chain: &Chain<'_>,
    services: &Services,
    exchange: &Exchange,
    spend: Spend,
    account: u32,
) -> Result<Mined> {
    let tag = match spend {
        Spend::Aggregated => "aggregated",
        Spend::Separate => "separate",
    };
    let customer = cisa_wallet(chain.node, &format!("customer-{tag}"), account)?;
    let wallet = cisa_wallet(chain.node, &format!("exchange-{tag}"), account + 1)?;

    // The customer needs one coin that covers its deposit and the fee; the
    // exchange needs one coin per top-up, so that it has several to add.
    let funding = exchange.deposit * 2;
    chain
        .funder
        .send_to_address(&customer.get_new_address()?, funding)?;
    for _ in 0..exchange.top_up {
        chain
            .funder
            .send_to_address(&wallet.get_new_address()?, exchange.coin())?;
    }
    chain.node.client.generate_to_address(1, chain.mine_to)?;
    tracing::info!(
        "customer funded with {funding}; exchange funded with {} coins of {}",
        exchange.top_up,
        exchange.coin()
    );

    let persister = InMemoryPersister::default();
    let (session, pj_uri) = services.open_session(&wallet, &persister)?;
    let deposit_address = pj_uri.address().clone();
    tracing::info!(
        "exchange: BIP 77 session open for a {} deposit",
        exchange.deposit
    );

    let mut original = customer.create_psbt(
        &[(deposit_address.clone(), exchange.deposit)],
        exchange.fee_rate,
    )?;
    tracing::info!(
        "customer: deposit of {} signed as a fallback, {} input(s), {} sat/vB",
        exchange.deposit,
        original.inputs.len(),
        exchange.fee_rate.to_sat_per_vb_ceil()
    );
    if spend == Spend::Aggregated {
        let declared = declare_fullagg(&mut original, &customer)?;
        if declared == 0 {
            bail!("the customer's deposit has no witness v2 input to aggregate");
        }
        tracing::info!("customer: {declared} input(s) declared for full aggregation");
    }
    let sent = original.clone();

    let proposed = services
        .negotiate(
            Round {
                roles: ("customer", "exchange"),
                session,
                persister: &persister,
                original,
                pj_uri,
                floor: exchange.floor(),
                spend,
                describe: summarize_inputs,
            },
            |original, persister| {
                cut(
                    &wallet,
                    chain.customers,
                    exchange,
                    spend,
                    original,
                    persister,
                )
            },
        )
        .await?;

    let (customer_fee, exchange_fee) =
        attribute_fee(&sent, &proposed, &deposit_address.script_pubkey())?;
    let fee = proposed.fee()?;
    tracing::info!(
        "fee paid by the customer {} sat, by the exchange {} sat",
        customer_fee.to_sat(),
        exchange_fee.to_sat()
    );

    let transaction = finalize(&customer, &proposed)?;
    let (transaction, confirmations) =
        broadcast_and_mine(chain.node, &customer, chain.mine_to, &transaction)?;
    Ok(Mined {
        transaction,
        confirmations,
        fee,
        customer_fee,
        exchange_fee,
    })
}

/// The two-party payment's contribution: one coin, chosen to avoid the
/// unnecessary input heuristic.
fn add_one_coin(
    wallet: &CoreWallet,
    original: Receiver<UncheckedOriginalPayload>,
    persister: &InMemoryPersister<SessionEvent>,
) -> Result<Receiver<PayjoinProposal>> {
    let proposal = checks(wallet, original, persister)?
        .commit_outputs()
        .save(persister)?;
    let candidates = wallet.list_unspent(Spend::Aggregated)?;
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
    sign(wallet, proposal, persister)
}

/// The exchange's contribution: the deposit output becomes the pending
/// withdrawals plus the exchange's own change, and the exchange's coins make
/// up what the deposit does not cover.
fn cut(
    wallet: &CoreWallet,
    customers: &CoreWallet,
    exchange: &Exchange,
    spend: Spend,
    original: Receiver<UncheckedOriginalPayload>,
    persister: &InMemoryPersister<SessionEvent>,
) -> Result<Receiver<PayjoinProposal>> {
    let mut outputs = Vec::with_capacity(exchange.withdrawals + 1);
    for _ in 0..exchange.withdrawals {
        outputs.push(TxOut {
            value: exchange.withdrawal,
            script_pubkey: customers.get_new_address()?.script_pubkey(),
        });
    }
    // The exchange's own output starts empty. Contributing inputs moves
    // whatever the withdrawals do not consume into it, so the exchange never
    // has to work out its change before it knows what it is spending.
    let drain = wallet.get_new_address()?.script_pubkey();
    outputs.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: drain.clone(),
    });

    let proposal = checks(wallet, original, persister)?
        .replace_receiver_outputs(outputs, &drain)
        .map_err(|e| anyhow!("replacing the deposit output failed: {e:?}"))?
        .commit_outputs()
        .save(persister)?;
    tracing::info!(
        "exchange: {} withdrawal(s) of {} replace the deposit output",
        exchange.withdrawals,
        exchange.withdrawal
    );

    let coins = wallet.list_unspent(spend)?;
    let contributed = coins.len();
    let proposal = proposal
        .contribute_inputs(coins)
        .map_err(|e| anyhow!("contributing inputs failed: {e:?}"))?
        .commit_inputs()
        .save(persister)?;
    tracing::info!(
        "exchange: topped up with {contributed} coin(s), {}",
        match spend {
            Spend::Aggregated => "all in one group",
            Spend::Separate => "each signed on its own",
        }
    );

    let proposal = proposal
        .apply_fee_range(Some(exchange.floor()), Some(exchange.fee_rate))
        .save(persister)?;
    sign(wallet, proposal, persister)
}

/// The receiver signs the inputs it contributed, in one call.
///
/// A group member gets a partial signature, which the wallet can produce
/// because every nonce of the group is in the PSBT already, the sender's
/// from the original PSBT and the receiver's from the reservations made when
/// the coins were chosen. An input that opted out gets an ordinary signature
/// and is finalized on the spot.
fn sign(
    wallet: &CoreWallet,
    proposal: Receiver<ProvisionalProposal>,
    persister: &InMemoryPersister<SessionEvent>,
) -> Result<Receiver<PayjoinProposal>> {
    Ok(proposal
        .finalize_proposal(|psbt: &Psbt| wallet.process_psbt(psbt).map_err(implementation_error))
        .save(persister)?)
}

/// The BIP 78 checks a receiver runs before it is allowed to change
/// anything.
fn checks(
    wallet: &CoreWallet,
    original: Receiver<UncheckedOriginalPayload>,
    persister: &InMemoryPersister<SessionEvent>,
) -> Result<Receiver<WantsOutputs>> {
    let proposal = original
        .check_broadcast_suitability(None, |tx| {
            wallet.can_broadcast(tx).map_err(implementation_error)
        })
        .save(persister)?;
    let proposal = proposal
        .check_inputs_not_owned(&mut |outpoint| {
            let txout = wallet.get_txout(*outpoint).map_err(implementation_error)?;
            wallet
                .is_mine(&txout.script_pubkey)
                .map_err(implementation_error)
        })
        .save(persister)?;
    Ok(proposal
        .check_no_inputs_seen_before(&mut |_| Ok(false))
        .save(persister)?
        .identify_receiver_outputs(&mut |script| {
            wallet.is_mine(script).map_err(implementation_error)
        })
        .save(persister)?)
}

/// What each party paid of the transaction's fee.
///
/// BIP 78 splits it at the sender's change output: the sender pays what its
/// own transaction would have cost, plus the contribution the receiver took
/// out of that output, and the receiver pays the rest.
fn attribute_fee(original: &Psbt, proposal: &Psbt, payee: &Script) -> Result<(Amount, Amount)> {
    let change = original
        .unsigned_tx
        .output
        .iter()
        .find(|txout| txout.script_pubkey != *payee)
        .context("the deposit has no change output to take a contribution from")?;
    let kept = proposal
        .unsigned_tx
        .output
        .iter()
        .find(|txout| txout.script_pubkey == change.script_pubkey)
        .context("the proposal dropped the customer's change output")?;
    let contribution = change
        .value
        .checked_sub(kept.value)
        .context("the proposal raised the customer's change")?;
    let sender = original.fee()? + contribution;
    let receiver = proposal
        .fee()?
        .checked_sub(sender)
        .context("the proposal pays less fee than the deposit alone would have")?;
    Ok((sender, receiver))
}

/// Sign the proposal, aggregate the group if there is one, and pull out the
/// transaction.
fn finalize(sender: &CoreWallet, proposed: &Psbt) -> Result<Transaction> {
    let signed = sender.process_psbt(proposed)?;
    let finalized = sender.rpc().finalize_psbt(&signed)?;
    let Some(finalized) = finalized.psbt else {
        bail!("the proposal did not finalize")
    };
    let transaction = Psbt::from_str(&finalized)?.extract_tx()?;
    let sizes: Vec<Vec<usize>> = transaction
        .input
        .iter()
        .map(|input| input.witness.iter().map(<[u8]>::len).collect())
        .collect();
    tracing::info!("final witnesses: element sizes {sizes:?}");
    Ok(transaction)
}

fn broadcast_and_mine(
    node: &Node,
    sender: &CoreWallet,
    mine_to: &Address,
    transaction: &Transaction,
) -> Result<(Transaction, u32)> {
    let txid = sender.broadcast_tx(transaction)?;
    tracing::info!("broadcast {txid}");
    node.client.generate_to_address(1, mine_to)?;
    let mined = node
        .client
        .get_raw_transaction_verbose(txid)?
        .into_model()?;
    let confirmations = mined.confirmations.unwrap_or_default() as u32;
    tracing::info!(
        "node: mined with {confirmations} confirmation(s), {} WU, signatures verified by consensus",
        mined.transaction.weight().to_wu()
    );
    Ok((mined.transaction, confirmations))
}

/// Everything one session needs besides the receiver's contribution.
struct Round<'a> {
    /// What to call the sender and the receiver in the log.
    roles: (&'a str, &'a str),
    session: Receiver<Initialized>,
    persister: &'a InMemoryPersister<SessionEvent>,
    original: Psbt,
    pj_uri: PjUri,
    /// The fee rate floor the sender puts in its request.
    floor: FeeRate,
    /// How the receiver spends its coins, which decides what the log says
    /// about the signatures.
    spend: Spend,
    /// How to render a PSBT's aggregation fields for the log.
    describe: fn(&Psbt) -> String,
}

/// The payjoin directory and OHTTP relay both parties talk to.
struct Services {
    inner: TestServices,
    agent: reqwest::Client,
    ohttp_keys: OhttpKeys,
}

impl Services {
    async fn start() -> Result<Self> {
        let inner = TestServices::initialize().await.map_err(|e| anyhow!(e))?;
        inner
            .wait_for_services_ready()
            .await
            .map_err(|e| anyhow!(e))?;
        let agent = reqwest::Client::builder()
            .no_proxy()
            .add_root_certificate(reqwest::Certificate::from_der(&inner.cert())?)
            .build()?;
        let ohttp_keys = inner.fetch_ohttp_keys().await?;
        tracing::info!(
            "payjoin directory {} and OHTTP relay {} running in-process",
            inner.directory_url(),
            inner.ohttp_relay_url()
        );
        Ok(Self {
            inner,
            agent,
            ohttp_keys,
        })
    }

    /// Open a receiver session paying to a fresh address of `wallet`.
    fn open_session(
        &self,
        wallet: &CoreWallet,
        persister: &InMemoryPersister<SessionEvent>,
    ) -> Result<(Receiver<Initialized>, PjUri)> {
        let session = ReceiverBuilder::new(
            wallet.get_new_address()?,
            self.inner.directory_url().as_str(),
            self.ohttp_keys.clone(),
        )?
        .build()
        .save(persister)?;
        let pj_uri = Uri::from_str(&session.pj_uri().to_string())
            .map_err(|e| anyhow!("{e}"))?
            .assume_checked()
            .check_pj_supported()
            .map_err(|e| anyhow!("{e}"))?;
        Ok((session, pj_uri))
    }

    async fn post(&self, request: Request) -> Result<Vec<u8>> {
        let response = self
            .agent
            .post(request.url)
            .header("Content-Type", request.content_type)
            .body(request.body)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!("the directory returned {}", response.status());
        }
        Ok(response.bytes().await?.to_vec())
    }

    /// Carry one payjoin between the two parties: the sender posts its
    /// original PSBT, the receiver answers with what `propose` builds, and
    /// the sender ends up with the proposal it has to sign.
    async fn negotiate(
        &self,
        round: Round<'_>,
        propose: impl FnOnce(
            Receiver<UncheckedOriginalPayload>,
            &InMemoryPersister<SessionEvent>,
        ) -> Result<Receiver<PayjoinProposal>>,
    ) -> Result<Psbt> {
        let (sender, receiver) = round.roles;
        let describe = round.describe;
        let persister = round.persister;
        let relay = self.inner.ohttp_relay_url();
        let send_persister = InMemoryPersister::default();

        tracing::info!("{sender}: original PSBT {}", describe(&round.original));
        let req_ctx = SenderBuilder::new(round.original, round.pj_uri)
            .build_recommended(round.floor)?
            .save(&send_persister)?;
        let (request, post_ctx) = req_ctx.create_v2_post_request(relay.as_str())?;
        let response = self.post(request).await?;
        let send_ctx = req_ctx
            .process_response(&response, post_ctx)
            .save(&send_persister)?;
        tracing::info!("{sender}: original PSBT posted to the directory through the relay");

        let (request, ctx) = round.session.create_poll_request(relay.as_str())?;
        let response = self.post(request).await?;
        let original = match round
            .session
            .process_response(&response, ctx)
            .save(persister)?
        {
            OptionalTransitionOutcome::Progress(original) => original,
            OptionalTransitionOutcome::Stasis(_) => {
                bail!("the directory returned no original PSBT")
            }
        };
        tracing::info!("{receiver}: original PSBT fetched, BIP 78 checks passed");

        let proposal = propose(original, persister)?;
        tracing::info!("{receiver}: proposal {}", describe(proposal.psbt()));
        let (request, ctx) = proposal.create_post_request(relay.as_str())?;
        let response = self.post(request).await?;
        proposal.process_response(&response, ctx).save(persister)?;
        match round.spend {
            Spend::Aggregated => {
                tracing::info!("{receiver}: proposal posted, session done, no nonce state kept")
            }
            Spend::Separate => {
                tracing::info!("{receiver}: proposal posted, its own inputs already signed")
            }
        }

        let (request, poll_ctx) = send_ctx.create_poll_request(relay.as_str())?;
        let response = self.post(request).await?;
        let checked = match send_ctx
            .process_response(&response, poll_ctx)
            .save(&send_persister)?
        {
            OptionalTransitionOutcome::Progress(psbt) => psbt,
            OptionalTransitionOutcome::Stasis(_) => bail!("the directory returned no proposal"),
        };
        match round.spend {
            Spend::Aggregated => tracing::info!(
                "{sender}: proposal fetched, own nonce and mode fields intact, aggregating"
            ),
            Spend::Separate => {
                tracing::info!("{sender}: proposal fetched, signing its own input")
            }
        }
        Ok(checked)
    }
}

/// The aggregation fields one input carries.
fn fields_of(input: &bitcoin::psbt::Input) -> String {
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
        "no aggregation fields".to_string()
    } else {
        fields.join(", ")
    }
}

/// One entry per input naming the aggregation fields it carries, so a log of
/// the round trip shows what each message added.
fn describe_inputs(psbt: &Psbt) -> String {
    let inputs: Vec<String> = psbt
        .inputs
        .iter()
        .enumerate()
        .map(|(index, input)| format!("input {index}: {}", fields_of(input)))
        .collect();
    format!("has {} input(s); {}", psbt.inputs.len(), inputs.join("; "))
}

/// How many inputs carry each field, for a transaction with more inputs than
/// a line per input would fit.
fn summarize_inputs(psbt: &Psbt) -> String {
    let count = |carries: fn(&bitcoin::psbt::Input) -> bool| {
        psbt.inputs.iter().filter(|input| carries(input)).count()
    };
    format!(
        "has {} input(s); {} in the group, {} with a public nonce, {} with a partial signature",
        psbt.inputs.len(),
        count(cisa::is_fullagg),
        count(|input| cisa::fullagg_pub_nonce(input).is_some()),
        count(|input| cisa::fullagg_partial_sig(input).is_some()),
    )
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
