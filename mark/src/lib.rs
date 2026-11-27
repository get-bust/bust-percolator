//! Bust mark: marks one Percolator asset to the time-weighted average of one pool, read on chain: a PumpSwap pool, or
//! a Raydium CPMM pool (where coins from Raydium's launchpad go) or a Raydium AMM v4 pool (where older Pump coins
//! went). `pools` has their layouts.
//!
//! A pool quoted in the collateral is read as it is. A pool quoted in another token (SOL on mainnet) is priced in
//! dollars at each sample with that token's Pyth price (a `PriceUpdateV2` account with full verification), so the
//! average is of the dollar price; a stale or wide Pyth price refuses the update.
//!
//! A feed account per asset holds the pool's two vaults and a ring of observations. Anyone may call `Update` once
//! `min_interval_secs` have passed since the last one: the program reads both vaults itself, adds an observation, and
//! pushes the average over `window_secs` to Percolator as the asset's authority mark (`PushAuthMark`), signed by the
//! feed's oracle PDA. Whoever calls it chooses only when, never the price. The pool is chosen when the feed is made,
//! by its admin, and the vaults are read from the pool account so they cannot be swapped for others.
//!
//! The oracle PDA becomes the asset's oracle authority through `TakeOracle`, signed by the authority it replaces.
//! `HandOracle` gives the role to another key, signed by the feed's admin and that key, so a feed can be retired.
//!
//! Instructions (first byte the tag, then little-endian fields):
//!   0 Init         asset_index u16, contract_base_atoms u64, window_secs u64, min_interval_secs u64, max_step_bps u64,
//!                  quote_feed_id 32 bytes (zero: quoted in the collateral), max_quote_age_secs u64,
//!                  max_quote_conf_bps u64, quote_decimals u8, quote_mint 32 bytes (the pool's other token must be it)
//!                  [admin (signer, writable), feed (writable), pool, market, percolator, system program,
//!                   base vault, quote vault, quote price account (when quote_feed_id is set)]
//!   1 Update       market_id u64, observation_sequence u64, authority_epoch u64
//!                  [feed (writable), pool, base vault, quote vault, market (writable), oracle PDA, percolator,
//!                   quote price account (when the feed has one)]
//!   2 TakeOracle   market_id u64, authority_epoch u64
//!                  [current oracle authority (signer), feed, oracle PDA, market (writable), percolator]
//!   3 HandOracle   market_id u64, authority_epoch u64, new_authority 32 bytes
//!                  [admin (signer), feed, oracle PDA, new authority (signer), market (writable), percolator]
//!   4 SetParams    window_secs u64, min_interval_secs u64, max_step_bps u64
//!                  [admin (signer), feed (writable)]
//!   5 Close        [admin (signer, writable), feed (writable)]: the rent back to the admin. The oracle PDA keeps its
//!                  role, and a feed opened again at the same address has the same PDA.

pub mod pools;
pub mod twap;

use solana_program::{
    account_info::{next_account_info, AccountInfo},
    clock::Clock,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction, system_program,
    sysvar::Sysvar,
};
use twap::Obs;

solana_program::declare_id!("DGCkC4YboRrAxFKY5piEgCE9yjT1tCxwzKeMqZ7NCBfo");

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process);

pub const TOKEN: Pubkey = solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const PYTH_RECEIVER: Pubkey = solana_program::pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
const PYTH_PRICE_UPDATE_V2: [u8; 8] = [0x22, 0xf1, 0x23, 0x63, 0x9d, 0x7e, 0xf4, 0xcd];
pub const TOKEN_2022: Pubkey = solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Percolator v16 instruction tags and the oracle role's kind in `UpdateAssetAuthority`.
const PUSH_AUTH_MARK: u8 = 63;
const UPDATE_ASSET_AUTHORITY: u8 = 65;
const ASSET_AUTH_ORACLE: u8 = 4;

const MAGIC: &[u8; 8] = b"bustmrk3";
/// Observations kept. The window must fit in this many intervals, so the ring always reaches back to its start.
pub const RING: usize = 256;
const OBS_LEN: usize = 24;
const HEADER_LEN: usize = 352;
pub const FEED_LEN: usize = HEADER_LEN + RING * OBS_LEN;

