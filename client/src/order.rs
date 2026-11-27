//! One order, placed the way the app places it: through the matcher first, where the trader signs alone and the
//! house charges nothing of its own. If the matcher refuses an open because the pool has run past the mark by more
//! than its gap limit, the house quotes a paid entry: it co-signs a `TradeNoCpi` with the gap as the fee, so the trader
//! enters as if at the pool's price (`bust_matcher::rules::paid_entry`). The trader caps the fee it accepts.
//!
//! Only the paid entry needs the house to answer. If it does not, every other order still goes through the matcher.
//! Percolator revokes the matcher on the house portfolio whenever that portfolio trades outside it, so the paid entry
//! enables it again in the same transaction.

use crate::{
    ctx::{eng, pk, read_state, root, Ctx},
    mark::crank_ix,
    market::show,
    matcher::{enable_ix, filled_in, simulate, show_context, trade_ixs, Sim},
};
use anyhow::{bail, Result};
use bust_mark::Feed;
use bust_matcher::{
    context_address, layout,
    rules::{paid_entry, PaidEntry, Pool, Snapshot},
    Context,
};
use percolator_prog::{ix::Instruction as Ix, state};
use serde_json::Value;
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey, signature::Signer};

const POS_SCALE: f64 = 1_000_000.0;
/// The widest gap the house sells as a paid entry when devnet.json names none, in basis points.
const PAID_MAX_GAP_BPS: u64 = 2_000;

/// What the matcher's `Prepare` would read now, read off chain the same way, with the mark it would trade at.
fn snapshot(c: &Ctx, st: &Value, trader_portfolio: &Pubkey) -> Result<(Snapshot, Context, u64)> {
    let market = pk(st, "market")?;
    let ctx = Context::load(&c.data(&context_address(&market, &pk(st, "housePortfolio")?, &c.house.pubkey()).0)?)?;
    let asset = layout::asset(&c.data(&market)?, ctx.asset_index).ok_or_else(|| anyhow::anyhow!("not a market"))?;
    let position_q = layout::position(&c.data(trader_portfolio)?, ctx.asset_index, &asset).ok_or_else(|| anyhow::anyhow!("not a portfolio"))?;
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
        .map(|(base_reserve, spot)| Pool { base_reserve, spot });
    let snap = Snapshot { position_q, oi_long_q: asset.oi_long_q, oi_short_q: asset.oi_short_q, pool, contract_base_atoms: feed.contract_base_atoms };
    Ok((snap, ctx, asset.effective_price))
}

