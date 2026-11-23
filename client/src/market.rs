//! Opening the market: the market group account, its vault, the authority mark, two portfolios and their deposits.

use crate::ctx::{eng, load_or_new, pk, read_state, vault_accounts, write_state, Ctx};
use anyhow::Result;
use percolator_prog::{constants::PORTFOLIO_ACCOUNT_LEN, ix::Instruction as Ix, state};
use serde_json::{json, Value};
use solana_sdk::{instruction::AccountMeta, pubkey::Pubkey, signature::Signer, system_instruction};

/// Funds the trader starts with in its token account, and what it deposits.
const TRADER_TOKENS: u64 = 1_000;
const TRADER_DEPOSIT: u128 = 100_000_000;
/// The house's capital as the counterparty, and the insurance it puts in front of it.
const HOUSE_TOKENS: u64 = 20_000;
const HOUSE_DEPOSIT: u128 = 10_000_000_000;
const INSURANCE: u128 = 1_000_000_000;

/// Creates the market (or resumes an existing one), sets its first mark and opens and funds both portfolios.
pub fn setup(c: &Ctx, initial_mark: u64) -> Result<()> {
    let mint = c.collateral;
    let market_kp = load_or_new("market.json")?;
    let house_pf = load_or_new("house-portfolio.json")?;
    let trader_pf = load_or_new("trader-portfolio.json")?;
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

        // A memecoin profile: 10% initial margin (10x), 5% maintenance, the mark moves at most 10 bps per slot and
        // at most 20 slots are accrued per crank, so one crank moves it at most 2%.
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
            max_price_move_bps_per_slot: 10,
            max_accrual_dt_slots: 20,
            max_abs_funding_e9_per_slot: 0,
            min_funding_lifetime_slots: 20,
            max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1,
            max_bankrupt_close_lifetime_slots: 1_000,
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
