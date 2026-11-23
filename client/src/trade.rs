//! Trading the trader against the house, and moving the trader's money out.

use crate::{
    ctx::{eng, pk, root, Ctx},
    market::show,
    mark::{crank_ix, pool_spot, read_pool},
};
use anyhow::{bail, Result};
use percolator_prog::{ix::Instruction as Ix, state};
use serde_json::Value;
use solana_sdk::{instruction::AccountMeta, signature::Signer};

/// Position sizes are in 1e-6 contract.
const POS_SCALE: f64 = 1_000_000.0;

/// The house quotes every trade (it co-signs as the counterparty), so its rules decide what can be opened:
/// - open interest on a side may not exceed `oi_cap_bps` of the pool's collateral depth after a trade that adds risk;
/// - when the pool and the mark disagree, the side that would profit as the mark catches up pays the gap as a fee, and
///   past `max_gap_bps` the house does not open that side at all.
/// Trades that reduce risk are always quoted, at no fee. The fee is charged to both accounts and lands in insurance,
/// which the house funds, so in effect the trader pays it.
struct Quote {
    fee_bps: u64,
    note: String,
}

fn quote(c: &Ctx, oi_long_q: u128, oi_short_q: u128, mark: u64, position_q: i128, size_q: i128) -> Result<Quote> {
    let next_q = position_q + size_q;
    if next_q.unsigned_abs() <= position_q.unsigned_abs() {
        return Ok(Quote { fee_bps: 0, note: "reduces risk".into() });
    }
    let cfg: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(root().join("devnet.json"))?)?;
    let oi_cap_bps = cfg["matcher"]["oiCapBps"].as_u64().unwrap_or(500);
    let max_gap_bps = cfg["matcher"]["maxGapBps"].as_u64().unwrap_or(100);
    let pool = read_pool()?;
    let spot = pool_spot(c, &pool)?;
    let depth = c.rpc.get_token_account_balance(&pk(&pool, "quoteVault")?)?.amount.parse::<f64>()?;

    // the side this trade grows, after the trade (the house is every trader's counterparty)
    let added = (next_q.unsigned_abs() - position_q.unsigned_abs()) as f64;
    let side_q = if next_q > 0 { oi_long_q } else { oi_short_q } as f64 + added;
    let notional = side_q * mark as f64 / POS_SCALE;
    let cap = depth * oi_cap_bps as f64 / 10_000.0;
    if notional > cap {
        bail!(
            "the house does not quote: open interest {:.2} would exceed {:.2}, {} bps of the pool's {:.2}",
            notional / 1e6, cap / 1e6, oi_cap_bps, depth / 1e6
        );
    }
    let gap = (spot - mark as f64) / mark as f64;
    let gains = (next_q > 0 && gap > 0.0) || (next_q < 0 && gap < 0.0);
    let gap_bps = (gap.abs() * 10_000.0).round() as u64;
    if gains && gap_bps > max_gap_bps {
        bail!(
            "the house does not quote: the pool ({:.4}) is {} bps from the mark ({:.4}), past {} bps, on the side that gains",
            spot / 1e6, gap_bps, mark as f64 / 1e6, max_gap_bps
        );
    }
    let fee_bps = if gains { gap_bps } else { 0 };
    Ok(Quote {
        fee_bps,
        note: format!("pool {:.4} mark {:.4}, open interest {:.2} of {:.2}", spot / 1e6, mark as f64 / 1e6, notional / 1e6, cap / 1e6),
    })
}