fn paid_max_gap_bps() -> u64 {
    std::fs::read_to_string(root().join("devnet.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|cfg| cfg["matcher"]["paidMaxGapBps"].as_u64())
        .unwrap_or(PAID_MAX_GAP_BPS)
}

/// The house's quote for a paid entry, read after a crank that brings both portfolios and the mark to this slot. The
/// house signs only what this returns: in production this runs on its side, against the chain, not the trader's word.
pub fn house_quote(c: &Ctx, st: &Value, trader_portfolio: &Pubkey, size_q: i128) -> Result<(PaidEntry, u64)> {
    let market = pk(st, "market")?;
    let slot = c.rpc.get_slot()?;
    c.send(vec![crank_ix(c, &c.house.pubkey(), market, *trader_portfolio, slot), crank_ix(c, &c.house.pubkey(), market, pk(st, "housePortfolio")?, slot)], &[&c.house])?;
    let (snap, ctx, mark) = snapshot(c, st, trader_portfolio)?;
    let (cfg, group) = eng(state::read_market(&c.data(&market)?))?;
    let entry = paid_entry(&snap, &ctx.params(), paid_max_gap_bps(), cfg.trade_fee_base_bps, group.config.max_trading_fee_bps, mark, size_q)
        .map_err(|r| anyhow::anyhow!("the house does not sell this entry: {r:?} (pool {:?}, mark {mark})", snap.pool.map(|p| p.spot)))?;
    Ok((entry, mark))
}

/// Places an order of `contracts` (positive: long); a paid entry is taken only at a fee of `max_fee_bps` or less.
pub fn order(c: &Ctx, contracts: f64, max_fee_bps: u64) -> Result<()> {
    let st = read_state()?;
    let trader = c.trader.pubkey();
    let portfolio = pk(&st, "traderPortfolio")?;
    let size_q = (contracts * POS_SCALE).round() as i128;

    // 1) the matcher: whatever it fills, at no fee of the house's
    let ixs = trade_ixs(c, &st, &trader, portfolio, size_q, false)?;
    let mut left = match simulate(c, &ixs, &c.trader)? {
        Sim::Passed { units, logs } => {
            let filled = filled_in(&logs).unwrap_or(size_q);
            let limit = (units * 6 / 5).min(1_400_000) as u32;
            let sig = c.send_with_budget(ixs, &[&c.trader], limit, 1_000)?;
            println!("matcher filled {} of {contracts} contracts, signed by the trader alone: {sig}", filled as f64 / POS_SCALE);
            size_q - filled
        }
        // the matcher's own refusal for the gap (its error codes overlap Percolator's, so its log tells them apart)
        Sim::Failed { logs, .. } if logs.iter().any(|l| l.starts_with("Program log: refused: Gap")) => {
            let why = logs.iter().find(|l| l.contains("refused")).cloned().unwrap_or_default();
            println!("matcher refused: the pool is past its gap limit on this side\n  {why}");
            size_q
        }
        Sim::Failed { error, logs, .. } => {
            let why = logs.iter().find(|l| l.contains("refused")).cloned().unwrap_or(error);
            bail!("the matcher refused the order: {why}");
        }
    };
    if left == 0 {
        return show(c);
    }

    // 2) a paid entry for what is left, if the gap is why
    let (entry, mark) = match house_quote(c, &st, &portfolio, left) {
        Ok(q) => q,
        Err(e) => {
            println!("{e}");
            return show(c);
        }
    };
    let fee_usdc = entry.size_q.unsigned_abs() as f64 / POS_SCALE * mark as f64 / 1e6 * entry.fee_bps as f64 / 1e4;
    println!(
        "house quote: {} contracts at a fee of {} bps ({fee_usdc:.6} USDC), the gap to the pool plus the market's base fee",
        entry.size_q as f64 / POS_SCALE,
        entry.fee_bps
    );
    if entry.fee_bps > max_fee_bps {
        println!("the fee is above the {max_fee_bps} bps you accept: not sent");
        return show(c);
    }

    let market = pk(&st, "market")?;
    let house = pk(&st, "housePortfolio")?;
    let (td, hd) = (c.data(&portfolio)?, c.data(&house)?);
    let (_, group) = eng(state::read_market(&c.data(&market)?))?;
    let slot = c.rpc.get_slot()?;
    let ixs = vec![
        crank_ix(c, &trader, market, portfolio, slot),
        crank_ix(c, &trader, market, house, slot),
        c.prog(
            Ix::TradeNoCpi {
                account_a_portfolio_id: eng(state::read_portfolio_id(&td))?,
                account_a_position_epoch: eng(state::read_portfolio_position_epoch(&td))?,
                account_b_portfolio_id: eng(state::read_portfolio_id(&hd))?,
                account_b_position_epoch: eng(state::read_portfolio_position_epoch(&hd))?,
                asset_index: 0,
                market_id: group.assets[0].market_id,
                size_q: entry.size_q,
                exec_price: mark,
                fee_bps: entry.fee_bps,
                backing_fee_cap_bps: 0,
            },
            vec![
                AccountMeta::new(trader, true),
                AccountMeta::new(c.house.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(portfolio, false),
                AccountMeta::new(house, false),
            ],
        ),
        // the trade moved the house portfolio outside the matcher, which revokes it: enable it again in the same
        // transaction, at the position epoch the trade leaves
        enable_ix(c, &st, eng(state::read_portfolio_position_epoch(&hd))? + 1, eng(state::read_portfolio_matcher_sequence(&hd))?)?,
    ];
    let sig = c.send(ixs, &[&c.trader, &c.house])?;
    left -= entry.size_q;
    println!("paid entry of {} contracts, signed by the trader and the house: {sig}", entry.size_q as f64 / POS_SCALE);
    if left != 0 {
        println!("{} contracts not filled: the open interest cap", left as f64 / POS_SCALE);
    }
    show_context(c)?;
    show(c)
}
