//! A race: several traders send an order at the same moment, spread over some of the house portfolios, and then
//! close. Every trade advances its house portfolio's position epoch and names it exactly, so of the orders sent at
//! once against one house portfolio only the first goes through; spread over as many house portfolios as orders, all
//! of them should.

use crate::{
    ctx::{load_or_new, pk, read_state, Ctx},
    market::deposit_into,
    matcher::{shards, trade_ixs},
    order::{gap_fee, snapshot},
};
use anyhow::{bail, Result};
use bust_matcher::layout;
use percolator_prog::{constants::PORTFOLIO_ACCOUNT_LEN, ix::Instruction as Ix};
use serde_json::Value;
use solana_client::rpc_config::RpcSendTransactionConfig;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::AccountMeta,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::time::{Duration, Instant};

/// Each racer's order, in 1e-6 contract.
const SIZE_Q: i128 = 50_000;

/// The racers: `n` traders of their own (keys/racer-<k>.json), each opened with a portfolio and 50 USDC in it on first
/// use.
fn racers(c: &Ctx, st: &Value, n: usize) -> Result<Vec<(Keypair, Pubkey)>> {
    let (market, mint) = (pk(st, "market")?, pk(st, "mint")?);
    let mut out = Vec::new();
    for k in 0..n {
        let racer = load_or_new(&format!("racer-{k}.json"))?;
        let pf = crate::ctx::market_key(&format!("racer-portfolio-{k}.json"))?;
        if c.rpc.get_account(&pf.pubkey()).is_err() {
            let tokens = spl_associated_token_account::get_associated_token_address(&racer.pubkey(), &mint);
            let rent = c.rpc.get_minimum_balance_for_rent_exemption(PORTFOLIO_ACCOUNT_LEN)?;
            c.send(
                vec![
                    system_instruction::transfer(&c.house.pubkey(), &racer.pubkey(), 20_000_000),
                    spl_associated_token_account::instruction::create_associated_token_account_idempotent(&c.house.pubkey(), &racer.pubkey(), &mint, &spl_token::ID),
                    spl_token::instruction::mint_to(&spl_token::ID, &mint, &tokens, &c.mint_authority.pubkey(), &[], 100_000_000)?,
                    system_instruction::create_account(&c.house.pubkey(), &pf.pubkey(), rent, PORTFOLIO_ACCOUNT_LEN as u64, &c.program),
                    c.prog(Ix::InitPortfolio, vec![AccountMeta::new(racer.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf.pubkey(), false)]),
                ],
                &[&c.house, &c.mint_authority, &pf, &racer],
            )?;
            deposit_into(c, st, &racer, &pf.pubkey(), 50_000_000)?;
            println!("racer {k}: {} portfolio {}", racer.pubkey(), pf.pubkey());
        }
        out.push((racer, pf.pubkey()));
    }
    Ok(out)
}

/// Sends every order at once and waits for each to land; returns how many went through and the errors of the rest.
fn send_all(c: &Ctx, txs: Vec<Transaction>) -> Result<(usize, Vec<String>)> {
    let config = RpcSendTransactionConfig { skip_preflight: true, ..Default::default() };
    let sigs: Vec<Signature> = txs.iter().map(|tx| c.rpc.send_transaction_with_config(tx, config)).collect::<Result<_, _>>()?;
    let started = Instant::now();
    loop {
        let statuses = c.rpc.get_signature_statuses(&sigs)?.value;
        if statuses.iter().all(|s| s.as_ref().is_some_and(|s| s.satisfies_commitment(c.rpc.commitment()))) {
            let errors: Vec<String> = statuses.iter().filter_map(|s| s.as_ref()?.err.as_ref().map(|e| format!("{e:?}"))).collect();
            return Ok((sigs.len() - errors.len(), errors));
        }
        if started.elapsed() > Duration::from_secs(90) {
            bail!("not all orders landed in 90 s");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// `traders` racers open at once over the first `used` house portfolios, then close at once.
pub fn race(c: &Ctx, traders: usize, used: usize) -> Result<()> {
    let st = read_state()?;
    let houses = shards(&st)?;
    if used == 0 || used > houses.len() {
        bail!("there are {} house portfolios", houses.len());
    }
    let racers = racers(c, &st, traders)?;
    // the asset to this slot and its holders fresh first, as a keeper leaves them, then the side with the smaller
    // gap, paying it if there is one
    crate::mark::settle(c, &st, 1_000)?;
    let (snap, ctx, mark) = snapshot(c, &st, &racers[0].1)?;
    let long = snap.pool.is_some_and(|p| p.high.saturating_sub(mark) <= mark.saturating_sub(p.low));
    let size_q = if long { SIZE_Q } else { -SIZE_Q };
    let (fee, _) = gap_fee(&snap, &ctx, mark, size_q);

    for round in ["open", "close"] {
        let blockhash = c.rpc.get_latest_blockhash()?;
        let mut txs = Vec::new();
        for (k, (racer, pf)) in racers.iter().enumerate() {
            let size = if round == "open" {
                size_q
            } else {
                // close what each one holds: an order that did not go through has nothing to close
                let market = c.data(&pk(&st, "market")?)?;
                let asset = layout::asset(&market, 0).ok_or_else(|| anyhow::anyhow!("not a market"))?;
                -layout::position(&c.data(pf)?, 0, &asset).unwrap_or(0)
            };
            if size == 0 {
                continue;
            }
            let pay = if round == "open" { fee } else { 0 };
            let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(500_000), ComputeBudgetInstruction::set_compute_unit_price(1_000)];
            ixs.extend(trade_ixs(c, &st, &racer.pubkey(), *pf, houses[k % used], size, false, pay)?);
            txs.push(Transaction::new_signed_with_payer(&ixs, Some(&racer.pubkey()), &[racer], blockhash));
        }
        let sent = txs.len();
        let (ok, errors) = send_all(c, txs)?;
        println!("{round}: {sent} orders at once over {used} house portfolio(s): {ok} went through");
        for e in errors {
            println!("  {e}");
        }
    }
    Ok(())
}

