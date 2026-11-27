//! Trading through the on-chain matcher (bust-matcher): the trader signs alone, and the house's rules are checked on
//! chain against a snapshot `Prepare` takes in the same transaction, right before Percolator's `TradeCpi`.
//!
//! The house sets it up once: it opens the matcher's context for its portfolio and enables that matcher on the
//! portfolio in Percolator (`SetMatcherConfig`). After that, the house does not take part in a trade, not even one
//! that pays the gap: that is a token transfer from the trader to the house's fee account in the same transaction.
//! The keeper samples the pool into the context (`Observe`) with every feed update.

use crate::{
    ctx::{eng, pk, read_state, write_state, Ctx},
    feed,
    mark::crank_ix,
    market::show,
};
use anyhow::{bail, Result};
use bust_matcher::{context_address, delegate_address, Context, TAG_CLOSE, TAG_INIT, TAG_OBSERVE, TAG_PREPARE, TAG_SET_PARAMS};
use bust_mark::{feed_address, Feed};
use percolator_prog::{ix::Instruction as Ix, state};
use serde_json::Value;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_program, sysvar,
    transaction::Transaction,
};

/// Positions are in 1e-6 contract.
const POS_SCALE: f64 = 1_000_000.0;
/// The most the house lets Percolator charge it per trade through the matcher, in basis points.
const TRADE_FEE_CAP_BPS: u16 = 100;

fn matcher_ix(tag: u8, fields: &[&[u8]], accounts: Vec<AccountMeta>) -> Instruction {
    let mut data = vec![tag];
    for f in fields {
        data.extend_from_slice(f);
    }
    Instruction { program_id: bust_matcher::ID, accounts, data }
}

/// The house's settings as `Init` and `SetParams` take them.
fn settings(oi_cap_bps: u64, max_gap_bps: u64, paid_max_gap_bps: u64, window_secs: u64) -> Vec<u8> {
    [oi_cap_bps, max_gap_bps, paid_max_gap_bps, window_secs].iter().flat_map(|v| v.to_le_bytes()).chain(0u16.to_le_bytes()).collect()
}

/// The house's collateral token account, where traders pay the gap.
pub fn fee_account(c: &Ctx) -> Pubkey {
    spl_associated_token_account::get_associated_token_address(&c.house.pubkey(), &c.collateral)
}

pub fn context_key(c: &Ctx, st: &Value) -> Result<Pubkey> {
    Ok(context_address(&pk(st, "market")?, &pk(st, "housePortfolio")?, &c.house.pubkey()).0)
}

/// Opens the matcher's context for the house portfolio and enables the matcher on it in Percolator, with no expiry. A
/// context of an earlier layout is closed and opened again at the same address, which Percolator's grant names.
pub fn init(c: &Ctx, oi_cap_bps: u64, max_gap_bps: u64, paid_max_gap_bps: u64, window_secs: u64) -> Result<()> {
    let mut st = read_state()?;
    let (market, house) = (pk(&st, "market")?, pk(&st, "housePortfolio")?);
    let ctx = context_key(c, &st)?;
    let delegate = delegate_address(&c.program, &market, &house, &c.house.pubkey(), &ctx);
    let feed = feed_address(&market, 0).0;
    let current = c.rpc.get_account(&ctx).ok();
    if current.as_ref().is_some_and(|a| Context::load(&a.data).is_err()) {
        let close = matcher_ix(TAG_CLOSE, &[], vec![AccountMeta::new(c.house.pubkey(), true), AccountMeta::new(ctx, false)]);
        println!("closed a context of an earlier layout: {}", c.send(vec![close], &[&c.house])?);
    }
    if current.is_none() || current.is_some_and(|a| Context::load(&a.data).is_err()) {
        let open = matcher_ix(
            TAG_INIT,
            &[&0u16.to_le_bytes(), &settings(oi_cap_bps, max_gap_bps, paid_max_gap_bps, window_secs)],
            vec![
                AccountMeta::new(c.house.pubkey(), true),
                AccountMeta::new(ctx, false),
                AccountMeta::new_readonly(market, false),
                AccountMeta::new_readonly(house, false),
                AccountMeta::new_readonly(c.program, false),
                AccountMeta::new_readonly(feed, false),
                AccountMeta::new_readonly(fee_account(c), false),
                // the keeper samples the pool with the house's key on devnet; on mainnet a key of its own
                AccountMeta::new_readonly(c.house.pubkey(), false),
                AccountMeta::new_readonly(system_program::ID, false),
            ],
        );
        println!("context {ctx}: {}", c.send(vec![open], &[&c.house])?);
    }

    let hd = c.data(&house)?;
    let enable = enable_ix(c, &st, eng(state::read_portfolio_position_epoch(&hd))?, eng(state::read_portfolio_matcher_sequence(&hd))?)?;
    println!("matcher enabled on the house portfolio: {}", c.send(vec![enable], &[&c.house])?);
    st["matcherCtx"] = ctx.to_string().into();
    st["matcherDelegate"] = delegate.to_string().into();
    write_state(&st)?;
    show_context(c)
}

