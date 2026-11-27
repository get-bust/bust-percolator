//! The authority mark and the permissionless crank that moves the effective price toward it.

use crate::ctx::{eng, pk, Ctx};
use anyhow::Result;
use percolator_prog::{
    ix::{CrankObservationHint, Instruction as Ix},
    state,
};
use serde_json::Value;
use solana_sdk::{instruction::AccountMeta, instruction::Instruction, pubkey::Pubkey, signature::Signer};
use std::{
    collections::VecDeque,
    thread,
    time::{Duration, Instant},
};

/// Pushes one authority mark for asset 0. The engine does not jump to it: each crank moves the effective price toward
/// the mark by at most the per-slot cap times the slots it accrues.
pub fn push(c: &Ctx, st: &Value, mark: u64) -> Result<String> {
    let market = pk(st, "market")?;
    let data = c.data(&market)?;
    let (_, group) = eng(state::read_market(&data))?;
    let seq = eng(state::read_asset_control_sequences(&data, 0))?;
    c.send(
        vec![c.prog(
            Ix::PushAuthMark {
                asset_index: 0,
                market_id: group.assets[0].market_id,
                now_slot: c.rpc.get_slot()?.max(group.current_slot),
                mark_e6: mark,
                observation_sequence: seq.oracle_observation + 1,
                authority_epoch: seq.authority_epoch,
            },
            vec![AccountMeta::new(c.house.pubkey(), true), AccountMeta::new(market, false)],
        )],
        &[&c.house],
    )
}

/// A crank of one portfolio at `slot`. An AuthMark asset is observed with no oracle accounts.
pub fn crank_ix(c: &Ctx, signer: &Pubkey, market: Pubkey, portfolio: Pubkey, slot: u64) -> Instruction {
    c.prog(
        Ix::PermissionlessCrank { now_slot: slot, observations: vec![CrankObservationHint { asset_index: 0, oracle_accounts: 0 }] },
        vec![AccountMeta::new(*signer, true), AccountMeta::new(market, false), AccountMeta::new(portfolio, false)],
    )
}

/// Cranks a portfolio: `trader`, `house`, or any portfolio key in the state file (an earlier trader's, kept under its
/// tag). Anyone may crank; the house pays here. A crank also expires a lapsed backing bucket the portfolio refers to.
pub fn crank(c: &Ctx, st: &Value, who: &str) -> Result<String> {
    let portfolio = match who {
        "house" => pk(st, "housePortfolio")?,
        "trader" => pk(st, "traderPortfolio")?,
        key => pk(st, key)?,
    };
    let ix = crank_ix(c, &c.house.pubkey(), pk(st, "market")?, portfolio, c.rpc.get_slot()?);
    c.send(vec![ix], &[&c.house])
}

/// The PumpSwap pool's price in collateral atoms per contract (1,000,000 coins), from its two vaults in
/// state/pool.json (written by scripts/pool.cjs).
pub fn pool_spot(c: &Ctx, pool: &Value) -> Result<f64> {
    let base = c.rpc.get_token_account_balance(&pk(pool, "baseVault")?)?.amount.parse::<f64>()?;
    let quote = c.rpc.get_token_account_balance(&pk(pool, "quoteVault")?)?.amount.parse::<f64>()?;
    Ok(quote / base * 1e12)
}

pub fn read_pool() -> Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(crate::ctx::root().join("state/pool.json"))?)?)
}

/// Time-weighted average price over the last `window`. Before its first sample it holds `seed`, the price when it
/// started, so a fresh pusher averages over a full window instead of over the few samples it has taken.
pub struct Twap {
    window: Duration,
    seed: f64,
    samples: VecDeque<(Instant, f64)>,
}

impl Twap {
    pub fn new(window_secs: u64, seed: f64) -> Self {
        Self { window: Duration::from_secs(window_secs), seed, samples: VecDeque::new() }
    }

