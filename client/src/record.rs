//! Recording a run, and starting over with a fresh trader.

use crate::{
    ctx::{eng, load_or_new, pk, read_state, root, write_state, Ctx},
    mark::{crank, pool_spot, push, read_pool, Twap},
    market::{deposit, show},
};
use anyhow::Result;
use percolator_prog::{constants::PORTFOLIO_ACCOUNT_LEN, ix::Instruction as Ix, state};
use serde_json::{json, Value};
use solana_sdk::{instruction::AccountMeta, signature::Signer, system_instruction};
use std::{
    fs, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

/// Runs the pusher and cranks the trader on every tick for `secs` seconds, printing one JSON line per tick: wall-clock
/// ms, the pool's price, the pushed average, the stored target and effective mark, open interest and both portfolios.
/// The pool is read at the start of a tick and the line is stamped at its end, a few seconds later.
pub fn record(c: &Ctx, st: &Value, window: u64, secs: u64) -> Result<()> {
    let pool = read_pool()?;
    let market = pk(st, "market")?;
    let (tp, hp) = (pk(st, "traderPortfolio")?, pk(st, "housePortfolio")?);
    let mut twap = Twap::new(window);
    let mut last = Instant::now() - Duration::from_secs(60);
    let start = Instant::now();
    while start.elapsed().as_secs() < secs {
        let spot = pool_spot(c, &pool)?;
        let avg = twap.add(spot);
        let mut pushed = false;
        if last.elapsed() >= Duration::from_secs(3) {
            pushed = push(c, st, avg.round() as u64).is_ok();
            last = Instant::now();
        }
        let cranked = crank(c, st, "trader").is_ok();
        let (_, g) = eng(state::read_market(&c.data(&market)?))?;
        let t = eng(state::read_portfolio(&c.data(&tp)?))?;
        let h = eng(state::read_portfolio(&c.data(&hp)?))?;
        let a = &g.assets[0];
        println!(
            "{}",
            json!({
                "ms": SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64,
                "slot": g.current_slot,
                "spot": spot / 1e6, "twap": avg / 1e6,
                "target": a.raw_oracle_target_price as f64 / 1e6, "effective": a.effective_price as f64 / 1e6,
                "oiLong": a.oi_eff_long_q as f64 / 1e6, "pushed": pushed, "cranked": cranked,
                "traderCapital": t.capital.get() as f64 / 1e6, "traderPnl": t.pnl.get() as f64 / 1e6,
                "traderBasis": t.legs[0].basis_pos_q.get() as f64 / 1e6,
                "houseCapital": h.capital.get() as f64 / 1e6, "housePnl": h.pnl.get() as f64 / 1e6,
            })
        );
        thread::sleep(Duration::from_millis(300));
    }
    Ok(())
}

/// Sets the current trader aside under `tag` (its keypairs move to keys/*-<tag>.json and its addresses stay in the
/// state file) and opens a fresh trader with 100 USDC deposited. A trader whose profit is junior and unconverted
/// cannot trade again, so each run starts from a clean portfolio.
pub fn new_trader(tag: &str) -> Result<()> {
    let mut st = read_state()?;
    let keys = root().join("keys");
    fs::rename(keys.join("trader.json"), keys.join(format!("trader-{tag}.json")))?;
    fs::rename(keys.join("trader-portfolio.json"), keys.join(format!("trader-portfolio-{tag}.json")))?;
    st[format!("trader_{tag}")] = st["trader"].clone();
    st[format!("traderPortfolio_{tag}")] = st["traderPortfolio"].clone();

    let c = Ctx::new()?;
    let pf = load_or_new("trader-portfolio.json")?;
    let (mint, market) = (pk(&st, "mint")?, pk(&st, "market")?);
    let rent = c.rpc.get_minimum_balance_for_rent_exemption(PORTFOLIO_ACCOUNT_LEN)?;
    let tokens = spl_associated_token_account::get_associated_token_address(&c.trader.pubkey(), &mint);
    let sig = c.send(
        vec![
            system_instruction::transfer(&c.house.pubkey(), &c.trader.pubkey(), 100_000_000),
            spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                &c.house.pubkey(),
                &c.trader.pubkey(),
                &mint,
                &spl_token::ID,
            ),
            spl_token::instruction::mint_to(&spl_token::ID, &mint, &tokens, &c.mint_authority.pubkey(), &[], 1_000 * 1_000_000)?,
            system_instruction::create_account(&c.house.pubkey(), &pf.pubkey(), rent, PORTFOLIO_ACCOUNT_LEN as u64, &c.program),
            c.prog(
                Ix::InitPortfolio,
                vec![AccountMeta::new(c.trader.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf.pubkey(), false)],
            ),
        ],
        &[&c.house, &c.mint_authority, &pf, &c.trader],
    )?;
    st["trader"] = json!(c.trader.pubkey().to_string());
    st["traderPortfolio"] = json!(pf.pubkey().to_string());
    write_state(&st)?;
    println!("trader {} portfolio {}: {sig}", c.trader.pubkey(), pf.pubkey());
    deposit(&c, &st, false, 100_000_000)?;
    show(&c)
}
