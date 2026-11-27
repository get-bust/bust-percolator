//! Devnet client for Bust markets on Percolator.
//!
//! One market group with one asset: asset 0 is a perp on a Pump coin, priced by an authority-pushed mark (AuthMark)
//! and collateralised in a devnet test USDC. One contract is 1,000,000 coins; marks are collateral atoms per
//! contract. The house is the market authority, the oracle authority and the trader's counterparty.
//!
//!   perc setup <mark_e6>            open the market, set its first mark, open and fund both portfolios
//!   perc show                       the mark, open interest and both portfolios
//!   perc deposit <usdc>             the trader deposits more collateral
//!   perc back <usdc>                the house backs each side of the asset with <usdc>
//!   perc renew                      move each side's backing to a far expiry (only with no claims on it)
//!   perc push <mark_e6>             push one authority mark
//!   perc crank <trader|house|key>   permissionless crank of one portfolio (key: a portfolio in the state file)
//!   perc trade <contracts>          the trader against the house at the effective mark (positive: long)
//!   perc withdraw <usdc>            the trader withdraws collateral
//!   perc convert <usdc>             the trader converts released profit into capital
//!   perc pusher <window_secs>       follow the PumpSwap pool: push its time-weighted average every 4 s
//!   perc record <window_secs> <secs> pusher plus a crank per tick, one JSON line per tick
//!   perc newtrader <tag>            set the current trader aside under <tag> and open a fresh one
//!   perc feed init <window_secs> <interval_secs> <max_step_bps> [pool_file]
//!                                   open the on-chain mark feed (bust-mark) on the pool in state/pool.json (or
//!                                   state/<pool_file>; a SOL pool is priced in dollars with Pyth's SOL/USD)
//!   perc feed close                 close the feed, its rent back to the house
//!   perc feed take | release        the feed takes the oracle role from the house, or gives it back
//!   perc feed update                one feed update
//!   perc feed run <secs> [micro_lamports]
//!                                   the keeper: an update every <secs> seconds, priced at [micro_lamports] a
//!                                   compute unit (default 1,000)
//!   perc feed show                  the feed and the asset's mark target
//!   perc matcher init <oi_cap_bps> <max_gap_bps>
//!                                   open the on-chain matcher's context for the house portfolio and enable it there
//!   perc matcher params <oi_cap_bps> <max_gap_bps>
//!                                   change the house's rules
//!   perc matcher trade <contracts> [update]
//!                                   the trader trades through the matcher, signing alone ([update]: a feed update
//!                                   first, in the same transaction)
//!   perc matcher show               the context and its last snapshot
//!   perc matcher attack             simulate the ways around the snapshot; each must be refused
//!   perc probe <rpc> <pool> <quote_mint>
//!                                   read any cluster's pool with the feed's layouts: reserves and price
//!
//! Configuration: devnet.json (RPC, program id, collateral mint). Keys: HOUSE_KEYPAIR (default
//! ~/.config/solana/id.json), MINT_AUTHORITY_KEYPAIR (the collateral mint's authority, default the house), and
//! keys/ for the market, portfolio and trader keypairs this client creates. Run state: state/perc.json.

mod ctx;
mod feed;
mod market;
mod mark;
mod matcher;
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
    if args.first().map(String::as_str) == Some("probe") {
        return feed::probe(&arg(1)?, &arg(2)?, &arg(3)?);
    }
    let c = Ctx::new()?;
    match args.first().map(String::as_str) {
        Some("setup") => market::setup(&c, arg(1)?.parse()?),
        Some("show") => market::show(&c),
        Some("deposit") => {
            market::deposit(&c, &read_state()?, false, usdc(&arg(1)?)?)?;
            market::show(&c)
        }
        Some("back") => market::back(&c, &read_state()?, usdc(&arg(1)?)?),
        Some("renew") => market::renew(&c, &read_state()?),
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
        Some("feed") => match arg(1)?.as_str() {
            "init" => feed::init(&c, arg(2)?.parse()?, arg(3)?.parse()?, arg(4)?.parse()?, &arg(5).unwrap_or_else(|_| "pool.json".into())),
            "close" => feed::close(&c),
            "take" => feed::take(&c),
            "release" => feed::release(&c),
            "update" => {
                println!("{}", feed::update(&c)?);
                feed::show(&c)
            }
            "run" => feed::run(&c, arg(2)?.parse()?, arg(3).map(|a| a.parse()).unwrap_or(Ok(1_000))?),
            "show" => feed::show(&c),
            _ => bail!("feed: init | take | release | close | update | run | show"),
        },
        Some("matcher") => match arg(1)?.as_str() {
            "init" => matcher::init(&c, arg(2)?.parse()?, arg(3)?.parse()?),
            "params" => matcher::params(&c, arg(2)?.parse()?, arg(3)?.parse()?),
            "trade" => matcher::trade(&c, arg(2)?.parse()?, arg(3).is_ok_and(|a| a == "update")),
            "show" => matcher::show_context(&c),
            "attack" => matcher::attack(&c),
            _ => bail!("matcher: init | params | trade | show | attack"),
        },
        Some("record") => record::record(&c, &read_state()?, arg(1)?.parse()?, arg(2)?.parse()?),
        _ => bail!("unknown command; see the list at the top of client/src/main.rs"),
    }
}
