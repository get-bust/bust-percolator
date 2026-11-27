//! One order, placed the way the app places it: through the matcher, signed by the trader alone. When the pool has
//! run past the mark by more than the free gap on the side the order opens, the matcher fills that open only as far
//! as the gap is paid, with a token transfer from the trader to the house's fee account in the same transaction; the
//! trader enters as if at the pool's price, and caps the fee it pays. The house signs nothing.
//!
//! The fee is worked out here with the matcher's own rules, against the same reading (`rules::fill`), with a little
//! room for the pool moving before the transaction lands: what the fee does not cover is left unfilled, never charged.

use crate::{
    ctx::{pk, read_state, Ctx},
    market::show,
    matcher::{context_key, filled_in, shards, show_context, simulate, trade_ixs, Sim},
};
use anyhow::{anyhow, Result};
use bust_mark::Feed;
use bust_matcher::{
    layout,
    rules::{self, fee_for, gap, oi_cap_q, Pool, Snapshot, POS_SCALE},
    Context,
};
use serde_json::Value;
use solana_sdk::{pubkey::Pubkey, signature::Signer};

/// Rounds over the house portfolios an order makes before giving up, 0.8 s apart.
const RETRY_ROUNDS: u32 = 4;
/// The fee is sized for a price this much worse than the one read, in basis points of the gap.
const FEE_ROOM_BPS: u128 = 200;

/// What the matcher's `Prepare` would read now, read off chain the same way, with the mark it would trade at.
pub fn snapshot(c: &Ctx, st: &Value, trader_portfolio: &Pubkey) -> Result<(Snapshot, Context, u64)> {
    let market = pk(st, "market")?;
    let ctx = Context::load(&c.data(&context_key(c, st)?)?)?;
    let asset = layout::asset(&c.data(&market)?, ctx.asset_index).ok_or_else(|| anyhow!("not a market"))?;
    let position_q = layout::position(&c.data(trader_portfolio)?, ctx.asset_index, &asset).ok_or_else(|| anyhow!("not a portfolio"))?;
    let feed = Feed::load(&c.data(&ctx.feed)?)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    let amount = |k: &Pubkey| -> Result<u64> { Ok(c.rpc.get_token_account_balance(k)?.amount.parse()?) };
    let price = if feed.quote_feed == Pubkey::default() {
        None
    } else {
        bust_mark::read_quote_price(&feed, &c.data(&feed.quote_feed)?, now).ok()
    };
    let pool = bust_mark::read_pool(&feed, &c.data(&feed.pool)?, amount(&feed.base_vault)?, amount(&feed.quote_vault)?, price, now)
        .ok()
        .map(|(base_reserve, spot)| {
            let (high, low) = rules::range(&ctx.buckets, ctx.window_secs, now, spot);
            Pool { base_reserve, spot, high, low }
        });
    let snap = Snapshot {
        position_q,
        oi_long_q: asset.oi_long_q,
        oi_short_q: asset.oi_short_q,
        pool,
        contract_base_atoms: feed.contract_base_atoms,
        fee_paid: 0,
        sampled: now - ctx.last_observed <= ctx.window_secs as i64,
    };
    Ok((snap, ctx, asset.effective_price))
}

/// The fee that lets the open part of `size_q` past the free gap, with room, and the part it pays for; zero when
/// nothing past the free gap is opened.
pub fn gap_fee(snap: &Snapshot, ctx: &Context, mark: u64, size_q: i128) -> (u64, u128) {
    let Some(pool) = snap.pool else { return (0, 0) };
    let long = size_q > 0;
    let (gap_atoms, gap_bps) = gap(&pool, mark, long);
    if gap_bps <= ctx.max_gap_bps || gap_bps > ctx.paid_max_gap_bps {
        return (0, 0);
    }
    let closing = if snap.position_q != 0 && (snap.position_q > 0) != long { size_q.unsigned_abs().min(snap.position_q.unsigned_abs()) } else { 0 };
    let side = if long { snap.oi_long_q } else { snap.oi_short_q };
    let room = oi_cap_q(&pool, mark, snap.contract_base_atoms, ctx.oi_cap_bps).saturating_sub(side);
    let opening = (size_q.unsigned_abs() - closing).min(room);
    let with_room = gap_atoms as u128 * (10_000 + FEE_ROOM_BPS) / 10_000;
    (fee_for(opening, with_room.min(u64::MAX as u128) as u64), opening)
}

