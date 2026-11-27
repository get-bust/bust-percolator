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
//! The pool price in the trade's own transaction is the trader's to move, so the gap is measured against the prices
//! the house's keeper sampled over the last `window_secs` as well (`rules`): `Observe`, signed by the context's
//! observer key, samples the pool in a transaction of its own (the keeper sends it with every feed update). Without a
//! keeper sample in the window nothing opens; closing never needs one. A trader pays the gap with a token transfer to
//! the house's fee account as the instruction right before `Prepare`.
//!
//! One context serves one house portfolio on one asset. It is a PDA of this program, and Percolator's delegate PDA
//! for (market, house portfolio, house owner, this program, context) must sign every match.
//!
//! Instructions (first byte the tag, then little-endian fields):
//!   0  Match      Percolator's matcher call: req_id u64, asset_index u16, lp_account_id u64, oracle_price u64,
//!                 req_size i128, then 24 unused bytes
//!                 [delegate (signer), context (writable), instructions sysvar]
//!   3  (Percolator's batch call) refused
//!   10 Init       asset_index u16, oi_cap_bps u64, max_gap_bps u64, paid_max_gap_bps u64, window_secs u64,
//!                 backing_fee_cap_bps u16
//!                 [house owner (signer, writable), context (writable), market, house portfolio, percolator program,
//!                  mark feed, fee account (a collateral token account), observer, system program]
//!   11 SetParams  oi_cap_bps u64, max_gap_bps u64, paid_max_gap_bps u64, window_secs u64, backing_fee_cap_bps u16
//!                 [house owner (signer), context (writable), mark feed, fee account, observer]
//!   12 Prepare    [context (writable), market, trader portfolio, mark feed, pool, base vault, quote vault,
//!                  instructions sysvar, quote price account (when the feed has one)]
//!   13 Close      [house owner (signer, writable), context (writable)]: the rent back to the house owner; closes a
//!                  context of an earlier layout too
//!   14 Observe    [observer (signer), context (writable), mark feed, pool, base vault, quote vault, quote price
//!                  account (when the feed has one)]

pub mod layout;
pub mod rules;

use bust_mark::Feed;
use rules::{Bucket, Params, Pool, Refusal, Snapshot, BUCKETS};
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
pub const TAG_OBSERVE: u8 = 14;

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

/// SPL Token `Transfer` and `TransferChecked`, and where each names the destination.
const TOKEN_TRANSFER: u8 = 3;
const TOKEN_TRANSFER_CHECKED: u8 = 12;

/// Percolator's matcher return (ABI 3): written into the first 64 bytes of the context.
const ABI: u32 = 3;
const FLAG_VALID: u32 = 1;
const FLAG_PARTIAL_OK: u32 = 2;
const FLAG_BACKING_FEE_CAP_SHIFT: u32 = 8;
const RETURN_LEN: usize = 64;

/// Every layout's magic starts with the first seven bytes; the eighth is the layout.
const MAGIC: &[u8; 8] = b"bustmch3";
pub const CTX_LEN: usize = 664;
/// Where every layout keeps the house owner, for `Close`.
const OWNER_OFF: usize = 172;
const BUCKETS_OFF: usize = 520;
const BUCKET_LEN: usize = 24;