/// Enables the matcher on the house portfolio, with no expiry, for the portfolio at `position_epoch` and
/// `expected_sequence`. Percolator revokes it whenever the portfolio's position changes outside the matcher (a trade
/// the house signs, a liquidation), so a trade the house signs carries this right after it.
pub fn enable_ix(c: &Ctx, st: &Value, position_epoch: u64, expected_sequence: u64) -> Result<Instruction> {
    let (market, house) = (pk(st, "market")?, pk(st, "housePortfolio")?);
    let ctx = context_key(c, st)?;
    let (_, frontier) = eng(state::read_asset_lifecycle_generation_preflight(&c.data(&market)?, 0, true))?;
    Ok(c.prog(
        Ix::SetMatcherConfig {
            portfolio_id: eng(state::read_portfolio_id(&c.data(&house)?))?,
            expected_sequence,
            position_epoch,
            asset_generation_frontier: frontier,
            enabled: 1,
            trade_fee_cap_bps: TRADE_FEE_CAP_BPS,
            expiry_slot: u64::MAX,
        },
        vec![
            AccountMeta::new(c.house.pubkey(), true),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new(house, false),
            AccountMeta::new_readonly(bust_matcher::ID, false),
            AccountMeta::new_readonly(ctx, false),
            AccountMeta::new_readonly(delegate_address(&c.program, &market, &house, &c.house.pubkey(), &ctx), false),
        ],
    ))
}

/// Changes the house's rules.
pub fn params(c: &Ctx, oi_cap_bps: u64, max_gap_bps: u64, paid_max_gap_bps: u64, window_secs: u64) -> Result<()> {
    let st = read_state()?;
    let ixn = matcher_ix(
        TAG_SET_PARAMS,
        &[&settings(oi_cap_bps, max_gap_bps, paid_max_gap_bps, window_secs)],
        vec![
            AccountMeta::new_readonly(c.house.pubkey(), true),
            AccountMeta::new(context_key(c, &st)?, false),
            AccountMeta::new_readonly(feed_address(&pk(&st, "market")?, 0).0, false),
            AccountMeta::new_readonly(fee_account(c), false),
            AccountMeta::new_readonly(c.house.pubkey(), false),
        ],
    );
    println!("{}", c.send(vec![ixn], &[&c.house])?);
    show_context(c)
}

/// The pool's accounts as the feed names them, and its quote price account when it has one.
fn pool_metas(feed: &Feed) -> Vec<AccountMeta> {
    let mut metas = vec![
        AccountMeta::new_readonly(feed.pool, false),
        AccountMeta::new_readonly(feed.base_vault, false),
        AccountMeta::new_readonly(feed.quote_vault, false),
    ];
    if feed.quote_feed != Pubkey::default() {
        metas.push(AccountMeta::new_readonly(feed.quote_feed, false));
    }
    metas
}