    /// Adds a sample taken now and returns the average over the window ending now.
    pub fn add(&mut self, price: f64) -> f64 {
        let now = Instant::now();
        self.samples.push_back((now, price));
        let start = now.checked_sub(self.window).unwrap_or(now);
        // keep one sample at or before the window's start: it is the price at the start
        while self.samples.len() > 1 && self.samples[1].0 <= start {
            let (_, p) = self.samples.pop_front().unwrap();
            self.seed = p;
        }
        let mut at = start;
        let mut value = self.seed;
        let mut sum = 0.0;
        for &(t, p) in &self.samples {
            if t <= start {
                value = p;
                continue;
            }
            sum += value * t.duration_since(at).as_secs_f64();
            at = t;
            value = p;
        }
        sum += value * now.duration_since(at).as_secs_f64();
        sum / self.window.as_secs_f64()
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

/// Follows the pool: samples it every 2 s and pushes its time-weighted average over `window` seconds every 4 s.
pub fn pusher(c: &Ctx, st: &Value, window: u64) -> Result<()> {
    let pool = read_pool()?;
    let mut twap = Twap::new(window, pool_spot(c, &pool)?);
    let mut last = Instant::now() - Duration::from_secs(60);
    loop {
        let spot = pool_spot(c, &pool)?;
        let avg = twap.add(spot);
        if last.elapsed() >= Duration::from_secs(4) {
            match push(c, st, avg.round() as u64) {
                Ok(sig) => println!("spot {:.4} twap({window}s, n={}) {:.4} pushed {}", spot / 1e6, twap.len(), avg / 1e6, &sig[..12]),
                Err(e) => println!("push failed: {e}"),
            }
            last = Instant::now();
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// A crank's compute units: 64,683 on devnet for one that moves no price, 83,427 for one that does. How many fit a
/// transaction.
pub const CRANK_UNITS: u32 = 90_000;
const CRANKS_PER_TX: usize = 15;

/// How many slots the asset's accrual is behind the chain, and the most one crank moves it.
pub fn accrual_lag(c: &Ctx, market: &Pubkey) -> Result<(u64, u64)> {
    let (_, group) = eng(state::read_market(&c.data(market)?))?;
    let behind = c.rpc.get_slot()?.saturating_sub(group.assets[0].slot_last);
    Ok((behind, group.config.max_accrual_dt_slots.max(1)))
}

/// The portfolios that hold a leg of asset 0, with their data.
pub fn holder_accounts(c: &Ctx, st: &Value) -> Result<Vec<(Pubkey, Vec<u8>)>> {
    use solana_client::{
        rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
        rpc_filter::{Memcmp, RpcFilterType},
    };
    let market = pk(st, "market")?;
    let (_, group) = eng(state::read_market(&c.data(&market)?))?;
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(10, vec![bust_matcher::layout::KIND_PORTFOLIO])),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(bust_matcher::layout::PORTFOLIO_MARKET_OFF, market.to_bytes().to_vec())),
        ]),
        account_config: RpcAccountInfoConfig { encoding: Some(solana_account_decoder::UiAccountEncoding::Base64), ..Default::default() },
        ..Default::default()
    };
    Ok(c.rpc
        .get_program_accounts_with_config(&c.program, config)?
        .into_iter()
        .filter(|(_, a)| bust_matcher::layout::holds_leg(&a.data, 0, group.assets[0].market_id))
        .map(|(k, a)| (k, a.data))
        .collect())
}

/// The portfolios that hold a leg of asset 0.
pub fn holders(c: &Ctx, st: &Value) -> Result<Vec<Pubkey>> {
    Ok(holder_accounts(c, st)?.into_iter().map(|(k, _)| k).collect())
}

/// The holders whose leg was settled before its side's last K/F change: Percolator refuses any trade that adds risk on
/// the asset until each of them is cranked.
pub fn stale_holders(c: &Ctx, st: &Value) -> Result<Vec<Pubkey>> {
    let asset = bust_matcher::layout::asset(&c.data(&pk(st, "market")?)?, 0).ok_or_else(|| anyhow::anyhow!("not a market"))?;
    Ok(holder_accounts(c, st)?.into_iter().filter(|(_, d)| bust_matcher::layout::leg_stale(d, 0, &asset)).map(|(k, _)| k).collect())
}

/// What `settle` sent: transactions through, refused as having nothing to do (another crank did it first), failed.
#[derive(Default, Debug)]
pub struct Settled {
    pub ok: u32,
    pub idle: u32,
    pub failed: u32,
}

