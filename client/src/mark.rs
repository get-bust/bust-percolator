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

/// Cranks the trader's or the house's portfolio; anyone may, the house pays here.
pub fn crank(c: &Ctx, st: &Value, who: &str) -> Result<String> {
    let portfolio = if who == "house" { pk(st, "housePortfolio")? } else { pk(st, "traderPortfolio")? };
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

/// Time-weighted average of the samples within `window` of the newest.
pub struct Twap {
    window: Duration,
    samples: VecDeque<(Instant, f64)>,
}

impl Twap {
    pub fn new(window_secs: u64) -> Self {
        Self { window: Duration::from_secs(window_secs), samples: Default::default() }
    }

    pub fn add(&mut self, price: f64) -> f64 {
        let now = Instant::now();
        self.samples.push_back((now, price));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > self.window) {
            self.samples.pop_front();
        }
        self.samples.iter().map(|(_, p)| p).sum::<f64>() / self.samples.len() as f64
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }
}

/// Follows the pool: samples it every 2 s and pushes its time-weighted average over `window` seconds every 4 s.
pub fn pusher(c: &Ctx, st: &Value, window: u64) -> Result<()> {
    let pool = read_pool()?;
    let mut twap = Twap::new(window);
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