/// The trader trades `contracts` against the house (positive: the trader goes long) at the effective mark, both
/// signing (TradeNoCpi). Both portfolios are cranked to this slot in the same transaction: a trade that increases
/// risk is refused while the asset's losses are not accrued to the current slot.
///
/// The engine books the position at its effective mark; the execution price only sizes the trade's notional (fees
/// and limits). The stored mark can lag the pushed one until a crank moves it, so a crank of its own runs first and
/// the price is read after it.
///
/// A trade that closes the trader's position converts its profit in the same transaction. Conversion needs the
/// account's health certificate current, and a crank does not refresh the certificate of an account with no position:
/// once a new mark is pushed after the close, the profit stays junior until the account trades again. The profit is
/// settled by the trade itself, so the conversion is sent with every close; with nothing to convert it fails, and the
/// close is sent alone.
pub fn trade(c: &Ctx, st: &Value, contracts: f64) -> Result<()> {
    let market = pk(st, "market")?;
    let (tp, hp) = (pk(st, "traderPortfolio")?, pk(st, "housePortfolio")?);
    let slot = c.rpc.get_slot()?;
    c.send(vec![crank_ix(c, &c.house.pubkey(), market, tp, slot), crank_ix(c, &c.house.pubkey(), market, hp, slot)], &[&c.house])?;
    let (_, group) = eng(state::read_market(&c.data(&market)?))?;
    let asset = &group.assets[0];
    let (td, hd) = (c.data(&tp)?, c.data(&hp)?);
    let size_q = (contracts * POS_SCALE).round() as i128;
    let price = asset.effective_price;
    let position_q = eng(state::read_portfolio(&td))?.legs[0].basis_pos_q.get();
    let q = quote(c, asset.oi_eff_long_q, asset.oi_eff_short_q, price, position_q, size_q)?;
    println!("house quote: fee {} bps ({})", q.fee_bps, q.note);
    let slot = c.rpc.get_slot()?;
    let trade_ixs = vec![
        crank_ix(c, &c.house.pubkey(), market, tp, slot),
        crank_ix(c, &c.house.pubkey(), market, hp, slot),
        c.prog(
            Ix::TradeNoCpi {
                account_a_portfolio_id: eng(state::read_portfolio_id(&td))?,
                account_a_position_epoch: eng(state::read_portfolio_position_epoch(&td))?,
                account_b_portfolio_id: eng(state::read_portfolio_id(&hd))?,
                account_b_position_epoch: eng(state::read_portfolio_position_epoch(&hd))?,
                asset_index: 0,
                market_id: asset.market_id,
                size_q,
                exec_price: price,
                fee_bps: q.fee_bps,
                backing_fee_cap_bps: 0,
            },
            vec![
                AccountMeta::new(c.trader.pubkey(), true),
                AccountMeta::new(c.house.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(tp, false),
                AccountMeta::new(hp, false),
            ],
        ),
    ];
    let signers = [&c.house, &c.trader];
    let sig = if position_q != 0 && position_q + size_q == 0 {
        // the trade advances both portfolios' position epochs
        let convert = c.prog(
            Ix::ConvertReleasedPnl {
                portfolio_id: eng(state::read_portfolio_id(&td))?,
                position_epoch: eng(state::read_portfolio_position_epoch(&td))? + 1,
                amount: u64::MAX as u128,
            },
            vec![AccountMeta::new(c.trader.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(tp, false)],
        );
        match c.send([trade_ixs.clone(), vec![convert]].concat(), &signers) {
            Ok(sig) => {
                println!("the close converts its profit in the same transaction");
                sig
            }
            Err(e) => {
                println!("no profit converts with the close ({}); closing alone", e.to_string().lines().next().unwrap_or(""));
                c.send(trade_ixs, &signers)?
            }
        }
    } else {
        c.send(trade_ixs, &signers)?
    };
    let side = if size_q > 0 { "trader long" } else { "trader short" };
    println!("trade {contracts} contracts at {} ({side}): {sig}", price as f64 / 1e6);
    show(c)
}

/// The trader withdraws `amount` collateral atoms to its token account.
pub fn withdraw(c: &Ctx, st: &Value, amount: u128) -> Result<()> {
    let pf = pk(st, "traderPortfolio")?;
    let data = c.data(&pf)?;
    let destination = spl_associated_token_account::get_associated_token_address(&c.trader.pubkey(), &pk(st, "mint")?);
    let sig = c.send(
        vec![c.prog(
            Ix::Withdraw {
                portfolio_id: eng(state::read_portfolio_id(&data))?,
                expected_sequence: eng(state::read_portfolio_matcher_sequence(&data))?,
                amount,
            },
            vec![
                AccountMeta::new(c.trader.pubkey(), true),
                AccountMeta::new(pk(st, "market")?, false),
                AccountMeta::new(pf, false),
                AccountMeta::new(destination, false),
                AccountMeta::new(pk(st, "vault")?, false),
                AccountMeta::new_readonly(pk(st, "vaultAuthority")?, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
        )],
        &[&c.trader],
    )?;
    println!("withdraw {}: {sig}", amount as f64 / 1e6);
    show(c)
}

/// Converts the trader's released profit into withdrawable capital, after a crank in the same transaction. Profit
/// is junior: in a live market it converts only against the asset's backing (source claims), not the counterparty.
pub fn convert(c: &Ctx, st: &Value, amount: u128) -> Result<()> {
    let market = pk(st, "market")?;
    let pf = pk(st, "traderPortfolio")?;
    let data = c.data(&pf)?;
    let sig = c.send(
        vec![
            crank_ix(c, &c.trader.pubkey(), market, pf, c.rpc.get_slot()?),
            c.prog(
                Ix::ConvertReleasedPnl {
                    portfolio_id: eng(state::read_portfolio_id(&data))?,
                    position_epoch: eng(state::read_portfolio_position_epoch(&data))?,
                    amount,
                },
                vec![AccountMeta::new(c.trader.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf, false)],
            ),
        ],
        &[&c.trader],
    )?;
    println!("convert {}: {sig}", amount as f64 / 1e6);
    show(c)
}
