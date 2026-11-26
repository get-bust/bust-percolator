//! The on-chain mark: a bust-mark feed that reads the PumpSwap pool and pushes its average as the asset's mark.
//!
//! The feed program reads the pool itself; this client only decides when to call it and passes the market's
//! control sequences, which Percolator checks. The feed's oracle PDA must first take the oracle role from the house.

use crate::ctx::{eng, pk, read_state, write_state, Ctx};
use crate::mark::read_pool;
use anyhow::{anyhow, Result};
use bust_mark::{feed_address, observations, oracle_address, Feed};
use percolator_prog::state;
use serde_json::Value;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Signer,
    system_program,
};
use std::{thread, time::Duration};

/// One contract is 1,000,000 coins of 6 decimals.
const CONTRACT_BASE_ATOMS: u64 = 1_000_000_000_000;

fn ix(tag: u8, fields: &[&[u8]], accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = vec![tag];
    for f in fields {
        data.extend_from_slice(f);
    }
    Instruction { program_id: bust_mark::ID, accounts, data }
}

/// The market id of asset 0, the next oracle observation sequence and the authority epoch.
fn sequences(c: &Ctx, market: &Pubkey) -> Result<(u64, u64, u64)> {
    let data = c.data(market)?;
    let (_, group) = eng(state::read_market(&data))?;
    let seq = eng(state::read_asset_control_sequences(&data, 0))?;
    Ok((group.assets[0].market_id, seq.oracle_observation + 1, seq.authority_epoch))
}

fn feed_key(st: &Value) -> Result<Pubkey> {
    Ok(feed_address(&pk(st, "market")?, 0).0)
}

/// Opens the feed for asset 0 on the pool in state/pool.json.
pub fn init(c: &Ctx, window: u64, interval: u64, max_step_bps: u64) -> Result<()> {
    let mut st = read_state()?;
    let pool = read_pool()?;
    let market = pk(&st, "market")?;
    let (feed, _) = feed_address(&market, 0);
    let accounts = vec![
        AccountMeta::new(c.house.pubkey(), true),
        AccountMeta::new(feed, false),
        AccountMeta::new_readonly(pk(&pool, "pool")?, false),
        AccountMeta::new_readonly(market, false),
        AccountMeta::new_readonly(c.program, false),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new_readonly(pk(&pool, "baseVault")?, false),
        AccountMeta::new_readonly(pk(&pool, "quoteVault")?, false),
    ];
    let fields: [&[u8]; 5] = [&0u16.to_le_bytes(), &CONTRACT_BASE_ATOMS.to_le_bytes(), &window.to_le_bytes(), &interval.to_le_bytes(), &max_step_bps.to_le_bytes()];
    let sig = c.send(vec![ix(0, &fields, accounts)], &[&c.house])?;
    st["feed"] = feed.to_string().into();
    st["feedOracle"] = oracle_address(&feed).0.to_string().into();
    write_state(&st)?;
    println!("feed {feed} {sig}");
    show(c)
}

/// The feed's oracle PDA takes the oracle role of asset 0 from the house.
pub fn take(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let market = pk(&st, "market")?;
    let feed = feed_key(&st)?;
    let (market_id, _, epoch) = sequences(c, &market)?;
    let accounts = vec![
        AccountMeta::new_readonly(c.house.pubkey(), true),
        AccountMeta::new_readonly(feed, false),
        AccountMeta::new_readonly(oracle_address(&feed).0, false),
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(c.program, false),
    ];
    let sig = c.send(vec![ix(2, &[&market_id.to_le_bytes(), &epoch.to_le_bytes()], accounts)], &[&c.house])?;
    println!("oracle role to {} {sig}", oracle_address(&feed).0);
    Ok(())
}

/// The house takes the oracle role back from the feed, so `perc push` and the off-chain pusher work again.
pub fn release(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let market = pk(&st, "market")?;
    let feed = feed_key(&st)?;
    let (market_id, _, epoch) = sequences(c, &market)?;
    let house = c.house.pubkey();
    let accounts = vec![
        AccountMeta::new_readonly(house, true),
        AccountMeta::new_readonly(feed, false),
        AccountMeta::new_readonly(oracle_address(&feed).0, false),
        AccountMeta::new_readonly(house, true),
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(c.program, false),
    ];
    let sig = c.send(vec![ix(3, &[&market_id.to_le_bytes(), &epoch.to_le_bytes(), house.as_ref()], accounts)], &[&c.house])?;
    println!("oracle role back to the house {sig}");
    Ok(())
}

pub fn update_ix(c: &Ctx, st: &Value) -> Result<Instruction> {
    let pool = read_pool()?;
    let market = pk(st, "market")?;
    let feed = feed_key(st)?;
    let (market_id, seq, epoch) = sequences(c, &market)?;
    let accounts = vec![
        AccountMeta::new(feed, false),
        AccountMeta::new_readonly(pk(&pool, "baseVault")?, false),
        AccountMeta::new_readonly(pk(&pool, "quoteVault")?, false),
        AccountMeta::new(market, false),
        AccountMeta::new_readonly(oracle_address(&feed).0, false),
        AccountMeta::new_readonly(c.program, false),
    ];
    Ok(ix(1, &[&market_id.to_le_bytes(), &seq.to_le_bytes(), &epoch.to_le_bytes()], accounts))
}

/// One update: the feed reads the pool, adds an observation and pushes the average.
pub fn update(c: &Ctx) -> Result<String> {
    let st = read_state()?;
    c.send(vec![update_ix(c, &st)?], &[&c.house])
}

/// Calls the feed every `secs` seconds. A call the program refuses as too soon is skipped.
pub fn run(c: &Ctx, secs: u64) -> Result<()> {
    loop {
        match update(c) {
            Ok(sig) => {
                let (feed, target) = read(c)?;
                println!("pool {:.4} mark {:.4} {}", feed.last_price as f64 / 1e6, target as f64 / 1e6, &sig[..12]);
            }
            Err(e) => println!("update failed: {}", e.to_string().lines().find(|l| l.contains("Custom") || l.contains("Error")).unwrap_or("?")),
        }
        thread::sleep(Duration::from_secs(secs));
    }
}

/// The feed and the asset's current authority mark target.
fn read(c: &Ctx) -> Result<(Feed, u64)> {
    let st = read_state()?;
    let data = c.data(&feed_key(&st)?)?;
    let feed = Feed::load(&data).map_err(|e| anyhow!("{e:?}"))?;
    let market = c.data(&pk(&st, "market")?)?;
    let (_, group) = eng(state::read_market(&market))?;
    Ok((feed, group.assets[0].raw_oracle_target_price))
}

pub fn show(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let key = feed_key(&st)?;
    let data = c.data(&key)?;
    let feed = Feed::load(&data).map_err(|e| anyhow!("{e:?}"))?;
    let obs = observations(&data, &feed);
    let newest = obs.last().copied();
    println!("feed {key}");
    println!("  pool {}  window {} s  interval {} s  max step {} bps", feed.pool, feed.window_secs, feed.min_interval_secs, feed.max_step_bps);
    println!("  held price {:.4}  observations {}  newest at {:?}", feed.last_price as f64 / 1e6, obs.len(), newest.map(|o| o.ts));
    let (_, target) = read(c)?;
    println!("  asset 0 mark target {:.4}", target as f64 / 1e6);
    Ok(())
}