/// The keeper samples the pool into the context, signed by the context's observer (the house's key on devnet).
pub fn observe_ix(c: &Ctx, st: &Value) -> Result<Instruction> {
    let ctx_key = context_key(c, st)?;
    let ctx = Context::load(&c.data(&ctx_key)?)?;
    let feed = Feed::load(&c.data(&ctx.feed)?)?;
    let mut metas = vec![AccountMeta::new_readonly(ctx.observer, true), AccountMeta::new(ctx_key, false), AccountMeta::new_readonly(ctx.feed, false)];
    metas.extend(pool_metas(&feed));
    Ok(matcher_ix(TAG_OBSERVE, &[], metas))
}

/// The instructions of one matcher trade, signed by `trader` alone: an optional feed update, cranks of both
/// portfolios to this slot, the gap's fee as a transfer to the house when `fee` is not zero, `Prepare`, and `TradeCpi`.
pub fn trade_ixs(c: &Ctx, st: &Value, trader: &Pubkey, trader_portfolio: Pubkey, size_q: i128, with_update: bool, fee: u64) -> Result<Vec<Instruction>> {
    let market = pk(st, "market")?;
    let house = pk(st, "housePortfolio")?;
    let ctx_key = context_key(c, st)?;
    let ctx = Context::load(&c.data(&ctx_key)?)?;
    let feed = Feed::load(&c.data(&ctx.feed)?)?;
    // a new mark first, then the cranks that bring both portfolios to it
    let mut ixs = Vec::new();
    if with_update {
        ixs.push(feed::update_ix(c, st)?);
    }
    let slot = c.rpc.get_slot()?;
    ixs.push(crank_ix(c, trader, market, trader_portfolio, slot));
    ixs.push(crank_ix(c, trader, market, house, slot));

    if fee > 0 {
        // the instruction right before Prepare: Prepare reads what it paid
        let from = spl_associated_token_account::get_associated_token_address(trader, &c.collateral);
        ixs.push(spl_token::instruction::transfer(&spl_token::ID, &from, &ctx.fee_account, trader, &[], fee)?);
    }
    let mut prepare = vec![
        AccountMeta::new(ctx_key, false),
        AccountMeta::new_readonly(market, false),
        AccountMeta::new_readonly(trader_portfolio, false),
        AccountMeta::new_readonly(ctx.feed, false),
    ];
    let mut pool = pool_metas(&feed);
    let price = (pool.len() == 4).then(|| pool.remove(3));
    prepare.extend(pool);
    prepare.push(AccountMeta::new_readonly(sysvar::instructions::ID, false));
    prepare.extend(price);
    ixs.push(matcher_ix(TAG_PREPARE, &[], prepare));

    let (td, hd) = (c.data(&trader_portfolio)?, c.data(&house)?);
    let (cfg, group) = eng(state::read_market(&c.data(&market)?))?;
    ixs.push(c.prog(
        Ix::TradeCpi {
            account_a_portfolio_id: eng(state::read_portfolio_id(&td))?,
            account_a_position_epoch: eng(state::read_portfolio_position_epoch(&td))?,
            account_b_portfolio_id: eng(state::read_portfolio_id(&hd))?,
            account_b_position_epoch: eng(state::read_portfolio_position_epoch(&hd))?,
            account_b_matcher_sequence: eng(state::read_portfolio_matcher_sequence(&hd))?,
            asset_index: 0,
            market_id: group.assets[0].market_id,
            size_q,
            fee_bps: cfg.trade_fee_base_bps,
            limit_price: 0,
            backing_fee_cap_bps: 0,
        },
        vec![
            AccountMeta::new(*trader, true),
            AccountMeta::new(market, false),
            AccountMeta::new(trader_portfolio, false),
            AccountMeta::new(house, false),
            AccountMeta::new_readonly(bust_matcher::ID, false),
            AccountMeta::new(ctx_key, false),
            AccountMeta::new_readonly(ctx.delegate, false),
            AccountMeta::new_readonly(sysvar::instructions::ID, false),
        ],
    ));
    Ok(ixs)
}

