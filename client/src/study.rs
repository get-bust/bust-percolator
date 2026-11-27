//! How often a trade that adds risk goes through, under a running keeper: the trader opens a small position through
//! the matcher every few seconds and closes it, retrying an open that Percolator refuses for a moment (the asset's
//! accrual behind, holders stale after the mark moved, the price still moving toward a new mark). Between orders it
//! samples whether the market is ready for an open at all. Run it beside `perc feed run`, once a market's parameters
//! are set, in a calm pool and while the pool is pumped.

use crate::{
    ctx::{eng, pk, read_state, Ctx},
    matcher::{simulate, trade_ixs, Sim},
    order::{gap_fee, snapshot},
};
use anyhow::Result;
use percolator_prog::state;
use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Write,
    time::{Duration, Instant},
};

/// Each order, in 1e-6 contract.
const SIZE_Q: i128 = 50_000;
const RETRY_PAUSE: Duration = Duration::from_millis(800);

/// What the market looks like to an open right now.
struct Readiness {
    lag_atoms: u64,
    stale: u64,
    behind: u64,
    #[allow(dead_code)]
    dt: u64,
}

fn readiness(c: &Ctx, market: &solana_sdk::pubkey::Pubkey) -> Result<Readiness> {
    let (_, group) = eng(state::read_market(&c.data(market)?))?;
    let a = &group.assets[0];
    Ok(Readiness {
        lag_atoms: a.raw_oracle_target_price.abs_diff(a.effective_price),
        stale: a.stale_account_count_long + a.stale_account_count_short,
        behind: group.current_slot.saturating_sub(a.slot_last),
        dt: group.config.max_accrual_dt_slots,
    })
}

/// The Percolator or matcher error code of a failed simulation, from its logs.
fn failure(error: &str, logs: &[String]) -> String {
    let error = &error.split_whitespace().collect::<String>();
    if let Some(r) = logs.iter().find(|l| l.contains("refused:")) {
        return r.trim_start_matches("Program log: ").split(';').next().unwrap_or("").to_string();
    }
    match error.split("Custom(").nth(1).and_then(|t| t.split(')').next()) {
        Some("19") => "EngineStale".into(),
        Some("21") => "EngineLockActive".into(),
        Some("22") => "EngineNonProgress".into(),
        Some(code) => format!("custom {code}"),
        None => error.chars().take(60).collect(),
    }
}

/// One order through the matcher, retried up to `retries` times on a refusal of the moment. Returns the attempts it
/// took, how it ended, and why the first attempt failed if it did.
fn place(c: &Ctx, st: &serde_json::Value, size_q: i128, retries: u32) -> Result<(u32, String, String)> {
    let trader = solana_sdk::signature::Signer::pubkey(&c.trader);
    let portfolio = pk(st, "traderPortfolio")?;
    let houses = crate::matcher::shards_for(c, st, size_q)?;
    let mut last = String::new();
    let mut first = String::new();
    for attempt in 1..=retries + 1 {
        let (snap, ctx, mark) = snapshot(c, st, &portfolio)?;
        let (fee, _) = gap_fee(&snap, &ctx, mark, size_q);
        let house = houses[(attempt as usize - 1) % houses.len()];
        let ixs = trade_ixs(c, st, &trader, portfolio, house, size_q, false, fee)?;
        match simulate(c, &ixs, &c.trader)? {
            Sim::Passed { units, .. } => match c.send_with_budget(ixs, &[&c.trader], (units * 6 / 5).min(1_400_000) as u32, 1_000) {
                Ok(_) => return Ok((attempt, "ok".into(), first)),
                Err(e) => last = failure(&e.to_string(), &[]),
            },
            Sim::Failed { error, logs } => {
                last = failure(&error, &logs);
                // a refusal by the house's rules is not a moment's refusal: retrying changes nothing
                if last.starts_with("refused") {
                    return Ok((attempt, last, first));
                }
            }
        }
        if first.is_empty() {
            first = last.clone();
        }
        std::thread::sleep(RETRY_PAUSE);
    }
    Ok((retries + 1, last, first))
}

/// Opens and closes every `every` seconds for `minutes`, writing one JSON line an order to state/study-<tag>.jsonl,
/// and prints a summary.
pub fn study(c: &Ctx, tag: &str, minutes: u64, every: u64, retries: u32) -> Result<()> {
    let st = read_state()?;
    let market = pk(&st, "market")?;
    let mut log = OpenOptions::new().create(true).append(true).open(crate::ctx::root().join(format!("state/study-{tag}.jsonl")))?;
    let started = Instant::now();
    let (mut opens, mut first_try, mut within, mut closes, mut closed) = (0u32, 0u32, 0u32, 0u32, 0u32);
    let (mut samples, mut ready) = (0u32, 0u32);
    let mut reasons: std::collections::BTreeMap<String, u32> = Default::default();
    let mut firsts: std::collections::BTreeMap<String, u32> = Default::default();
    let mut side = 1i128;
    while started.elapsed() < Duration::from_secs(minutes * 60) {
        let r = readiness(c, &market)?;
        samples += 1;
        if r.lag_atoms == 0 && r.stale == 0 && r.behind == 0 {
            ready += 1;
        }
        // with a standing position, every order adds to it (an open with open interest, the case Percolator guards);
        // without one, the sides alternate
        let market_data = c.data(&market)?;
        let asset = bust_matcher::layout::asset(&market_data, 0).ok_or_else(|| anyhow::anyhow!("not a market"))?;
        let held = bust_matcher::layout::position(&c.data(&pk(&st, "traderPortfolio")?)?, 0, &asset).unwrap_or(0);
        side = if held != 0 { held.signum() } else { -side };
        let (attempts, outcome, first) = place(c, &st, side * SIZE_Q, retries)?;
        opens += 1;
        if !first.is_empty() {
            *firsts.entry(first.clone()).or_default() += 1;
        }
        if outcome == "ok" {
            within += 1;
            if attempts == 1 {
                first_try += 1;
            }
        }
        if attempts > 1 || outcome != "ok" {
            *reasons.entry(outcome.clone()).or_default() += 1;
        }
        writeln!(log, "{}", json!({ "t": started.elapsed().as_secs(), "kind": "open", "attempts": attempts, "outcome": outcome, "first": first, "lag": r.lag_atoms, "stale": r.stale, "behind": r.behind }))?;
        if outcome == "ok" {
            let (attempts, outcome, first) = place(c, &st, -side * SIZE_Q, retries)?;
            if !first.is_empty() {
                *firsts.entry(format!("close: {first}")).or_default() += 1;
            }
            closes += 1;
            if outcome == "ok" {
                closed += 1;
            }
            writeln!(log, "{}", json!({ "t": started.elapsed().as_secs(), "kind": "close", "attempts": attempts, "outcome": outcome }))?;
        }
        std::thread::sleep(Duration::from_secs(every));
    }
    let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    println!(
        "{tag}: {opens} opens, {:.0}% at the first try, {:.0}% within {retries} retries; {closes} closes, {:.0}% through; ready for an open in {:.0}% of {samples} samples",
        pct(first_try, opens),
        pct(within, opens),
        pct(closed, closes),
        pct(ready, samples)
    );
    for (why, n) in reasons {
        println!("  {n} x ended {why}");
    }
    for (why, n) in firsts {
        println!("  {n} x first attempt failed: {why}");
    }
    Ok(())
}