/// Places an order of `contracts` (positive: long), paying the gap only up to `max_fee_bps` of what it opens.
pub fn order(c: &Ctx, contracts: f64, max_fee_bps: u64) -> Result<()> {
    let st = read_state()?;
    let trader = c.trader.pubkey();
    let portfolio = pk(&st, "traderPortfolio")?;
    let size_q = (contracts * POS_SCALE as f64).round() as i128;

    let (snap, ctx, mark) = snapshot(c, &st, &portfolio)?;
    let free = rules::fill(&snap, &ctx.params(), mark, size_q);
    let (fee, paid_q) = gap_fee(&snap, &ctx, mark, size_q);
    let mut pay = 0;
    if fee > 0 {
        let notional = paid_q as f64 / POS_SCALE as f64 * mark as f64;
        let fee_bps = (fee as f64 / notional * 1e4).ceil() as u64;
        println!(
            "the pool is {:.2}% past the mark on this side: {} contracts open only with the gap paid, {:.6} USDC ({fee_bps} bps)",
            gap(&snap.pool.unwrap(), mark, size_q > 0).1 as f64 / 100.0,
            paid_q as f64 / POS_SCALE as f64,
            fee as f64 / 1e6
        );
        if fee_bps <= max_fee_bps {
            pay = fee;
        } else {
            println!("that is above the {max_fee_bps} bps you accept: the order goes without it");
        }
    }
    if pay == 0 {
        if let Err(r) = free {
            println!("the matcher would refuse it: {r:?}");
            return show(c);
        }
    }

    // a house portfolio picked at random, and the next one if it has just traded (its position epoch moved) or
    // cannot take the position; a refusal by the rules is the same on every one of them
    let all = shards(&st)?;
    let houses = crate::matcher::shards_for(c, &st, size_q)?;
    let mut last = String::new();
    // for a few seconds after each mark Percolator refuses a trade that adds risk while the keeper settles the
    // market: the order goes round the house portfolios again after a pause, as the app does
    let rounds = (0..RETRY_ROUNDS).flat_map(|r| houses.iter().map(move |h| (r, h)));
    let mut current = 0;
    for (round, house) in rounds {
        // every way out of this loop but a failure breaks it, so a new round means the last one failed throughout
        if round != current {
            current = round;
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        let index = all.iter().position(|h| h == house).unwrap_or(0);
        let ixs = trade_ixs(c, &st, &trader, portfolio, *house, size_q, false, pay)?;
        match simulate(c, &ixs, &c.trader)? {
            Sim::Passed { units, logs } => {
                let filled = filled_in(&logs).unwrap_or(size_q);
                let limit = (units * 6 / 5).min(1_400_000) as u32;
                match c.send_with_budget(ixs, &[&c.trader], limit, 1_000) {
                    Ok(sig) => {
                        println!(
                            "filled {} of {contracts} contracts against house portfolio {index}{}, signed by the trader alone: {sig}",
                            filled as f64 / POS_SCALE as f64,
                            if pay > 0 { format!(", {:.6} USDC paid for the gap", pay as f64 / 1e6) } else { String::new() }
                        );
                        last.clear();
                        break;
                    }
                    Err(e) => last = crate::ctx::brief(&e),
                }
            }
            Sim::Failed { error, logs } => {
                if let Some(why) = logs.iter().find(|l| l.contains("refused")) {
                    println!("not sent: {why}");
                    last.clear();
                    break;
                }
                last = error;
            }
        }
    }
    if !last.is_empty() {
        println!("no house portfolio took it: {last}");
    }
    show_context(c)?;
    show(c)
}
