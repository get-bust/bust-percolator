//! Trading the trader against the house, and moving the trader's money out.

use crate::{
    ctx::{eng, pk, Ctx},
    market::show,
    mark::crank_ix,
};
use anyhow::Result;
use percolator_prog::{ix::Instruction as Ix, state};
use serde_json::Value;
use solana_sdk::{instruction::AccountMeta, signature::Signer};

/// Position sizes are in 1e-6 contract.
const POS_SCALE: f64 = 1_000_000.0;

/// The trader trades `contracts` against the house (positive: the trader goes long) at the effective mark, both
/// signing (TradeNoCpi). Both portfolios are cranked to this slot in the same transaction: a trade that increases
/// risk is refused while the asset's losses are not accrued to the current slot.
pub fn trade(c: &Ctx, st: &Value, contracts: f64) -> Result<()> {
    let market = pk(st, "market")?;
    let (tp, hp) = (pk(st, "traderPortfolio")?, pk(st, "housePortfolio")?);
    let (_, group) = eng(state::read_market(&c.data(&market)?))?;
    let asset = &group.assets[0];
    let (td, hd) = (c.data(&tp)?, c.data(&hp)?);
    let size_q = (contracts * POS_SCALE).round() as i128;
    let price = asset.effective_price;
    let slot = c.rpc.get_slot()?;
    let sig = c.send(
        vec![
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
                    fee_bps: 0,
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
        ],
        &[&c.house, &c.trader],
    )?;
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