/// The context after the return prefix: the house's settings, the snapshot `Prepare` leaves for `Match`, and the
/// sampled pool prices.
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
    /// The collateral token account the gap is paid to.
    pub fee_account: Pubkey,
    /// The keeper key that samples the pool.
    pub observer: Pubkey,
    pub oi_cap_bps: u64,
    pub max_gap_bps: u64,
    pub paid_max_gap_bps: u64,
    pub window_secs: u64,
    pub backing_fee_cap_bps: u16,
    /// When the keeper last sampled the pool.
    pub last_observed: i64,
    // snapshot
    pub snap_slot: u64,
    /// The top-level index of the `TradeCpi` the snapshot was taken for.
    pub snap_index: u16,
    pub snap_pool_ok: bool,
    /// Whether the keeper sampled the pool within the window of the snapshot.
    pub snap_sampled: bool,
    pub snap_trader: Pubkey,
    pub snap_position_q: i128,
    pub snap_oi_long_q: u128,
    pub snap_oi_short_q: u128,
    pub snap_spot: u64,
    pub snap_high: u64,
    pub snap_low: u64,
    pub snap_base_reserve: u64,
    pub snap_contract_base_atoms: u64,
    pub snap_fee_paid: u64,
    pub buckets: [Bucket; BUCKETS],
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
        let mut buckets = [Bucket::default(); BUCKETS];
        for (k, b) in buckets.iter_mut().enumerate() {
            let o = BUCKETS_OFF + k * BUCKET_LEN;
            *b = Bucket { start: u64_at(d, o) as i64, high: u64_at(d, o + 8), low: u64_at(d, o + 16) };
        }
        Ok(Self {
            bump: d[72],
            armed: d[73] == 1,
            asset_index: u16_at(d, 74),
            percolator: key_at(d, 76),
            market: key_at(d, 108),
            house_portfolio: key_at(d, 140),
            house_owner: key_at(d, OWNER_OFF),
            delegate: key_at(d, 204),
            feed: key_at(d, 236),
            fee_account: key_at(d, 268),
            observer: key_at(d, 300),
            oi_cap_bps: u64_at(d, 332),
            max_gap_bps: u64_at(d, 340),
            paid_max_gap_bps: u64_at(d, 348),
            window_secs: u64_at(d, 356),
            backing_fee_cap_bps: u16_at(d, 364),
            last_observed: u64_at(d, 368) as i64,
            snap_slot: u64_at(d, 376),
            snap_index: u16_at(d, 384),
            snap_pool_ok: d[386] == 1,
            snap_sampled: d[387] == 1,
            snap_trader: key_at(d, 392),
            snap_position_q: u128_at(d, 424) as i128,
            snap_oi_long_q: u128_at(d, 440),
            snap_oi_short_q: u128_at(d, 456),
            snap_spot: u64_at(d, 472),
            snap_high: u64_at(d, 480),
            snap_low: u64_at(d, 488),
            snap_base_reserve: u64_at(d, 496),
            snap_contract_base_atoms: u64_at(d, 504),
            snap_fee_paid: u64_at(d, 512),
            buckets,
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
            (OWNER_OFF, &self.house_owner),
            (204, &self.delegate),
            (236, &self.feed),
            (268, &self.fee_account),
            (300, &self.observer),
            (392, &self.snap_trader),
        ] {
            d[o..o + 32].copy_from_slice(k.as_ref());
        }
        for (o, v) in [
            (332, self.oi_cap_bps),
            (340, self.max_gap_bps),
            (348, self.paid_max_gap_bps),
            (356, self.window_secs),
            (368, self.last_observed as u64),
            (376, self.snap_slot),
            (472, self.snap_spot),
            (480, self.snap_high),
            (488, self.snap_low),
            (496, self.snap_base_reserve),
            (504, self.snap_contract_base_atoms),
            (512, self.snap_fee_paid),
        ] {
            d[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        d[364..366].copy_from_slice(&self.backing_fee_cap_bps.to_le_bytes());
        d[384..386].copy_from_slice(&self.snap_index.to_le_bytes());
        d[386] = self.snap_pool_ok as u8;
        d[387] = self.snap_sampled as u8;
        d[424..440].copy_from_slice(&self.snap_position_q.to_le_bytes());
        d[440..456].copy_from_slice(&self.snap_oi_long_q.to_le_bytes());
        d[456..472].copy_from_slice(&self.snap_oi_short_q.to_le_bytes());
        for (k, b) in self.buckets.iter().enumerate() {
            let o = BUCKETS_OFF + k * BUCKET_LEN;
            d[o..o + 8].copy_from_slice(&(b.start as u64).to_le_bytes());
            d[o + 8..o + 16].copy_from_slice(&b.high.to_le_bytes());
            d[o + 16..o + 24].copy_from_slice(&b.low.to_le_bytes());
        }
    }

    pub fn params(&self) -> Params {
        Params { oi_cap_bps: self.oi_cap_bps, max_gap_bps: self.max_gap_bps, paid_max_gap_bps: self.paid_max_gap_bps }
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            position_q: self.snap_position_q,
            oi_long_q: self.snap_oi_long_q,
            oi_short_q: self.snap_oi_short_q,
            pool: self.snap_pool_ok.then_some(Pool {
                base_reserve: self.snap_base_reserve,
                spot: self.snap_spot,
                high: self.snap_high,
                low: self.snap_low,
            }),
            contract_base_atoms: self.snap_contract_base_atoms,
            fee_paid: self.snap_fee_paid,
            sampled: self.snap_sampled,
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
    /// No keeper sample in the window: nothing opens.
    Unsampled,
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
            Refusal::Unsampled => MatchError::Unsampled,
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

/// The house's settings, as read from an instruction.
struct Settings {
    oi_cap_bps: u64,
    max_gap_bps: u64,
    paid_max_gap_bps: u64,
    window_secs: u64,
    backing_fee_cap_bps: u16,
}

fn read_settings(d: &mut &[u8]) -> Result<Settings, ProgramError> {
    let s = Settings {
        oi_cap_bps: read_u64(d)?,
        max_gap_bps: read_u64(d)?,
        paid_max_gap_bps: read_u64(d)?,
        window_secs: read_u64(d)?,
        backing_fee_cap_bps: read_u16(d)?,
    };
    if s.oi_cap_bps == 0
        || s.oi_cap_bps > 100_000
        || s.max_gap_bps > 10_000
        || s.paid_max_gap_bps > 10_000
        || s.window_secs < BUCKETS as u64
        || s.window_secs > 3_600
        || s.backing_fee_cap_bps > 10_000
    {
        return Err(MatchError::BadParams.into());
    }
    Ok(s)
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

fn check_fee_account(ai: &AccountInfo) -> ProgramResult {
    if *ai.owner != bust_mark::TOKEN && *ai.owner != bust_mark::TOKEN_2022 {
        return Err(MatchError::WrongAccount.into());
    }
    Ok(())
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
        TAG_OBSERVE => observe(program_id, accounts),
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
    let fee_ai = next_account_info(it)?;
    let observer_ai = next_account_info(it)?;
    let system_ai = next_account_info(it)?;
    let asset_index = read_u16(d)?;
    let s = read_settings(d)?;
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
    check_fee_account(fee_ai)?;

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
        fee_account: *fee_ai.key,
        observer: *observer_ai.key,
        oi_cap_bps: s.oi_cap_bps,
        max_gap_bps: s.max_gap_bps,
        paid_max_gap_bps: s.paid_max_gap_bps,
        window_secs: s.window_secs,
        backing_fee_cap_bps: s.backing_fee_cap_bps,
        last_observed: 0,
        snap_slot: 0,
        snap_index: 0,
        snap_pool_ok: false,
        snap_sampled: false,
        snap_trader: Pubkey::default(),
        snap_position_q: 0,
        snap_oi_long_q: 0,
        snap_oi_short_q: 0,
        snap_spot: 0,
        snap_high: 0,
        snap_low: 0,
        snap_base_reserve: 0,
        snap_contract_base_atoms: 0,
        snap_fee_paid: 0,
        buckets: [Bucket::default(); BUCKETS],
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
    let fee_ai = next_account_info(it)?;
    let observer_ai = next_account_info(it)?;
    let s = read_settings(d)?;
    let mut ctx = load_context(program_id, ctx_ai)?;
    if !owner_ai.is_signer || *owner_ai.key != ctx.house_owner {
        return Err(MatchError::WrongAccount.into());
    }
    check_feed(&ctx.market, &ctx.percolator, ctx.asset_index, feed_ai)?;
    check_fee_account(fee_ai)?;
    ctx.feed = *feed_ai.key;
    ctx.fee_account = *fee_ai.key;
    ctx.observer = *observer_ai.key;
    ctx.oi_cap_bps = s.oi_cap_bps;
    ctx.max_gap_bps = s.max_gap_bps;
    ctx.paid_max_gap_bps = s.paid_max_gap_bps;
    if ctx.window_secs != s.window_secs {
        // the buckets were spans of the old window
        ctx.buckets = [Bucket::default(); BUCKETS];
        ctx.last_observed = 0;
    }
    ctx.window_secs = s.window_secs;
    ctx.backing_fee_cap_bps = s.backing_fee_cap_bps;
    ctx.armed = false;
    ctx.store(&mut ctx_ai.try_borrow_mut_data()?);
    Ok(())
}

/// The pool as read now, through the feed's own reader, or None when it cannot be read (closed for swaps, empty, a
/// stale quote price). Wrong accounts are an error: they are the caller's mistake, not the pool's state.
fn read_pool(feed: &Feed, it: &mut core::slice::Iter<AccountInfo>, now: i64) -> Result<Option<(u64, u64)>, ProgramError> {
    let pool_ai = next_account_info(it)?;
    let base_ai = next_account_info(it)?;
    let quote_ai = next_account_info(it)?;
    let price_ai = next_account_info(it).ok();
    match bust_mark::pool_reading(feed, pool_ai, base_ai, quote_ai, price_ai, now) {
        Ok(reading) => Ok(Some(reading)),
        Err(e) if e == ProgramError::Custom(bust_mark::MarkError::WrongAccount as u32) => Err(MatchError::WrongAccount.into()),
        Err(e) => {
            msg!("pool unreadable: {}", e);
            Ok(None)
        }
    }
}

/// The keeper samples the pool into the context's buckets, in a transaction of its own.
fn observe(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let observer_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let mut ctx = load_context(program_id, ctx_ai)?;
    if !observer_ai.is_signer || *observer_ai.key != ctx.observer || *feed_ai.key != ctx.feed {
        return Err(MatchError::WrongAccount.into());
    }
    let feed = check_feed(&ctx.market, &ctx.percolator, ctx.asset_index, feed_ai)?;
    let now = Clock::get()?.unix_timestamp;
    if let Some((_, spot)) = read_pool(&feed, it, now)? {
        rules::record(&mut ctx.buckets, ctx.window_secs, now, spot);
        ctx.last_observed = now;
        ctx.store(&mut ctx_ai.try_borrow_mut_data()?);
        msg!("sampled {}", spot);
    }
    Ok(())
}

/// What the instruction right before `here` paid to the fee account: a token transfer to it, or nothing.
fn fee_paid(sysvar_ai: &AccountInfo, here: u16, fee_account: &Pubkey) -> Result<u64, ProgramError> {
    let Some(prev) = here.checked_sub(1) else { return Ok(0) };
    let ix = ixs::load_instruction_at_checked(prev as usize, sysvar_ai)?;
    if ix.program_id != bust_mark::TOKEN && ix.program_id != bust_mark::TOKEN_2022 {
        return Ok(0);
    }
    let destination = match ix.data.first() {
        Some(&TOKEN_TRANSFER) if ix.data.len() == 9 => ix.accounts.get(1),
        Some(&TOKEN_TRANSFER_CHECKED) if ix.data.len() == 10 => ix.accounts.get(2),
        _ => None,
    };
    Ok(match destination {
        Some(m) if m.pubkey == *fee_account => u64::from_le_bytes(ix.data[1..9].try_into().unwrap()),
        _ => 0,
    })
}

/// Reads what the trade will meet and leaves it in the context for `Match`. Only a binding error fails it: a pool
/// that cannot be read is recorded as such, so the trader can still close.
fn prepare(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let ctx_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let trader_ai = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let mut ctx = load_context(program_id, ctx_ai)?;
    // the sysvar sits between the pool's accounts and the optional price account
    let sysvar_ai = accounts.get(7).ok_or(ProgramError::NotEnoughAccountKeys)?;
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
    // pool, base vault, quote vault, then the sysvar, then the price account
    let pool_accounts: Vec<AccountInfo> = accounts.get(4..7).ok_or(ProgramError::NotEnoughAccountKeys)?.iter().chain(accounts.get(8)).cloned().collect();
    let pool = read_pool(&feed, &mut pool_accounts.iter(), clock.unix_timestamp)?;

    ctx.armed = true;
    ctx.snap_slot = clock.slot;
    ctx.snap_index = here + 1;
    ctx.snap_trader = *trader_ai.key;
    ctx.snap_position_q = position_q;
    ctx.snap_oi_long_q = asset.oi_long_q;
    ctx.snap_oi_short_q = asset.oi_short_q;
    ctx.snap_pool_ok = pool.is_some();
    ctx.snap_fee_paid = fee_paid(sysvar_ai, here, &ctx.fee_account)?;
    ctx.snap_contract_base_atoms = feed.contract_base_atoms;
    // the trader's own reading widens the gap but is not kept: only the keeper's samples are
    ctx.snap_sampled = clock.unix_timestamp.saturating_sub(ctx.last_observed) <= ctx.window_secs as i64;
    if let Some((base, spot)) = pool {
        (ctx.snap_high, ctx.snap_low) = rules::range(&ctx.buckets, ctx.window_secs, clock.unix_timestamp, spot);
        (ctx.snap_base_reserve, ctx.snap_spot) = (base, spot);
    } else {
        (ctx.snap_base_reserve, ctx.snap_spot, ctx.snap_high, ctx.snap_low) = (0, 0, 0, 0);
    }
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
            "refused: {:?}; request {} position {} mark {} pool {} high {} low {} ({}, {}) paid {} oi {} / {}",
            r,
            req_q,
            ctx.snap_position_q,
            mark,
            ctx.snap_spot,
            ctx.snap_high,
            ctx.snap_low,
            if ctx.snap_pool_ok { "read" } else { "unreadable" },
            if ctx.snap_sampled { "sampled" } else { "unsampled" },
            ctx.snap_fee_paid,
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
    msg!("filled {} of {} at {} paid {}", filled, req_q, mark, ctx.snap_fee_paid);
    Ok(())
}

/// Closes a context of any layout: every layout keeps the house owner at the same offset.
fn close(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let owner_ai = next_account_info(it)?;
    let ctx_ai = next_account_info(it)?;
    if ctx_ai.owner != program_id || !ctx_ai.is_writable {
        return Err(MatchError::WrongAccount.into());
    }
    {
        let d = ctx_ai.try_borrow_data()?;
        if d.len() < OWNER_OFF + 32 || d[64..71] != MAGIC[..7] || !owner_ai.is_signer || key_at(&d, OWNER_OFF) != *owner_ai.key {
            return Err(MatchError::WrongAccount.into());
        }
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
        let mut buckets = [Bucket::default(); BUCKETS];
        buckets[2] = Bucket { start: 1_800_000_020, high: 4_100_000, low: 3_900_000 };
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
            fee_account: Pubkey::new_unique(),
            observer: Pubkey::new_unique(),
            oi_cap_bps: 500,
            max_gap_bps: 50,
            paid_max_gap_bps: 2_000,
            window_secs: 60,
            backing_fee_cap_bps: 7,
            last_observed: 1_800_000_031,
            snap_slot: 123_456,
            snap_index: 4,
            snap_pool_ok: true,
            snap_sampled: true,
            snap_trader: Pubkey::new_unique(),
            snap_position_q: -42_000_000,
            snap_oi_long_q: 1 << 100,
            snap_oi_short_q: 99,
            snap_spot: 4_000_000,
            snap_high: 4_100_000,
            snap_low: 3_900_000,
            snap_base_reserve: 500_000_000_000_000,
            snap_contract_base_atoms: 1_000_000_000_000,
            snap_fee_paid: 12_345,
            buckets,
        };
        let mut d = vec![0u8; CTX_LEN];
        ctx.store(&mut d);
        assert_eq!(Context::load(&d).unwrap(), ctx);
        // the return prefix is left alone, and the owner sits where every layout keeps it
        assert!(d[..RETURN_LEN].iter().all(|&b| b == 0));
        assert_eq!(key_at(&d, OWNER_OFF), ctx.house_owner);
    }
}