fn settle_send(c: &Ctx, ixs: Vec<Instruction>, units: u32, micro_lamports: u64, out: &mut Settled) {
    match c.send_with_budget(ixs, &[&c.house], units, micro_lamports) {
        Ok(_) => out.ok += 1,
        Err(e) if e.to_string().split_whitespace().collect::<String>().contains("Custom(22)") => out.idle += 1,
        Err(e) => {
            out.failed += 1;
            println!("settle: {}", crate::ctx::brief(&e));
        }
    }
}

/// Cranks that accrue the asset to the chain's slot, which moves its price toward the mark: as many as the gap since
/// its last accrual needs at `max_accrual_dt_slots` a crank, at most `cap`. None when nothing is left to accrue: such a
/// crank would fail. Each has slots to accrue when it lands, since the gap only grows until it does.
pub fn accrual_ixs(c: &Ctx, st: &Value, cap: usize) -> Result<Vec<Instruction>> {
    let market = pk(st, "market")?;
    let house = pk(st, "housePortfolio")?;
    let (behind, dt) = accrual_lag(c, &market)?;
    let n = (behind.div_ceil(dt) as usize).min(cap);
    let slot = c.rpc.get_slot()?;
    // the slot field is not read on chain; varying it keeps identical cranks in one transaction apart
    Ok((0..n as u64).map(|k| crank_ix(c, &c.house.pubkey(), market, house, slot + k)).collect())
}

/// Holders cranked in one transaction, at about 129,000 compute units each after a price move (measured on devnet).
/// After a move every holder has work, so a batch rarely meets a crank with nothing to do; when it does (a holder whose
/// own trade refreshed it first), only that batch is resent one by one.
const HOLDERS_PER_TX: usize = 8;

