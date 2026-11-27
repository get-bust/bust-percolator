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

/// The portfolios that hold a leg of asset 0: after the mark moves, every one of them has to be cranked before anyone
/// can add risk to the asset (Percolator counts their positions as stale until then).
pub fn holders(c: &Ctx, st: &Value) -> Result<Vec<Pubkey>> {
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
    let accounts = c.rpc.get_program_accounts_with_config(&c.program, config)?;
    Ok(accounts
        .into_iter()
        .filter(|(_, a)| bust_matcher::layout::holds_leg(&a.data, 0, group.assets[0].market_id))
        .map(|(k, _)| k)
        .collect())
}

/// Cranks enough to bring the asset's accrual to the chain's slot, at most `cap`; `already` cranks the transaction
/// sends anyway count toward it. None to spare: a crank with no slot left to accrue fails (`EngineNonProgress`) while
/// the price is still moving toward a new mark.
pub fn catch_up_ixs(c: &Ctx, st: &Value, cap: usize, already: usize) -> Result<Vec<Instruction>> {
    let market = pk(st, "market")?;
    let (behind, dt) = accrual_lag(c, &market)?;
    let n = (behind.div_ceil(dt) as usize).saturating_sub(already).min(cap);
    let slot = c.rpc.get_slot()?;
    let house = pk(st, "housePortfolio")?;
    // the slot field is not read on chain; varying it keeps identical cranks in one batch apart
    Ok((0..n as u64).map(|k| crank_ix(c, &c.house.pubkey(), market, house, slot + k)).collect())
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

/// Whether a crank since the last mark moved the price and left the asset's holders stale.
pub fn holders_stale(c: &Ctx, market: &Pubkey) -> Result<bool> {
    let (_, group) = eng(state::read_market(&c.data(market)?))?;
    let a = &group.assets[0];
    Ok(a.stale_account_count_long != 0 || a.stale_account_count_short != 0)
}

/// Cranks that bring the asset to the chain's slot and then every holder, which a price move left stale.
pub fn refresh_ixs(c: &Ctx, st: &Value, cap: usize) -> Result<Vec<Instruction>> {
    let market = pk(st, "market")?;
    let slot = c.rpc.get_slot()?;
    let mut ixs = catch_up_ixs(c, st, cap, 0)?;
    let room = cap.saturating_sub(ixs.len());
    ixs.extend(holders(c, st)?.into_iter().take(room).map(|h| crank_ix(c, &c.house.pubkey(), market, h, slot)));
    Ok(ixs)
}
