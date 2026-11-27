//! Bust matcher: the house's quoting rules (`rules`) for Percolator's `TradeCpi`, so a trader trades against the house
//! without the house signing, and can always close.
//!
//! Percolator calls the matcher with only the request (size, mark, ids) and forbids any account it owns among the
//! matcher's accounts, so the matcher cannot see the trader's position or the market's open interest when it is
//! called. The trader's transaction therefore runs `Prepare` as the instruction right before `TradeCpi`: it reads the
//! market, the trader's portfolio and the pool, checks through the instructions sysvar that the next instruction is a
//! `TradeCpi` for that same market, trader, house portfolio and context, and writes what it read into the context.
//! When Percolator then calls `Match`, the matcher checks through the sysvar that the instruction running is that
//! `TradeCpi` at the top level and that the one before it is this context's `Prepare`. Nothing runs between the two,
//! so the snapshot is the state the trade meets. The snapshot is spent by the match it was taken for.
//!
//! One context serves one house portfolio on one asset. It is a PDA of this program, and Percolator's delegate PDA
//! for (market, house portfolio, house owner, this program, context) must sign every match.
//!
//! Instructions (first byte the tag, then little-endian fields):
//!   0  Match      Percolator's matcher call: req_id u64, asset_index u16, lp_account_id u64, oracle_price u64,
//!                 req_size i128, then 24 unused bytes
//!                 [delegate (signer), context (writable), instructions sysvar]
//!   3  (Percolator's batch call) refused
//!   10 Init       asset_index u16, oi_cap_bps u64, max_gap_bps u64, backing_fee_cap_bps u16
//!                 [house owner (signer, writable), context (writable), market, house portfolio, percolator program,
//!                  mark feed, system program]
//!   11 SetParams  oi_cap_bps u64, max_gap_bps u64, backing_fee_cap_bps u16
//!                 [house owner (signer), context (writable), mark feed]
//!   12 Prepare    [context (writable), market, trader portfolio, mark feed, pool, base vault, quote vault,
//!                  instructions sysvar, quote price account (when the feed has one)]
//!   13 Close      [house owner (signer, writable), context (writable)]: the rent back to the house owner

pub mod layout;
pub mod rules;

use bust_mark::Feed;
use rules::{Params, Pool, Refusal, Snapshot};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::{instructions as ixs, Sysvar},
};

solana_program::declare_id!("7XE8mxQ9UxZ397pR5xrVUjKhv4EeSL3VK9idbfae2yCV");

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process);

pub const TAG_MATCH: u8 = 0;
pub const TAG_BATCH: u8 = 3;
pub const TAG_INIT: u8 = 10;
pub const TAG_SET_PARAMS: u8 = 11;
pub const TAG_PREPARE: u8 = 12;
pub const TAG_CLOSE: u8 = 13;

/// Percolator's `TradeCpi` tag and data length, and the accounts it names: signer, market, trader portfolio, house
/// portfolio, matcher program, matcher context, matcher delegate.
pub const TRADE_CPI: u8 = 10;
pub const TRADE_CPI_LEN: usize = 85;
const TRADE_MARKET: usize = 1;
const TRADE_TRADER: usize = 2;
const TRADE_HOUSE: usize = 3;
const TRADE_MATCHER: usize = 4;
const TRADE_CTX: usize = 5;
const TRADE_DELEGATE: usize = 6;

/// Percolator's matcher return (ABI 3): written into the first 64 bytes of the context.
const ABI: u32 = 3;
const FLAG_VALID: u32 = 1;
const FLAG_PARTIAL_OK: u32 = 2;
const FLAG_BACKING_FEE_CAP_SHIFT: u32 = 8;
const RETURN_LEN: usize = 64;

const MAGIC: &[u8; 8] = b"bustmch1";
pub const CTX_LEN: usize = 408;