/// Sends each instruction list as a transaction of its own, all at once, and waits for them together; returns the
/// lists refused for a crank with nothing to do.
fn send_together(c: &Ctx, txs: Vec<Vec<Instruction>>, units: u32, micro_lamports: u64, out: &mut Settled) -> Result<Vec<Vec<Instruction>>> {
    use solana_client::rpc_config::RpcSendTransactionConfig;
    use solana_sdk::{compute_budget::ComputeBudgetInstruction, transaction::Transaction};
    if txs.is_empty() {
        return Ok(Vec::new());
    }
    let blockhash = c.rpc.get_latest_blockhash()?;
    let config = RpcSendTransactionConfig { skip_preflight: true, ..Default::default() };
    let mut sigs = Vec::new();
    for ixs in &txs {
        let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(units), ComputeBudgetInstruction::set_compute_unit_price(micro_lamports)];
        all.extend(ixs.iter().cloned());
        let tx = Transaction::new_signed_with_payer(&all, Some(&c.house.pubkey()), &[&c.house], blockhash);
        sigs.push(c.rpc.send_transaction_with_config(&tx, config)?);
    }
    let started = std::time::Instant::now();
    loop {
        let statuses = c.rpc.get_signature_statuses(&sigs)?.value;
        if statuses.iter().all(|s| s.as_ref().is_some_and(|s| s.satisfies_commitment(c.rpc.commitment()))) || started.elapsed().as_secs() > 30 {
            let mut idle = Vec::new();
            for (s, ixs) in statuses.into_iter().zip(txs) {
                match s.and_then(|s| s.err) {
                    None => out.ok += 1,
                    Some(e) if format!("{e:?}").contains("Custom(22)") => {
                        out.idle += 1;
                        idle.push(ixs);
                    }
                    Some(e) => {
                        out.failed += 1;
                        println!("settle: {e:?}");
                    }
                }
            }
            return Ok(idle);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Brings the market back to where a trade that adds risk goes through, after a mark: accrues the asset to the
/// market's slot, which moves its price toward the mark (a crank accrues at most `max_accrual_dt_slots`, so it sends
/// as many as the gap needs and checks), until the price has caught up and no accrual is left half done; then cranks
/// every holder the move left stale, each in a transaction of its own and all at once, so one that another crank
/// settled first (`EngineNonProgress`) does not take the others down with it. Between marks nothing moves, and nothing
/// needs it.
pub fn settle(c: &Ctx, st: &Value, micro_lamports: u64) -> Result<Settled> {
    let market = pk(st, "market")?;
    let mut out = Settled::default();
    for _ in 0..6 {
        let (_, group) = eng(state::read_market(&c.data(&market)?))?;
        let a = &group.assets[0];
        if a.raw_oracle_target_price == a.effective_price && group.current_slot == a.slot_last {
            break;
        }
        let ixs = accrual_ixs(c, st, 12)?;
        if ixs.is_empty() {
            // the price moves only with slots to accrue: wait for one
            std::thread::sleep(std::time::Duration::from_millis(400));
            continue;
        }
        // the crank that moves the price costs more than the rest, by how much is not known beforehand
        settle_send(c, ixs, 1_400_000, micro_lamports, &mut out);
    }
    // the asset's own counts say whether anyone is stale; a leg's epoch alone over-counts (it can trail its side's
    // epoch without being counted)
    let (_, group) = eng(state::read_market(&c.data(&market)?))?;
    if group.assets[0].stale_account_count_long + group.assets[0].stale_account_count_short == 0 {
        return Ok(out);
    }
    let slot = c.rpc.get_slot()?;
    // a holder's crank after a price move settles its profit and loss: more than a plain crank, so the whole budget
    let cranks: Vec<Instruction> = stale_holders(c, st)?.into_iter().map(|h| crank_ix(c, &c.house.pubkey(), market, h, slot)).collect();
    let batches = cranks.chunks(HOLDERS_PER_TX).map(|b| b.to_vec()).collect();
    let refused = send_together(c, batches, 1_400_000, micro_lamports, &mut out)?;
    // a batch with one holder already settled is refused whole: those holders one by one
    let singles = refused.into_iter().flatten().map(|ix| vec![ix]).collect::<Vec<_>>();
    if !singles.is_empty() {
        send_together(c, singles, 1_000_000, micro_lamports, &mut out)?;
    }
    Ok(out)
}

/// Brings an asset that was left uncranked back to the chain's slot. Percolator accrues an asset at most
/// `max_accrual_dt_slots` slots a crank; once the asset has open interest, a trade that adds risk is refused while it
/// is behind. Sends batches of transactions of 20 cranks without waiting on each, then checks how far it got.
pub fn catch_up(c: &Ctx, st: &Value) -> Result<()> {
    use solana_client::rpc_config::RpcSendTransactionConfig;
    use solana_sdk::{compute_budget::ComputeBudgetInstruction, transaction::Transaction};
    let market = pk(st, "market")?;
    let house = pk(st, "housePortfolio")?;
    let config = RpcSendTransactionConfig { skip_preflight: true, ..Default::default() };
    let mut nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() << 20;
    let started = std::time::Instant::now();
    loop {
        let (behind, dt) = accrual_lag(c, &market)?;
        println!("{:>5} s: the asset's accrual is {behind} slots behind ({} cranks)", started.elapsed().as_secs(), behind.div_ceil(dt));
        if behind <= 2 * dt {
            return Ok(());
        }
        let txs = (behind.div_ceil(dt) as usize).div_ceil(CRANKS_PER_TX).min(48);
        let blockhash = c.rpc.get_latest_blockhash()?;
        let mut last = None;
        for _ in 0..txs {
            let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(CRANK_UNITS * CRANKS_PER_TX as u32)];
            for _ in 0..CRANKS_PER_TX {
                nonce += 1;
                ixs.push(crank_ix(c, &c.house.pubkey(), market, house, nonce));
            }
            let tx = Transaction::new_signed_with_payer(&ixs, Some(&c.house.pubkey()), &[&c.house], blockhash);
            last = Some(c.rpc.send_transaction_with_config(&tx, config)?);
        }
        if let Some(sig) = last {
            let started = std::time::Instant::now();
            while !c.rpc.confirm_transaction(&sig).unwrap_or(false) && started.elapsed().as_secs() < 30 {
                std::thread::sleep(std::time::Duration::from_millis(400));
            }
        }
    }
}