/// The feed's fixed fields. Offsets are those of `load` and `store`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    pub admin: Pubkey,
    pub percolator: Pubkey,
    pub market: Pubkey,
    pub pool: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub asset_index: u16,
    pub pool_kind: u8,
    pub bump: u8,
    pub oracle_bump: u8,
    pub contract_base_atoms: u64,
    pub window_secs: u64,
    pub min_interval_secs: u64,
    pub max_step_bps: u64,
    /// The price read at the newest observation; it holds until the next one.
    pub last_price: u64,
    /// Index of the newest observation, and how many are stored.
    pub head: u16,
    pub len: u16,
    /// The quote token's Pyth price account and feed id; the default key when the pool is quoted in the collateral.
    pub quote_feed: Pubkey,
    pub quote_feed_id: [u8; 32],
    pub max_quote_age_secs: u64,
    pub max_quote_conf_bps: u64,
    pub quote_decimals: u8,
    /// Which of a CPMM pool's two tokens is the coin; 0 for PumpSwap.
    pub base_side: u8,
}

fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}

impl Feed {
    pub fn load(d: &[u8]) -> Result<Self, ProgramError> {
        if d.len() != FEED_LEN || &d[..8] != MAGIC {
            return Err(ProgramError::InvalidAccountData);
        }
        Ok(Self {
            admin: key_at(d, 8),
            percolator: key_at(d, 40),
            market: key_at(d, 72),
            pool: key_at(d, 104),
            base_vault: key_at(d, 136),
            quote_vault: key_at(d, 168),
            asset_index: u16_at(d, 200),
            pool_kind: d[202],
            bump: d[203],
            oracle_bump: d[204],
            contract_base_atoms: u64_at(d, 208),
            window_secs: u64_at(d, 216),
            min_interval_secs: u64_at(d, 224),
            max_step_bps: u64_at(d, 232),
            last_price: u64_at(d, 248),
            head: u16_at(d, 256),
            len: u16_at(d, 258),
            quote_feed: key_at(d, 264),
            quote_feed_id: d[296..328].try_into().unwrap(),
            max_quote_age_secs: u64_at(d, 328),
            max_quote_conf_bps: u64_at(d, 336),
            quote_decimals: d[344],
            base_side: d[345],
        })
    }

    pub fn store(&self, d: &mut [u8]) {
        d[..8].copy_from_slice(MAGIC);
        for (o, k) in [(8, &self.admin), (40, &self.percolator), (72, &self.market), (104, &self.pool), (136, &self.base_vault), (168, &self.quote_vault)] {
            d[o..o + 32].copy_from_slice(k.as_ref());
        }
        d[200..202].copy_from_slice(&self.asset_index.to_le_bytes());
        d[202] = self.pool_kind;
        d[203] = self.bump;
        d[204] = self.oracle_bump;
        for (o, v) in [(208, self.contract_base_atoms), (216, self.window_secs), (224, self.min_interval_secs), (232, self.max_step_bps), (248, self.last_price)] {
            d[o..o + 8].copy_from_slice(&v.to_le_bytes());
        }
        d[256..258].copy_from_slice(&self.head.to_le_bytes());
        d[258..260].copy_from_slice(&self.len.to_le_bytes());
        d[264..296].copy_from_slice(self.quote_feed.as_ref());
        d[296..328].copy_from_slice(&self.quote_feed_id);
        d[328..336].copy_from_slice(&self.max_quote_age_secs.to_le_bytes());
        d[336..344].copy_from_slice(&self.max_quote_conf_bps.to_le_bytes());
        d[344] = self.quote_decimals;
        d[345] = self.base_side;
    }
}

pub fn obs_at(d: &[u8], i: usize) -> Obs {
    let o = HEADER_LEN + i * OBS_LEN;
    Obs { ts: i64::from_le_bytes(d[o..o + 8].try_into().unwrap()), cum: u128::from_le_bytes(d[o + 8..o + 24].try_into().unwrap()) }
}

fn set_obs(d: &mut [u8], i: usize, obs: Obs) {
    let o = HEADER_LEN + i * OBS_LEN;
    d[o..o + 8].copy_from_slice(&obs.ts.to_le_bytes());
    d[o + 8..o + 24].copy_from_slice(&obs.cum.to_le_bytes());
}