/// The context after the return prefix: the house's settings, then the snapshot `Prepare` leaves for `Match`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub bump: u8,
    pub armed: bool,
    pub asset_index: u16,
    pub percolator: Pubkey,
    pub market: Pubkey,
    pub house_portfolio: Pubkey,
    pub house_owner: Pubkey,
    pub delegate: Pubkey,
    pub feed: Pubkey,
    pub oi_cap_bps: u64,
    pub max_gap_bps: u64,
    pub backing_fee_cap_bps: u16,
    // snapshot
    pub snap_slot: u64,
    /// The top-level index of the `TradeCpi` the snapshot was taken for.
    pub snap_index: u16,
    pub snap_trader: Pubkey,
    pub snap_position_q: i128,
    pub snap_oi_long_q: u128,
    pub snap_oi_short_q: u128,
    pub snap_pool_ok: bool,
    pub snap_spot: u64,
    pub snap_base_reserve: u64,
    pub snap_contract_base_atoms: u64,
}

fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

impl Context {
    pub fn load(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() != CTX_LEN || &d[64..72] != MAGIC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            bump: d[72],
            armed: d[73] == 1,
            asset_index: u16_at(d, 74),
            percolator: key_at(d, 76),
            market: key_at(d, 108),
            house_portfolio: key_at(d, 140),
            house_owner: key_at(d, 172),
            delegate: key_at(d, 204),
            feed: key_at(d, 236),
            oi_cap_bps: u64_at(d, 268),
            max_gap_bps: u64_at(d, 276),
            backing_fee_cap_bps: u16_at(d, 284),
            snap_slot: u64_at(d, 288),
            snap_index: u16_at(d, 296),
            snap_trader: key_at(d, 298),
            snap_position_q: u128_at(d, 330) as i128,
            snap_oi_long_q: u128_at(d, 346),
            snap_oi_short_q: u128_at(d, 362),
            snap_pool_ok: d[378] == 1,
            snap_spot: u64_at(d, 379),
            snap_base_reserve: u64_at(d, 387),
            snap_contract_base_atoms: u64_at(d, 395),
        })
    }

    pub fn store(&self, d: &mut [u8]) {
        d[64..72].copy_from_slice(MAGIC);
        d[72] = self.bump;
        d[73] = self.armed as u8;
        d[74..76].copy_from_slice(&self.asset_index.to_le_bytes());
        for (o, k) in [
            (76, &self.percolator),
            (108, &self.market),
            (140, &self.house_portfolio),
            (172, &self.house_owner),
            (204, &self.delegate),
            (236, &self.feed),
            (298, &self.snap_trader),
        ] {
            d[o..o + 32].copy_from_slice(k.as_ref());
        }
        for (o, v) in [(268, self.oi_cap_bps), (276, self.max_gap_bps), (288, self.snap_slot)] {
            d[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        d[284..286].copy_from_slice(&self.backing_fee_cap_bps.to_le_bytes());
        d[296..298].copy_from_slice(&self.snap_index.to_le_bytes());
        d[330..346].copy_from_slice(&self.snap_position_q.to_le_bytes());
        d[346..362].copy_from_slice(&self.snap_oi_long_q.to_le_bytes());
        d[362..378].copy_from_slice(&self.snap_oi_short_q.to_le_bytes());
        d[378] = self.snap_pool_ok as u8;
        for (o, v) in [(379, self.snap_spot), (387, self.snap_base_reserve), (395, self.snap_contract_base_atoms)] {
            d[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    pub fn params(&self) -> Params {
        Params { oi_cap_bps: self.oi_cap_bps, max_gap_bps: self.max_gap_bps }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            position_q: self.snap_position_q,
            oi_long_q: self.snap_oi_long_q,
            oi_short_q: self.snap_oi_short_q,
            pool: self.snap_pool_ok.then_some(Pool { base_reserve: self.snap_base_reserve, spot: self.snap_spot }),
            contract_base_atoms: self.snap_contract_base_atoms,
        }
    }
}

/// The context for a house portfolio: one per (market, house portfolio, house owner).
pub fn context_address(market: &Pubkey, house_portfolio: &Pubkey, house_owner: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"ctx", market.as_ref(), house_portfolio.as_ref(), house_owner.as_ref()], &ID)
}

/// Percolator's delegate PDA that signs this context's matches.
pub fn delegate_address(percolator: &Pubkey, market: &Pubkey, house_portfolio: &Pubkey, house_owner: &Pubkey, ctx: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"matcher", market.as_ref(), house_portfolio.as_ref(), house_owner.as_ref(), ID.as_ref(), ctx.as_ref()],
        percolator,
    )
    .0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum MatchError {
    WrongAccount = 1,
    BadParams,
    /// The instruction before the trade is not this context's `Prepare`, or the snapshot is not for this trade.
    NotPrepared,
    /// Prepare is not followed by a `TradeCpi` for the same market, trader, house portfolio and context.
    NotATrade,
    PercolatorLayout,
    // refusals, one per rule
    BadRequest,
    PoolUnreadable,
    Gap,
    OiCap,
}

