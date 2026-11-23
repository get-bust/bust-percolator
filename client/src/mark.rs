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
