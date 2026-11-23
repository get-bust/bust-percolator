//! The authority mark and the permissionless crank that moves the effective price toward it.

use crate::ctx::{eng, pk, Ctx};
use anyhow::Result;
use percolator_prog::{
    ix::{CrankObservationHint, Instruction as Ix},
    state,
};
use serde_json::Value;
use solana_sdk::{instruction::AccountMeta, instruction::Instruction, pubkey::Pubkey, signature::Signer};

/// Pushes one authority mark for asset 0. The engine does not jump to it: each crank moves the effective price toward
/// the mark by at most the per-slot cap times the slots it accrues.
pub fn push(c: &Ctx, st: &Value, mark: u64) -> Result<String> {
    let market = pk(st, "market")?;
    let data = c.data(&market)?;
    let (_, group) = eng(state::read_market(&data))?;
    let seq = eng(state::read_asset_control_sequences(&data, 0))?;
    c.send(
        vec![c.prog(
            Ix::PushAuthMark {
                asset_index: 0,
                market_id: group.assets[0].market_id,
                now_slot: c.rpc.get_slot()?.max(group.current_slot),
                mark_e6: mark,
                observation_sequence: seq.oracle_observation + 1,
                authority_epoch: seq.authority_epoch,
            },
            vec![AccountMeta::new(c.house.pubkey(), true), AccountMeta::new(market, false)],
        )],
        &[&c.house],
    )
}

/// A crank of one portfolio at `slot`. An AuthMark asset is observed with no oracle accounts.
pub fn crank_ix(c: &Ctx, signer: &Pubkey, market: Pubkey, portfolio: Pubkey, slot: u64) -> Instruction {
    c.prog(
        Ix::PermissionlessCrank { now_slot: slot, observations: vec![CrankObservationHint { asset_index: 0, oracle_accounts: 0 }] },
        vec![AccountMeta::new(*signer, true), AccountMeta::new(market, false), AccountMeta::new(portfolio, false)],
    )
}

/// Cranks the trader's or the house's portfolio; anyone may, the house pays here.
pub fn crank(c: &Ctx, st: &Value, who: &str) -> Result<String> {
    let portfolio = if who == "house" { pk(st, "housePortfolio")? } else { pk(st, "traderPortfolio")? };
    let ix = crank_ix(c, &c.house.pubkey(), pk(st, "market")?, portfolio, c.rpc.get_slot()?);
    c.send(vec![ix], &[&c.house])
}
