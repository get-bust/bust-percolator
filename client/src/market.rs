//! Opening the market: the market group account, its vault, the authority mark, two portfolios and their deposits.

use crate::ctx::{eng, market_key, pk, read_state, vault_accounts, write_state, Ctx};
use anyhow::Result;
use percolator_prog::{
    constants::PORTFOLIO_ACCOUNT_LEN,
    ix::Instruction as Ix,
    risk::{BackingBucketStatusV16, BackingBucketV16, BOUND_SCALE},
    state,
};
use serde_json::{json, Value};
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey, signature::{Keypair, Signer}, system_instruction};

/// Funds the trader starts with in its token account, and what it deposits.
const TRADER_TOKENS: u64 = 1_000;
const TRADER_DEPOSIT: u128 = 100_000_000;
/// The house's capital as the counterparty, and the insurance it puts in front of it.
const HOUSE_TOKENS: u64 = 20_000;
const HOUSE_DEPOSIT: u128 = 10_000_000_000;
const INSURANCE: u128 = 1_000_000_000;

/// Creates the market (or resumes an existing one), sets its first mark and opens and funds both portfolios.
/// Opens the market. `move_bps_per_slot` and `accrual_dt_slots` set how fast the mark may move and how many slots a
/// crank accrues; Percolator holds their product under the maintenance margin.
pub fn setup(c: &Ctx, initial_mark: u64, move_bps_per_slot: u64, accrual_dt_slots: u64, max_funding_e9: u64) -> Result<()> {
    let mint = c.collateral;
    let market_kp = market_key("market.json")?;
    let house_pf = market_key("house-portfolio.json")?;
    let trader_pf = market_key("trader-portfolio.json")?;
    let market = market_kp.pubkey();
    let (vault_authority, vault) = vault_accounts(&c.program, &market, &mint);

    // SOL for the trader's own signatures.
    if c.rpc.get_balance(&c.trader.pubkey())? < 50_000_000 {
        c.send(vec![system_instruction::transfer(&c.house.pubkey(), &c.trader.pubkey(), 100_000_000)], &[&c.house])?;
    }

    let market_len = eng(state::market_account_len_for_capacity(1))?;
    let rent_market = c.rpc.get_minimum_balance_for_rent_exemption(market_len)?;
    let rent_portfolio = c.rpc.get_minimum_balance_for_rent_exemption(PORTFOLIO_ACCOUNT_LEN)?;
    let ata = |owner: &Pubkey| {
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(&c.house.pubkey(), owner, &mint, &spl_token::ID)
    };

    if c.rpc.get_account(&market).is_err() {
        let sig = c.send(
            vec![
                system_instruction::create_account(&c.house.pubkey(), &market, rent_market, market_len as u64, &c.program),
                ata(&vault_authority),
                ata(&c.trader.pubkey()),
                ata(&c.house.pubkey()),
            ],
            &[&c.house, &market_kp],
        )?;
        println!("market account and vault: {sig}");

        // A memecoin profile: 10% initial margin (10x), 5% maintenance; the mark moves at most `move_bps_per_slot` a
        // slot and at most `accrual_dt_slots` are accrued per crank (by default 10 bps and 20 slots, so one crank
        // moves it at most 2%). A loser's payment backs the
        // winners' claims for the freshness horizon, the largest of h_max, the accrual window and the bankrupt-close
        // lifetime: 216,000 slots (about a day) gives a winner a day to convert its profit.
        let init = Ix::InitMarket {
            max_portfolio_assets: 1,
            h_min: 0,
            h_max: 150,
            initial_price: initial_mark,
            min_nonzero_mm_req: 1,
            min_nonzero_im_req: 2,
            maintenance_margin_bps: 500,
            initial_margin_bps: 1_000,
            max_trading_fee_bps: 100,
            trade_fee_base_bps: 0,
            liquidation_fee_bps: 0,
            liquidation_fee_cap: 0,
            min_liquidation_abs: 0,
            max_price_move_bps_per_slot: move_bps_per_slot,
            max_accrual_dt_slots: accrual_dt_slots,
            // an AuthMark asset charges no funding, but with a nonzero bound a trade accrues the asset to its slot
            // itself (zero-move funding) instead of needing cranks in its transaction
            max_abs_funding_e9_per_slot: max_funding_e9,
            min_funding_lifetime_slots: accrual_dt_slots,
            max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1,
            max_bankrupt_close_lifetime_slots: 216_000,
            maintenance_fee_per_slot: 0,
        };
        let sig = c.send(
            vec![c.prog(
                init,
                vec![AccountMeta::new(c.house.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new_readonly(mint, false)],
            )],
            &[&c.house],
        )?;
        println!("InitMarket: {sig}");
    }

    let st = json!({
        "program": c.program.to_string(), "market": market.to_string(), "mint": mint.to_string(),
        "vault": vault.to_string(), "vaultAuthority": vault_authority.to_string(),
        "house": c.house.pubkey().to_string(), "trader": c.trader.pubkey().to_string(),
        "housePortfolio": house_pf.pubkey().to_string(), "traderPortfolio": trader_pf.pubkey().to_string(),
    });
    write_state(&st)?;
    configure_mark(c, &st, initial_mark)?;

    for (owner, pf) in [(&c.house, &house_pf), (&c.trader, &trader_pf)] {
        let sig = c.send(
            vec![
                system_instruction::create_account(&c.house.pubkey(), &pf.pubkey(), rent_portfolio, PORTFOLIO_ACCOUNT_LEN as u64, &c.program),
                c.prog(
                    Ix::InitPortfolio,
                    vec![AccountMeta::new(owner.pubkey(), true), AccountMeta::new(market, false), AccountMeta::new(pf.pubkey(), false)],
                ),
            ],
            &[&c.house, pf, owner],
        )?;
        println!("portfolio {} for {}: {sig}", pf.pubkey(), owner.pubkey());
    }

    for (owner, tokens) in [(&c.house, HOUSE_TOKENS), (&c.trader, TRADER_TOKENS)] {
        let account = spl_associated_token_account::get_associated_token_address(&owner.pubkey(), &mint);
        c.send(
            vec![spl_token::instruction::mint_to(&spl_token::ID, &mint, &account, &c.mint_authority.pubkey(), &[], tokens * 1_000_000)?],
            &[&c.house, &c.mint_authority],
        )?;
    }
    deposit(c, &st, true, HOUSE_DEPOSIT)?;
    deposit(c, &st, false, TRADER_DEPOSIT)?;

    let data = c.data(&market)?;
    let (_, group) = eng(state::read_market(&data))?;
    let seq = eng(state::read_asset_control_sequences(&data, 0))?;
    let house_tokens = spl_associated_token_account::get_associated_token_address(&c.house.pubkey(), &mint);
    let sig = c.send(
        vec![c.prog(
            Ix::TopUpInsurance {
                market_id: group.assets[0].market_id,
                intent_id: seq.insurance_top_up + 1,
                authority_epoch: seq.authority_epoch,
                amount: INSURANCE,
            },
            vec![
                AccountMeta::new(c.house.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(house_tokens, false),
                AccountMeta::new(vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
        )],
        &[&c.house],
    )?;
    println!("insurance {}: {sig}", INSURANCE as f64 / 1e6);
    show(c)
}

/// Switches asset 0 to an authority-pushed mark (AuthMark) at `mark`, signed by the house as oracle authority.
/// Control instructions carry the asset's current authority epoch and the next observation sequence.
pub fn configure_mark(c: &Ctx, st: &Value, mark: u64) -> Result<()> {
    let market = pk(st, "market")?;
    let data = c.data(&market)?;
    let (_, group) = eng(state::read_market(&data))?;
    let seq = eng(state::read_asset_control_sequences(&data, 0))?;
    let sig = c.send(
        vec![c.prog(
            Ix::ConfigureAuthMark {
                asset_index: 0,
                market_id: group.assets[0].market_id,
                now_slot: c.rpc.get_slot()?,
                initial_mark_e6: mark,
                observation_sequence: seq.oracle_observation + 1,
                authority_epoch: seq.authority_epoch,
            },
            vec![AccountMeta::new(c.house.pubkey(), true), AccountMeta::new(market, false)],
        )],
        &[&c.house],
    )?;
    println!("ConfigureAuthMark {mark}: {sig}");
    Ok(())
}

/// Deposits `amount` collateral atoms into the house's or the trader's portfolio.
pub fn deposit(c: &Ctx, st: &Value, house: bool, amount: u128) -> Result<()> {
    let (owner, pf) = if house { (&c.house, pk(st, "housePortfolio")?) } else { (&c.trader, pk(st, "traderPortfolio")?) };
    deposit_into(c, st, owner, &pf, amount)
}

/// `owner` deposits `amount` collateral atoms from its token account into its portfolio `pf`.
pub fn deposit_into(c: &Ctx, st: &Value, owner: &Keypair, pf: &Pubkey, amount: u128) -> Result<()> {
    let pf = *pf;
    let data = c.data(&pf)?;
    let source = spl_associated_token_account::get_associated_token_address(&owner.pubkey(), &pk(st, "mint")?);
    let sig = c.send(
        vec![c.prog(
            Ix::Deposit {
                portfolio_id: eng(state::read_portfolio_id(&data))?,
                expected_sequence: eng(state::read_portfolio_matcher_sequence(&data))?,
                amount,
            },
            vec![
                AccountMeta::new(owner.pubkey(), true),
                AccountMeta::new(pk(st, "market")?, false),
                AccountMeta::new(pf, false),
                AccountMeta::new(source, false),
                AccountMeta::new(pk(st, "vault")?, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
        )],
        &[owner],
    )?;
    println!("deposit {} into {pf}: {sig}", amount as f64 / 1e6);
    Ok(())
}

/// The asset's target and effective mark, open interest, vault and insurance, and both portfolios.
pub fn show(c: &Ctx) -> Result<()> {
    let st = read_state()?;
    let (_, group) = eng(state::read_market(&c.data(&pk(&st, "market")?)?))?;
    let a = &group.assets[0];
    println!(
        "mark target {:.4} effective {:.4} | OI long {:.2} short {:.2} contracts | vault {:.2} insurance {:.2} | slot {}",
        a.raw_oracle_target_price as f64 / 1e6,
        a.effective_price as f64 / 1e6,
        a.oi_eff_long_q as f64 / 1e6,
        a.oi_eff_short_q as f64 / 1e6,
        group.vault as f64 / 1e6,
        group.insurance as f64 / 1e6,
        group.current_slot
    );
    for (name, key) in [("trader", "traderPortfolio"), ("house", "housePortfolio")] {
        let p = eng(state::read_portfolio(&c.data(&pk(&st, key)?)?))?;
        println!(
            "  {name:6} capital {:.4} pnl {:.4} reserved {:.4} position {:.4} contracts",
            p.capital.get() as f64 / 1e6,
            p.pnl.get() as f64 / 1e6,
            p.reserved_pnl.get() as f64 / 1e6,
            p.legs[0].basis_pos_q.get() as f64 / 1e6,
        );
    }
    Ok(())
}

/// Backing lasts this many slots from a top-up, about a year at 400 ms. A live market puts no cap on a backing's
/// expiry, and a bucket that is still fresh takes more backing only at the expiry it already has, so a short expiry
/// cannot be pushed out while it runs: the house sets a far one instead of renewing.
const BACKING_SLOTS: u64 = 78_840_000;

/// Each side's backing bucket, and the expiry a top-up must carry: a fresh bucket's own, otherwise BACKING_SLOTS on
/// from now.
fn backing_bucket(c: &Ctx, group: &state::MarketGroupV16, domain: u16) -> Result<(BackingBucketV16, u64)> {
    let bucket = group.source_backing_buckets[domain as usize].clone();
    let now = c.rpc.get_slot()?.max(group.current_slot);
    let expiry = if bucket.status == BackingBucketStatusV16::Fresh && bucket.expiry_slot > now {
        bucket.expiry_slot
    } else {
        now + BACKING_SLOTS
    };
    Ok((bucket, expiry))
}

/// The house backs both sides of asset 0 with `amount` collateral atoms each: domain 1 pays the asset's long winners,
/// domain 0 its short winners. A winner is paid only from the backing of its own domain, and only a position opened
/// while that backing was in place holds a claim on it; profit with no claim stays junior and cannot be withdrawn.
pub fn back(c: &Ctx, st: &Value, amount: u128) -> Result<()> {
    let market = pk(st, "market")?;
    for domain in [0u16, 1u16] {
        let data = c.data(&market)?;
        let (_, group) = eng(state::read_market(&data))?;
        let (_, expiry) = backing_bucket(c, &group, domain)?;
        let sig = c.send(vec![top_up_ix(c, st, &data, &group, domain, amount, expiry)?], &[&c.house])?;
        println!("backing domain {domain}: {} until slot {expiry} ({sig})", amount as f64 / 1e6);
    }
    show(c)
}

/// Moves each side's backing to a far expiry: the bucket's unliened backing is withdrawn and the same amount topped
/// back up at BACKING_SLOTS from now, in one transaction. A bucket takes a new expiry only once it is empty, and only
/// backing no open position holds a claim on can be withdrawn, so a side with claims is left until they close.
pub fn renew(c: &Ctx, st: &Value) -> Result<()> {
    let market = pk(st, "market")?;
    let mint = pk(st, "mint")?;
    let house_tokens = spl_associated_token_account::get_associated_token_address(&c.house.pubkey(), &mint);
    for domain in [0u16, 1u16] {
        let data = c.data(&market)?;
        let (_, group) = eng(state::read_market(&data))?;
        let (bucket, _) = backing_bucket(c, &group, domain)?;
        let now = c.rpc.get_slot()?.max(group.current_slot);
        if bucket.status != BackingBucketStatusV16::Fresh || bucket.expiry_slot <= now {
            println!("backing domain {domain}: {:?} at slot {}, nothing fresh to renew", bucket.status, bucket.expiry_slot);
            continue;
        }
        if bucket.expiry_slot >= now + BACKING_SLOTS / 2 {
            println!("backing domain {domain}: runs until slot {}, far enough", bucket.expiry_slot);
            continue;
        }
        if bucket.valid_liened_backing_num != 0 || bucket.fresh_unliened_backing_num % BOUND_SCALE != 0 {
            println!("backing domain {domain}: open positions hold claims on it; renew it once they close");
            continue;
        }
        // The engine lets no backing leave while a loss is outstanding anywhere in the market
        let barrier = group.pending_domain_loss_barriers[domain as usize];
        if group.negative_pnl_account_count != 0
            || barrier != 0
            || group.threshold_stress_active
            || group.loss_stale_active
            || group.recovery_reason.is_some()
        {
            println!(
                "backing domain {domain}: a loss is outstanding (portfolios with negative PnL {}, loss barrier {barrier}, \
                 stress {}, stale loss {}, recovery {:?}); crank the portfolios that carry it, then renew",
                group.negative_pnl_account_count,
                group.threshold_stress_active,
                group.loss_stale_active,
                group.recovery_reason
            );
            continue;
        }
        let amount = bucket.fresh_unliened_backing_num / BOUND_SCALE;
        let seq = eng(state::read_asset_control_sequences(&data, 0))?;
        let withdraw = c.prog(
            Ix::WithdrawBackingBucket {
                domain,
                market_id: group.assets[0].market_id,
                authority_epoch: seq.authority_epoch,
                amount,
            },
            vec![
                AccountMeta::new(c.house.pubkey(), true),
                AccountMeta::new(market, false),
                AccountMeta::new(house_tokens, false),
                AccountMeta::new(pk(st, "vault")?, false),
                AccountMeta::new_readonly(pk(st, "vaultAuthority")?, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
        );
        let expiry = now + BACKING_SLOTS;
        let top_up = top_up_ix(c, st, &data, &group, domain, amount, expiry)?;
        // The part of a bucket the losers paid in leaves only once every position is settled and the other side's
        // claims are backed; the engine refuses the whole withdrawal otherwise, and the bucket keeps its expiry
        match c.send(vec![withdraw, top_up], &[&c.house]) {
            Ok(sig) => println!("backing domain {domain}: {} moved from slot {} to {expiry} ({sig})", amount as f64 / 1e6, bucket.expiry_slot),
            Err(e) => println!(
                "backing domain {domain}: the engine kept it at slot {} ({})",
                bucket.expiry_slot,
                e.to_string().lines().find(|l| l.contains("custom program error")).unwrap_or("refused").trim()
            ),
        }
    }
    show(c)
}

fn top_up_ix(
    c: &Ctx,
    st: &Value,
    data: &[u8],
    group: &state::MarketGroupV16,
    domain: u16,
    amount: u128,
    expiry_slot: u64,
) -> Result<solana_sdk::instruction::Instruction> {
    let seq = eng(state::read_asset_control_sequences(data, 0))?;
    let profile = eng(state::read_asset_oracle_profile(data, 0))?;
    let (backing_fee_bps, insurance_share_bps) = if domain == 0 {
        (profile.backing_trade_fee_bps_long, profile.backing_trade_fee_insurance_share_bps_long)
    } else {
        (profile.backing_trade_fee_bps_short, profile.backing_trade_fee_insurance_share_bps_short)
    };
    let house_tokens = spl_associated_token_account::get_associated_token_address(&c.house.pubkey(), &pk(st, "mint")?);
    Ok(c.prog(
        Ix::TopUpBackingBucket {
            domain,
            market_id: group.assets[0].market_id,
            intent_id: seq.backing_top_up + 1,
            authority_epoch: seq.authority_epoch,
            backing_fee_bps,
            insurance_share_bps,
            amount,
            expiry_slot,
        },
        vec![
            AccountMeta::new(c.house.pubkey(), true),
            AccountMeta::new(pk(st, "market")?, false),
            AccountMeta::new(house_tokens, false),
            AccountMeta::new(pk(st, "vault")?, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
    ))
}