/// A simulated transaction: the compute units and logs it used, or where and why it failed.
pub enum Sim {
    Passed { units: u64, logs: Vec<String> },
    Failed { error: String, logs: Vec<String> },
}

pub fn simulate(c: &Ctx, ixs: &[Instruction], payer: &Keypair) -> Result<Sim> {
    let mut all = vec![ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)];
    all.extend_from_slice(ixs);
    let tx = Transaction::new_signed_with_payer(&all, Some(&payer.pubkey()), &[payer], c.rpc.get_latest_blockhash()?);
    let sim = c.rpc.simulate_transaction(&tx)?.value;
    let logs = sim.logs.unwrap_or_default();
    Ok(match sim.err {
        None => Sim::Passed { units: sim.units_consumed.unwrap_or(0), logs },
        Some(err) => {
            Sim::Failed { error: format!("{err:?}"), logs }
        }
    })
}

/// The matcher's own log of a fill: "filled <size> of <request> at <mark>".
pub fn filled_in(logs: &[String]) -> Option<i128> {
    logs.iter().find_map(|l| l.strip_prefix("Program log: filled ")?.split(' ').next()?.parse().ok())
}

fn simulate_or_bail(c: &Ctx, ixs: &[Instruction], payer: &Keypair) -> Result<u64> {
    match simulate(c, ixs, payer)? {
        Sim::Passed { units, .. } => Ok(units),
        Sim::Failed { error, logs, .. } => {
            let tail: Vec<&String> = logs.iter().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
            bail!("the trade would fail: {error}\n{}", tail.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n"))
        }
    }
}

/// The trader trades `contracts` against the house through the matcher (positive: long), signing alone.
pub fn trade(c: &Ctx, contracts: f64, with_update: bool) -> Result<()> {
    let st = read_state()?;
    let size_q = (contracts * POS_SCALE).round() as i128;
    let ixs = trade_ixs(c, &st, &c.trader.pubkey(), pk(&st, "traderPortfolio")?, size_q, with_update, 0)?;
    // the limit is what a simulation used and a fifth more: a close that settles costs more than an open
    let units = simulate_or_bail(c, &ixs, &c.trader)?;
    let limit = (units * 6 / 5).min(1_400_000) as u32;
    let sig = c.send_with_budget(ixs, &[&c.trader], limit, 1_000)?;
    println!("matcher trade {contracts} contracts, {units} compute units: {sig}");
    show_context(c)?;
    show(c)
}

/// Tries the ways around the snapshot and the fee, and prints what each would get. Every one but the honest order
/// must fail; nothing is sent. The order opens on the side the gap favours, when there is one, so the fee matters.
pub fn attack(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let trader = c.trader.pubkey();
    let portfolio = pk(&st, "traderPortfolio")?;
    let (snap, ctx, mark) = crate::order::snapshot(c, &st, &portfolio)?;
    let long = snap.pool.is_some_and(|p| p.high > mark && p.high - mark >= mark.saturating_sub(p.low));
    let size_q: i128 = if long { 100_000 } else { -100_000 };
    let (fee, _) = crate::order::gap_fee(&snap, &ctx, mark, size_q);
    println!("an order of {} contract, the gap's fee {:.6} USDC", size_q as f64 / POS_SCALE, fee as f64 / 1e6);
    let honest = trade_ixs(c, &st, &trader, portfolio, size_q, false, fee)?;
    let (prep, trade) = (honest.len() - 2, honest.len() - 1);
    let mut cases: Vec<(&str, Vec<Instruction>)> = Vec::new();

    cases.push(("honest (must pass)", honest.clone()));

    let mut no_prepare = honest.clone();
    no_prepare.remove(prep);
    cases.push(("TradeCpi without Prepare", no_prepare));

    let mut gap = honest.clone();
    gap.insert(trade, honest[0].clone());
    cases.push(("an instruction between Prepare and TradeCpi", gap));

    // a second trade on the one snapshot, with the epochs Percolator will expect after the first
    let mut twice = honest.clone();
    let mut second = honest[trade].clone();
    for o in [9usize, 25] {
        let e = u64::from_le_bytes(second.data[o..o + 8].try_into().unwrap()) + 1;
        second.data[o..o + 8].copy_from_slice(&e.to_le_bytes());
    }
    twice.push(second);
    cases.push(("two trades on one Prepare", twice));

    // Prepare reads another portfolio than the one that trades
    let mut other = honest.clone();
    other[prep].accounts[2].pubkey = pk(&st, "housePortfolio")?;
    cases.push(("Prepare on another portfolio than the trade's", other));

    if fee > 0 {
        // the fee paid to the trader's own account instead of the house's
        let mut elsewhere = honest.clone();
        let own = spl_associated_token_account::get_associated_token_address(&trader, &c.collateral);
        elsewhere[prep - 1].accounts[1].pubkey = own;
        cases.push(("the fee paid to another account", elsewhere));

        // the fee paid, but not right before Prepare
        let mut early = honest.clone();
        let transfer = early.remove(prep - 1);
        early.insert(0, transfer);
        cases.push(("the fee paid earlier in the transaction", early));
    }

    for (name, ixs) in cases {
        match simulate(c, &ixs, &c.trader)? {
            Sim::Passed { units, .. } => println!("{name}: PASSES ({units} compute units)"),
            Sim::Failed { error, logs } => {
                let why = logs.iter().find(|l| l.contains("refused")).map(|l| l.trim_start_matches("Program log: ").split(';').next().unwrap_or("").to_string());
                println!("{name}: refused, {}", why.unwrap_or(error));
            }
        }
    }
    println!("matcher errors: 3 NotPrepared, 4 NotATrade");
    Ok(())
}

pub fn show_context(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let key = context_key(c, &st)?;
    let ctx = Context::load(&c.data(&key)?)?;
    println!(
        "matcher context {key}\n  delegate {}\n  feed {}\n  fee account {}\n  open interest cap {} bps of the pool, free gap {} bps, paid gap up to {} bps, sampled over {} s",
        ctx.delegate, ctx.feed, ctx.fee_account, ctx.oi_cap_bps, ctx.max_gap_bps, ctx.paid_max_gap_bps, ctx.window_secs
    );
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let recent: Vec<String> = ctx
        .buckets
        .iter()
        .filter(|b| b.high != 0 && b.start > now - ctx.window_secs as i64 - 10)
        .map(|b| format!("{}s ago {:.4}-{:.4}", now - b.start, b.low as f64 / 1e6, b.high as f64 / 1e6))
        .collect();
    println!("  observer {}, last sample {} s ago", ctx.observer, now - ctx.last_observed);
    println!("  samples in the window: {}", if recent.is_empty() { "none".into() } else { recent.join(", ") });
    println!(
        "  last snapshot: slot {} trader {} position {} oi long {} short {} paid {}",
        ctx.snap_slot,
        ctx.snap_trader,
        ctx.snap_position_q as f64 / POS_SCALE,
        ctx.snap_oi_long_q as f64 / POS_SCALE,
        ctx.snap_oi_short_q as f64 / POS_SCALE,
        ctx.snap_fee_paid as f64 / 1e6,
    );
    let hd = c.data(&pk(&st, "housePortfolio")?)?;
    let cfg = eng(state::read_portfolio_matcher_config(&hd))?;
    println!("  house portfolio matcher config: program {} context {}", Pubkey::new_from_array(cfg.matcher_program), Pubkey::new_from_array(cfg.matcher_context));
    Ok(())
}
