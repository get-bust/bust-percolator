//! Devnet client for Bust markets on Percolator.
//!
//! One market group with one asset: asset 0 is a perp on a Pump coin, priced by an authority-pushed mark (AuthMark)
//! and collateralised in a devnet test USDC. One contract is 1,000,000 coins; marks are collateral atoms per
//! contract. The house is the market authority, the oracle authority and the trader's counterparty.
//!
//!   perc setup <mark_e6>            open the market, set its first mark, open and fund both portfolios
//!   perc show                       the mark, open interest and both portfolios
//!   perc deposit <usdc>             the trader deposits more collateral
//!   perc push <mark_e6>             push one authority mark
//!   perc crank <trader|house>       permissionless crank of one portfolio
//!   perc trade <contracts>          the trader against the house at the effective mark (positive: long)
//!   perc withdraw <usdc>            the trader withdraws collateral
//!   perc convert <usdc>             the trader converts released profit into capital
//!   perc pusher <window_secs>       follow the PumpSwap pool: push its time-weighted average every 4 s
//!   perc record <window_secs> <secs> pusher plus a crank per tick, one JSON line per tick
//!   perc newtrader <tag>            set the current trader aside under <tag> and open a fresh one
//!
//! Configuration: devnet.json (RPC, program id, collateral mint). Keys: HOUSE_KEYPAIR (default
//! ~/.config/solana/id.json), MINT_AUTHORITY_KEYPAIR (the collateral mint's authority, default the house), and
//! keys/ for the market, portfolio and trader keypairs this client creates. Run state: state/perc.json.

mod ctx;
mod market;
mod mark;
mod trade;
mod record;

use anyhow::{anyhow, bail, Result};
use ctx::{read_state, Ctx};

/// A USDC amount as collateral atoms.
fn usdc(s: &str) -> Result<u128> {
    Ok((s.parse::<f64>()? * 1e6).round() as u128)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).cloned().ok_or_else(|| anyhow!("missing argument {i}"));
    if args.first().map(String::as_str) == Some("newtrader") {
        return record::new_trader(&arg(1)?);
    }
    let c = Ctx::new()?;
    match args.first().map(String::as_str) {
        Some("setup") => market::setup(&c, arg(1)?.parse()?),
        Some("show") => market::show(&c),
        Some("deposit") => {
            market::deposit(&c, &read_state()?, false, usdc(&arg(1)?)?)?;
            market::show(&c)
        }
        Some("push") => {
            println!("{}", mark::push(&c, &read_state()?, arg(1)?.parse()?)?);
            market::show(&c)
        }
        Some("crank") => {
            println!("{}", mark::crank(&c, &read_state()?, &arg(1)?)?);
            market::show(&c)
        }
        Some("trade") => trade::trade(&c, &read_state()?, arg(1)?.parse()?),
        Some("withdraw") => trade::withdraw(&c, &read_state()?, usdc(&arg(1)?)?),
        Some("convert") => trade::convert(&c, &read_state()?, usdc(&arg(1)?)?),
        Some("pusher") => mark::pusher(&c, &read_state()?, arg(1)?.parse()?),
        Some("record") => record::record(&c, &read_state()?, arg(1)?.parse()?, arg(2)?.parse()?),
        _ => bail!("unknown command; see the list at the top of client/src/main.rs"),
    }
}