/// The stored observations, oldest first.
pub fn observations(d: &[u8], feed: &Feed) -> Vec<Obs> {
    let len = feed.len as usize;
    (0..len).map(|k| obs_at(d, (feed.head as usize + RING + 1 - len + k) % RING)).collect()
}

/// Appends an observation, overwriting the oldest once the ring is full.
fn push_obs(d: &mut [u8], feed: &mut Feed, obs: Obs) {
    feed.head = ((feed.head as usize + 1) % RING) as u16;
    feed.len = (feed.len as usize + 1).min(RING) as u16;
    set_obs(d, feed.head as usize, obs);
}

pub fn feed_address(market: &Pubkey, asset_index: u16) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"feed", market.as_ref(), &asset_index.to_le_bytes()], &ID)
}

pub fn oracle_address(feed: &Pubkey) -> (Pubkey, u8) {
    Pubkey::find_program_address(&[b"oracle", feed.as_ref()], &ID)
}

#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum MarkError {
    UnknownPool = 1,
    WrongAccount,
    TooSoon,
    BadParams,
    EmptyPool,
    ShortHistory,
    QuotePrice,
}

impl From<MarkError> for ProgramError {
    fn from(e: MarkError) -> Self {
        ProgramError::Custom(e as u32)
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

fn read_key(d: &mut &[u8]) -> Result<Pubkey, ProgramError> {
    let (v, rest) = d.split_first_chunk::<32>().ok_or(ProgramError::InvalidInstructionData)?;
    *d = rest;
    Ok(Pubkey::new_from_array(*v))
}

/// The window must be covered by the ring at the shortest interval between updates.
fn check_params(window: u64, min_interval: u64, max_step_bps: u64) -> ProgramResult {
    if window == 0 || min_interval == 0 || window > min_interval * (RING as u64 - 2) || max_step_bps > 10_000 || window > i64::MAX as u64 {
        return Err(MarkError::BadParams.into());
    }
    Ok(())
}

/// A token account's amount, from an account owned by either token program.
fn token_amount(ai: &AccountInfo) -> Result<u64, ProgramError> {
    if *ai.owner != TOKEN && *ai.owner != TOKEN_2022 {
        return Err(MarkError::WrongAccount.into());
    }
    let d = ai.try_borrow_data()?;
    if d.len() < 72 {
        return Err(MarkError::WrongAccount.into());
    }
    Ok(u64_at(&d, 64))
}

/// The quote token's dollar price from a Pyth `PriceUpdateV2` account, as (price, expo): full verification, the
/// feed's id, no older than `max_age` seconds and a confidence within `max_conf_bps` of the price.
fn quote_price(feed: &Feed, ai: &AccountInfo, now: i64) -> Result<(u64, i32), ProgramError> {
    if *ai.key != feed.quote_feed || *ai.owner != PYTH_RECEIVER {
        return Err(MarkError::WrongAccount.into());
    }
    let d = ai.try_borrow_data()?;
    // discriminator, write authority, verification level (1: full), then the price feed message
    if d.len() < 134 || d[..8] != PYTH_PRICE_UPDATE_V2 || d[40] != 1 || d[41..73] != feed.quote_feed_id {
        return Err(MarkError::QuotePrice.into());
    }
    let price = i64::from_le_bytes(d[73..81].try_into().unwrap());
    let conf = u64_at(&d, 81);
    let expo = i32::from_le_bytes(d[89..93].try_into().unwrap());
    let publish_time = i64::from_le_bytes(d[93..101].try_into().unwrap());
    let price = u64::try_from(price).ok().filter(|&p| p > 0).ok_or(MarkError::QuotePrice)?;
    if now.saturating_sub(publish_time) > feed.max_quote_age_secs as i64
        || conf as u128 * 10_000 > price as u128 * feed.max_quote_conf_bps as u128
        || !(-18..=0).contains(&expo)
    {
        return Err(MarkError::QuotePrice.into());
    }
    Ok((price, expo))
}

/// The pool's base reserve in base atoms and its price in collateral atoms per contract; `price_ai` is the quote
/// price account when the feed has one. The matcher reads the pool through this too, so both see the same price.
pub fn pool_reading(feed: &Feed, pool_ai: &AccountInfo, base_ai: &AccountInfo, quote_ai: &AccountInfo, price_ai: Option<&AccountInfo>, now: i64) -> Result<(u64, u64), ProgramError> {
    if *pool_ai.key != feed.pool || *base_ai.key != feed.base_vault || *quote_ai.key != feed.quote_vault {
        return Err(MarkError::WrongAccount.into());
    }
    let (base, quote) = pools::reserves(feed.pool_kind, &pool_ai.try_borrow_data()?, feed.base_side, token_amount(base_ai)?, token_amount(quote_ai)?, now)
        .ok_or(MarkError::EmptyPool)?;
    let spot = if feed.quote_feed == Pubkey::default() {
        twap::spot(base, quote, feed.contract_base_atoms)
    } else {
        let (price, expo) = quote_price(feed, price_ai.ok_or(MarkError::QuotePrice)?, now)?;
        twap::spot_in_collateral(base, quote, feed.contract_base_atoms, price, expo, feed.quote_decimals)
    };
    Ok((base, spot.ok_or(MarkError::EmptyPool)?))
}

/// The pool's price in collateral atoms per contract.
fn pool_spot(feed: &Feed, pool_ai: &AccountInfo, base_ai: &AccountInfo, quote_ai: &AccountInfo, price_ai: Option<&AccountInfo>, now: i64) -> Result<u64, ProgramError> {
    pool_reading(feed, pool_ai, base_ai, quote_ai, price_ai, now).map(|(_, spot)| spot)
}

pub fn process(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (&tag, mut rest) = data.split_first().ok_or(ProgramError::InvalidInstructionData)?;
    match tag {
        0 => init(program_id, accounts, &mut rest),
        1 => update(program_id, accounts, &mut rest),
        2 => take_oracle(program_id, accounts, &mut rest),
        3 => hand_oracle(program_id, accounts, &mut rest),
        4 => set_params(program_id, accounts, &mut rest),
        5 => close(program_id, accounts),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn init(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let pool_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let percolator_ai = next_account_info(it)?;
    let system_ai = next_account_info(it)?;
    let asset_index = read_u16(d)?;
    let contract_base_atoms = read_u64(d)?;
    let (window_secs, min_interval_secs, max_step_bps) = (read_u64(d)?, read_u64(d)?, read_u64(d)?);
    let quote_feed_id: [u8; 32] = *d.split_first_chunk::<32>().ok_or(ProgramError::InvalidInstructionData)?.0;
    *d = &d[32..];
    let (max_quote_age_secs, max_quote_conf_bps) = (read_u64(d)?, read_u64(d)?);
    let quote_decimals = *d.first().ok_or(ProgramError::InvalidInstructionData)?;
    *d = &d[1..];
    let quote_mint = read_key(d)?;
    check_params(window_secs, min_interval_secs, max_step_bps)?;
    if !admin.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *system_ai.key != system_program::ID || *market_ai.owner != *percolator_ai.key || !percolator_ai.executable || contract_base_atoms == 0 {
        return Err(MarkError::WrongAccount.into());
    }
    let (address, bump) = feed_address(market_ai.key, asset_index);
    if *feed_ai.key != address {
        return Err(MarkError::WrongAccount.into());
    }
    // the vaults are read from the pool, so they are the pool's own
    let pool_kind = pools::kind_of(pool_ai.owner).ok_or(MarkError::UnknownPool)?;
    let (base_vault, quote_vault, base_side) = pools::vaults(pool_kind, &pool_ai.try_borrow_data()?, &quote_mint).ok_or(MarkError::UnknownPool)?;
    let base_ai = next_account_info(it)?;
    let quote_ai = next_account_info(it)?;
    let price_ai = if quote_feed_id == [0u8; 32] { None } else { Some(next_account_info(it)?) };

    let lamports = Rent::get()?.minimum_balance(FEED_LEN);
    let seeds: &[&[u8]] = &[b"feed", market_ai.key.as_ref(), &asset_index.to_le_bytes(), &[bump]];
    invoke_signed(
        &system_instruction::create_account(admin.key, feed_ai.key, lamports, FEED_LEN as u64, program_id),
        &[admin.clone(), feed_ai.clone(), system_ai.clone()],
        &[seeds],
    )?;
    let mut feed = Feed {
        admin: *admin.key,
        percolator: *percolator_ai.key,
        market: *market_ai.key,
        pool: *pool_ai.key,
        base_vault,
        quote_vault,
        asset_index,
        pool_kind,
        bump,
        oracle_bump: oracle_address(feed_ai.key).1,
        contract_base_atoms,
        window_secs,
        min_interval_secs,
        max_step_bps,
        last_price: 0,
        head: RING as u16 - 1,
        len: 0,
        quote_feed: price_ai.map(|a| *a.key).unwrap_or_default(),
        quote_feed_id,
        max_quote_age_secs,
        max_quote_conf_bps,
        quote_decimals,
        base_side,
    };
    let now = Clock::get()?.unix_timestamp;
    let spot = pool_spot(&feed, pool_ai, base_ai, quote_ai, price_ai, now)?;
    // Before the feed existed the price is taken to have been the first one read, for a whole window, so the
    // average starts at the pool's price rather than at the few seconds the feed has seen.
    let mut d = feed_ai.try_borrow_mut_data()?;
    push_obs(&mut d, &mut feed, Obs { ts: now - window_secs as i64, cum: 0 });
    push_obs(&mut d, &mut feed, Obs { ts: now, cum: spot as u128 * window_secs as u128 });
    feed.last_price = spot;
    feed.store(&mut d);
    msg!("feed for asset {} at {}", asset_index, spot);
    Ok(())
}

fn load_feed(program_id: &Pubkey, ai: &AccountInfo) -> Result<Feed, ProgramError> {
    if ai.owner != program_id {
        return Err(MarkError::WrongAccount.into());
    }
    Feed::load(&ai.try_borrow_data()?)
}

fn check_market(feed: &Feed, market_ai: &AccountInfo, percolator_ai: &AccountInfo, oracle_ai: &AccountInfo, feed_key: &Pubkey) -> ProgramResult {
    let oracle = Pubkey::create_program_address(&[b"oracle", feed_key.as_ref(), &[feed.oracle_bump]], &ID)?;
    if *market_ai.key != feed.market || *percolator_ai.key != feed.percolator || *oracle_ai.key != oracle {
        return Err(MarkError::WrongAccount.into());
    }
    Ok(())
}

fn update(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let feed_ai = next_account_info(it)?;
    let pool_ai = next_account_info(it)?;
    let base_ai = next_account_info(it)?;
    let quote_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let oracle_ai = next_account_info(it)?;
    let percolator_ai = next_account_info(it)?;
    let price_ai = next_account_info(it).ok();
    let (market_id, observation_sequence, authority_epoch) = (read_u64(d)?, read_u64(d)?, read_u64(d)?);
    let mut feed = load_feed(program_id, feed_ai)?;
    check_market(&feed, market_ai, percolator_ai, oracle_ai, feed_ai.key)?;

    let clock = Clock::get()?;
    let mark = {
        let mut d = feed_ai.try_borrow_mut_data()?;
        let last = obs_at(&d, feed.head as usize);
        if clock.unix_timestamp < last.ts + feed.min_interval_secs as i64 {
            return Err(MarkError::TooSoon.into());
        }
        let cum = twap::cum_at_now(last, feed.last_price, clock.unix_timestamp).ok_or(ProgramError::ArithmeticOverflow)?;
        push_obs(&mut d, &mut feed, Obs { ts: clock.unix_timestamp, cum });
        let mark = twap::average(&observations(&d, &feed), feed.window_secs as i64).ok_or(MarkError::ShortHistory)?;
        let spot = pool_spot(&feed, pool_ai, base_ai, quote_ai, price_ai, clock.unix_timestamp)?;
        feed.last_price = twap::clamp_step(spot, feed.last_price, feed.max_step_bps);
        feed.store(&mut d);
        msg!("spot {} held {} mark {}", spot, feed.last_price, mark);
        mark
    };

    let mut data = Vec::with_capacity(43);
    data.push(PUSH_AUTH_MARK);
    data.extend_from_slice(&feed.asset_index.to_le_bytes());
    for v in [market_id, clock.slot, mark, observation_sequence, authority_epoch] {
        data.extend_from_slice(&v.to_le_bytes());
    }
    let ix = Instruction {
        program_id: feed.percolator,
        accounts: vec![AccountMeta::new_readonly(*oracle_ai.key, true), AccountMeta::new(*market_ai.key, false)],
        data,
    };
    invoke_signed(&ix, &[oracle_ai.clone(), market_ai.clone(), percolator_ai.clone()], &[&[b"oracle", feed_ai.key.as_ref(), &[feed.oracle_bump]]])
}

fn update_authority_ix(feed: &Feed, current: &Pubkey, new: &Pubkey, market_id: u64, authority_epoch: u64) -> Instruction {
    let mut data = Vec::with_capacity(52);
    data.push(UPDATE_ASSET_AUTHORITY);
    data.extend_from_slice(&feed.asset_index.to_le_bytes());
    data.extend_from_slice(&market_id.to_le_bytes());
    data.extend_from_slice(&authority_epoch.to_le_bytes());
    data.push(ASSET_AUTH_ORACLE);
    data.extend_from_slice(new.as_ref());
    Instruction {
        program_id: feed.percolator,
        accounts: vec![AccountMeta::new_readonly(*current, true), AccountMeta::new_readonly(*new, true), AccountMeta::new(feed.market, false)],
        data,
    }
}

fn take_oracle(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let current = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let oracle_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let percolator_ai = next_account_info(it)?;
    let (market_id, authority_epoch) = (read_u64(d)?, read_u64(d)?);
    let feed = load_feed(program_id, feed_ai)?;
    check_market(&feed, market_ai, percolator_ai, oracle_ai, feed_ai.key)?;
    // Percolator checks that `current` is the oracle authority and has signed; the PDA signs as the incoming one.
    invoke_signed(
        &update_authority_ix(&feed, current.key, oracle_ai.key, market_id, authority_epoch),
        &[current.clone(), oracle_ai.clone(), market_ai.clone(), percolator_ai.clone()],
        &[&[b"oracle", feed_ai.key.as_ref(), &[feed.oracle_bump]]],
    )
}

fn hand_oracle(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let oracle_ai = next_account_info(it)?;
    let new_ai = next_account_info(it)?;
    let market_ai = next_account_info(it)?;
    let percolator_ai = next_account_info(it)?;
    let (market_id, authority_epoch, new_key) = (read_u64(d)?, read_u64(d)?, read_key(d)?);
    let feed = load_feed(program_id, feed_ai)?;
    check_market(&feed, market_ai, percolator_ai, oracle_ai, feed_ai.key)?;
    if !admin.is_signer || *admin.key != feed.admin {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if *new_ai.key != new_key {
        return Err(MarkError::WrongAccount.into());
    }
    invoke_signed(
        &update_authority_ix(&feed, oracle_ai.key, new_ai.key, market_id, authority_epoch),
        &[oracle_ai.clone(), new_ai.clone(), market_ai.clone(), percolator_ai.clone()],
        &[&[b"oracle", feed_ai.key.as_ref(), &[feed.oracle_bump]]],
    )
}

fn set_params(program_id: &Pubkey, accounts: &[AccountInfo], d: &mut &[u8]) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    let (window_secs, min_interval_secs, max_step_bps) = (read_u64(d)?, read_u64(d)?, read_u64(d)?);
    check_params(window_secs, min_interval_secs, max_step_bps)?;
    let mut feed = load_feed(program_id, feed_ai)?;
    if !admin.is_signer || *admin.key != feed.admin {
        return Err(ProgramError::MissingRequiredSignature);
    }
    // A longer window needs history the ring may not hold yet; the next update refuses until it does.
    feed.window_secs = window_secs;
    feed.min_interval_secs = min_interval_secs;
    feed.max_step_bps = max_step_bps;
    feed.store(&mut feed_ai.try_borrow_mut_data()?);
    Ok(())
}

fn close(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let it = &mut accounts.iter();
    let admin = next_account_info(it)?;
    let feed_ai = next_account_info(it)?;
    if feed_ai.owner != program_id {
        return Err(MarkError::WrongAccount.into());
    }
    // the admin is at the same offset in every layout, so a feed of an older layout can be closed too
    if !admin.is_signer || key_at(&feed_ai.try_borrow_data()?, 8) != *admin.key {
        return Err(ProgramError::MissingRequiredSignature);
    }
    **admin.try_borrow_mut_lamports()? += feed_ai.lamports();
    **feed_ai.try_borrow_mut_lamports()? = 0;
    feed_ai.try_borrow_mut_data()?.fill(0);
    feed_ai.assign(&solana_program::system_program::ID);
    feed_ai.realloc(0, false)
}