impl From<MatchError> for ProgramError {
    fn from(e: MatchError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

impl From<Refusal> for MatchError {
    fn from(r: Refusal) -> Self {
        match r {
            Refusal::BadRequest => MatchError::BadRequest,
            Refusal::PoolUnreadable => MatchError::PoolUnreadable,
            Refusal::Gap => MatchError::Gap,
            Refusal::OiCap => MatchError::OiCap,
        }
    }
}

fn read_u64(d: &mut &[u8]) -> Result<u64, ProgramError> {
    let (v, rest) = d.split_first_chunk::<8>().ok_or(ProgramError::InvalidInstructionData)?;
    *d = rest;
    Ok(u64::from_le_bytes(*v))
}

fn read_u16(d: &mut &[u8]) -> Result<u16, ProgramError> {
    let (v, rest) = d.split_first_chunk::<2>().ok_or(ProgramError::InvalidInstructionData)?;
    *d = rest;
    Ok(u16::from_le_bytes(*v))
}

fn check_params(oi_cap_bps: u64, max_gap_bps: u64, backing_fee_cap_bps: u16) -> ProgramResult {
    if oi_cap_bps == 0 || oi_cap_bps > 100_000 || max_gap_bps > 10_000 || backing_fee_cap_bps > 10_000 {
        return Err(MatchError::BadParams.into());
    }
    Ok(())
}

/// The feed must mark this asset of this market on this Percolator program.
fn check_feed(market: &Pubkey, percolator: &Pubkey, asset_index: u16, feed_ai: &AccountInfo) -> Result<Feed, ProgramError> {
    if *feed_ai.owner != bust_mark::ID {
        return Err(MatchError::WrongAccount.into());
    }
    let feed = Feed::load(&feed_ai.try_borrow_data()?)?;
    if feed.market != *market || feed.percolator != *percolator || feed.asset_index != asset_index {
        return Err(MatchError::WrongAccount.into());
    }
    Ok(feed)
}

fn load_context(program_id: &Pubkey, ai: &AccountInfo) -> Result<Context, ProgramError> {
    if ai.owner != program_id || !ai.is_writable {
        return Err(MatchError::WrongAccount.into());
    }
    Context::load(&ai.try_borrow_data()?)
}

pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (&tag, mut rest) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        TAG_MATCH => matched(program_id, accounts, rest),
        TAG_INIT => init(program_id, accounts, &mut rest),
        TAG_SET_PARAMS => set_params(program_id, accounts, &mut rest),
        TAG_PREPARE => prepare(program_id, accounts),
        TAG_CLOSE => close(program_id, accounts),
        // a batch fills several legs against one snapshot of one trader and asset: not supported
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn init(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let owner_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let house_ai = next_account_info(it)?;
    let percolator_ai = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let system_ai = next_account_info(it)?;
    let (asset_index, oi_cap_bps, max_gap_bps, backing_fee_cap_bps) = (read_u16(d)?, read_u64(d)?, read_u64(d)?, read_u16(d)?);
    check_params(oi_cap_bps, max_gap_bps, backing_fee_cap_bps)?;
    if !owner_ai.is_signer || *system_ai.key != system_program::ID || !percolator_ai.executable {
        return Err(MatchError::WrongAccount.into());
    }
    // the market and the house portfolio are Percolator's, and the house owner owns the portfolio
    if market_ai.owner != percolator_ai.key || house_ai.owner != percolator_ai.key {
        return Err(MatchError::WrongAccount.into());
    }
    layout::asset(&market_ai.try_borrow_data()?, asset_index).ok_or(MatchError::PercolatorLayout)?;
    if layout::portfolio_owner(&house_ai.try_borrow_data()?).ok_or(MatchError::PercolatorLayout)? != owner_ai.key.to_bytes() {
        return Err(MatchError::WrongAccount.into());
    }
    check_feed(market_ai.key, percolator_ai.key, asset_index, feed_ai)?;

    let (ctx_key, bump) = context_address(market_ai.key, house_ai.key, owner_ai.key);
    if *ctx_ai.key != ctx_key {
        return Err(MatchError::WrongAccount.into());
    }
    let seeds: &[&[u8]] = &[b"ctx", market_ai.key.as_ref(), house_ai.key.as_ref(), owner_ai.key.as_ref(), &[bump]];
    let rent = Rent::get()?.minimum_balance(CTX_LEN);
    let create = system_instruction::create_account(owner_ai.key, &ctx_key, rent, CTX_LEN as u64, program_id);
    invoke_signed(&create, &[owner_ai.clone(), ctx_ai.clone(), system_ai.clone()], &[seeds])?;

    let ctx = Context {
        bump,
        armed: false,
        asset_index,
        percolator: *percolator_ai.key,
        market: *market_ai.key,
        house_portfolio: *house_ai.key,
        house_owner: *owner_ai.key,
        delegate: delegate_address(percolator_ai.key, market_ai.key, house_ai.key, owner_ai.key, &ctx_key),
        feed: *feed_ai.key,
        oi_cap_bps,
        max_gap_bps,
        backing_fee_cap_bps,
        snap_slot: 0,
        snap_index: 0,
        snap_trader: Pubkey::default(),
        snap_position_q: 0,
        snap_oi_long_q: 0,
        snap_oi_short_q: 0,
        snap_pool_ok: false,
        snap_spot: 0,
        snap_base_reserve: 0,
        snap_contract_base_atoms: 0,
    };
    ctx.store(&mut ctx_ai.try_borrow_mut_data()?);
    msg!("context {} delegate {}", ctx_key, ctx.delegate);
    Ok(())
}

fn set_params(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let owner_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let (oi_cap_bps, max_gap_bps, backing_fee_cap_bps) = (read_u64(d)?, read_u64(d)?, read_u16(d)?);
    check_params(oi_cap_bps, max_gap_bps, backing_fee_cap_bps)?;
    let mut ctx = load_context(program_id, ctx_ai)?;
    if !owner_ai.is_signer || *owner_ai.key != ctx.house_owner {
        return Err(MatchError::WrongAccount.into());
    }
    check_feed(&ctx.market, &ctx.percolator, ctx.asset_index, feed_ai)?;
    ctx.feed = *feed_ai.key;
    ctx.oi_cap_bps = oi_cap_bps;
    ctx.max_gap_bps = max_gap_bps;
    ctx.backing_fee_cap_bps = backing_fee_cap_bps;
    ctx.armed = false;
    ctx.store(&mut ctx_ai.try_borrow_mut_data()?);
    Ok(())
}

/// Reads what the trade will meet and leaves it in the context for `Match`. Only a binding error fails it: a pool
/// that cannot be read is recorded as such, so the trader can still close.
fn prepare(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let ctx_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let trader_ai = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let pool_ai = next_account_info(it)?;
    let base_ai = next_account_info(it)?;
    let quote_ai = next_account_info(it)?;
    let sysvar_ai = next_account_info(it)?;
    let price_ai = next_account_info(it).ok();
    let mut ctx = load_context(program_id, ctx_ai)?;
    if *market_ai.key != ctx.market || *market_ai.owner != ctx.percolator || *trader_ai.owner != ctx.percolator || *feed_ai.key != ctx.feed {
        return Err(MatchError::WrongAccount.into());
    }
    if !ixs::check_id(sysvar_ai.key) {
        return Err(MatchError::WrongAccount.into());
    }

    // the next instruction is Percolator's TradeCpi for this market, trader, house portfolio and context
    let here = ixs::load_current_index_checked(sysvar_ai)?;
    let next = ixs::load_instruction_at_checked(here as usize + 1, sysvar_ai).map_err(|_| MatchError::NotATrade)?;
    let names = |i: usize, key: &Pubkey| next.accounts.get(i).is_some_and(|m| m.pubkey == *key);
    if next.program_id != ctx.percolator
        || next.data.len() != TRADE_CPI_LEN
        || next.data[0] != TRADE_CPI
        || u16::from_le_bytes([next.data[41], next.data[42]]) != ctx.asset_index
        || !names(TRADE_MARKET, &ctx.market)
        || !names(TRADE_TRADER, trader_ai.key)
        || !names(TRADE_HOUSE, &ctx.house_portfolio)
        || !names(TRADE_MATCHER, program_id)
        || !names(TRADE_CTX, ctx_ai.key)
        || !names(TRADE_DELEGATE, &ctx.delegate)
    {
        return Err(MatchError::NotATrade.into());
    }

    let asset = layout::asset(&market_ai.try_borrow_data()?, ctx.asset_index).ok_or(MatchError::PercolatorLayout)?;
    let position_q = layout::position(&trader_ai.try_borrow_data()?, ctx.asset_index, &asset).ok_or(MatchError::PercolatorLayout)?;
    let feed = check_feed(&ctx.market, &ctx.percolator, ctx.asset_index, feed_ai)?;
    let clock = Clock::get()?;
    let pool = bust_mark::pool_reading(&feed, pool_ai, base_ai, quote_ai, price_ai, clock.unix_timestamp);
    if let Err(e) = &pool {
        // the wrong pool or vaults is the caller's mistake; a pool that cannot be read now only stops opening
        if *e == ProgramError::Custom(bust_mark::MarkError::WrongAccount as u32) {
            return Err(MatchError::WrongAccount.into());
        }
        msg!("pool unreadable: {}", e);
    }

    ctx.armed = true;
    ctx.snap_slot = clock.slot;
    ctx.snap_index = here + 1;
    ctx.snap_trader = *trader_ai.key;
    ctx.snap_position_q = position_q;
    ctx.snap_oi_long_q = asset.oi_long_q;
    ctx.snap_oi_short_q = asset.oi_short_q;
    ctx.snap_pool_ok = pool.is_ok();
    (ctx.snap_base_reserve, ctx.snap_spot) = pool.unwrap_or((0, 0));
    ctx.snap_contract_base_atoms = feed.contract_base_atoms;
    ctx.store(&mut ctx_ai.try_borrow_mut_data()?);
    Ok(())
}

/// Percolator's call: fills the request by the rules against the snapshot taken right before it.
fn matched(program_id: &Pubkey, accounts: &[AccountInfo], d: &[u8]) -> ProgramResult {
    if d.len() < 42 {
        return Err(ProgramError::InvalidInstructionData);
    }
    let req_id = u64::from_le_bytes(d[0..8].try_into().unwrap());
    let asset_index = u16::from_le_bytes(d[8..10].try_into().unwrap());
    let lp_account_id = u64::from_le_bytes(d[10..18].try_into().unwrap());
    let mark = u64::from_le_bytes(d[18..26].try_into().unwrap());
    let req_q = i128::from_le_bytes(d[26..42].try_into().unwrap());

    let it = &mut accounts.iter();
    let delegate_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    let sysvar_ai = next_account_info(it)?;
    let mut ctx = load_context(program_id, ctx_ai)?;
    if !delegate_ai.is_signer || *delegate_ai.key != ctx.delegate || asset_index != ctx.asset_index || !ixs::check_id(sysvar_ai.key) {
        return Err(MatchError::WrongAccount.into());
    }

    // the running top-level instruction is the TradeCpi the snapshot was taken for, and the one before it is this
    // context's Prepare: Percolator called from another program, or a second trade on one snapshot, is refused
    let here = ixs::load_current_index_checked(sysvar_ai)?;
    let trade = ixs::load_instruction_at_checked(here as usize, sysvar_ai)?;
    let prev = match here.checked_sub(1) {
        Some(i) => ixs::load_instruction_at_checked(i as usize, sysvar_ai)?,
        None => return Err(MatchError::NotPrepared.into()),
    };
    let trade_names = |i: usize, key: &Pubkey| trade.accounts.get(i).is_some_and(|m| m.pubkey == *key);
    if !ctx.armed
        || ctx.snap_index != here
        || ctx.snap_slot != Clock::get()?.slot
        || trade.program_id != ctx.percolator
        || trade.data.first() != Some(&TRADE_CPI)
        || !trade_names(TRADE_CTX, ctx_ai.key)
        || !trade_names(TRADE_TRADER, &ctx.snap_trader)
        || prev.program_id != *program_id
        || prev.data.first() != Some(&TAG_PREPARE)
        || prev.accounts.first().map(|m| m.pubkey) != Some(*ctx_ai.key)
    {
        return Err(MatchError::NotPrepared.into());
    }

    let filled = rules::fill(&ctx.snapshot(), &ctx.params(), mark, req_q).map_err(|r| {
        msg!(
            "refused: {:?}; request {} position {} mark {} pool {} ({}) oi {} / {}",
            r,
            req_q,
            ctx.snap_position_q,
            mark,
            ctx.snap_spot,
            if ctx.snap_pool_ok { "read" } else { "unreadable" },
            ctx.snap_oi_long_q,
            ctx.snap_oi_short_q
        );
        ProgramError::from(MatchError::from(r))
    })?;
    ctx.armed = false;
    let mut data = ctx_ai.try_borrow_mut_data()?;
    ctx.store(&mut data);

    let mut flags = FLAG_VALID | ((ctx.backing_fee_cap_bps as u32) << FLAG_BACKING_FEE_CAP_SHIFT);
    if filled != req_q {
        flags |= FLAG_PARTIAL_OK;
    }
    let out = &mut data[..RETURN_LEN];
    out[0..4].copy_from_slice(&ABI.to_le_bytes());
    out[4..8].copy_from_slice(&flags.to_le_bytes());
    // the engine books at its own mark: the price only sizes the fee, and Percolator wants it non-zero
    out[8..16].copy_from_slice(&mark.to_le_bytes());
    out[16..32].copy_from_slice(&filled.to_le_bytes());
    out[32..40].copy_from_slice(&req_id.to_le_bytes());
    out[40..48].copy_from_slice(&lp_account_id.to_le_bytes());
    out[48..56].copy_from_slice(&mark.to_le_bytes());
    out[56..64].copy_from_slice(&(asset_index as u64).to_le_bytes());
    msg!("filled {} of {} at {}", filled, req_q, mark);
    Ok(())
}

fn close(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let owner_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    let ctx = load_context(program_id, ctx_ai)?;
    if !owner_ai.is_signer || *owner_ai.key != ctx.house_owner {
        return Err(MatchError::WrongAccount.into());
    }
    let lamports = ctx_ai.lamports();
    **ctx_ai.try_borrow_mut_lamports()? = 0;
    **owner_ai.try_borrow_mut_lamports()? += lamports;
    ctx_ai.try_borrow_mut_data()?.fill(0);
    ctx_ai.assign(&system_program::ID);
    ctx_ai.realloc(0, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_round_trips() {
        let ctx = Context {
            bump: 254,
            armed: true,
            asset_index: 3,
            percolator: Pubkey::new_unique(),
            market: Pubkey::new_unique(),
            house_portfolio: Pubkey::new_unique(),
            house_owner: Pubkey::new_unique(),
            delegate: Pubkey::new_unique(),
            feed: Pubkey::new_unique(),
            oi_cap_bps: 500,
            max_gap_bps: 50,
            backing_fee_cap_bps: 7,
            snap_slot: 123_456,
            snap_index: 4,
            snap_trader: Pubkey::new_unique(),
            snap_position_q: -42_000_000,
            snap_oi_long_q: 1 << 100,
            snap_oi_short_q: 99,
            snap_pool_ok: true,
            snap_spot: 4_000_000,
            snap_base_reserve: 500_000_000_000_000,
            snap_contract_base_atoms: 1_000_000_000_000,
        };
        let mut d = vec![0u8; CTX_LEN];
        ctx.store(&mut d);
        assert_eq!(Context::load(&d).unwrap(), ctx);
        // the return prefix is left alone
        assert!(d[..RETURN_LEN].iter().all(|&b| b == 0));
    }
}
