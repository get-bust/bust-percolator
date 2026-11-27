//! The few Percolator v16 fields the matcher reads, at fixed offsets.
//!
//! Percolator's own readers would pull its whole program into this one, so the offsets are written out here and
//! `tests/layout.rs` holds them to Percolator's types (`offset_of!` at the pinned engine revision) and to its own
//! readers on real devnet accounts. Every field read is a byte-array pod, so the layout has no padding.
//!
//! Market account: a 16-byte header (magic u64, version u16, kind u8), the wrapper config, the market group header,
//! then one slot per asset: the wrapper's oracle storage (512 bytes) and the engine's asset slot, whose first field is
//! the asset state.
//!
//! Portfolio account: the same header, then the engine portfolio with its owner and a fixed array of 16 legs.

pub const MAGIC: u64 = 0x5045_5243_5631_3600; // "PERCV16\0"
pub const VERSION: u16 = 16;
pub const KIND_MARKET: u8 = 1;
pub const KIND_PORTFOLIO: u8 = 2;
const HEADER_LEN: usize = 16;

const MARKET_GROUP_OFF: usize = HEADER_LEN + 448;
const GROUP_HEADER_LEN: usize = 710;
const MAX_MARKET_SLOTS_OFF: usize = MARKET_GROUP_OFF + 32 + 2;
const SLOT_CAPACITY_OFF: usize = MARKET_GROUP_OFF + 265;
const SLOT_LEN: usize = 1909;
const ASSET_IN_SLOT: usize = 512;

// asset state fields
const MARKET_ID: usize = 0;
const EFFECTIVE_PRICE: usize = 25;
const A_LONG: usize = 49;
const A_SHORT: usize = 65;
const OI_LONG: usize = 289;
const OI_SHORT: usize = 305;
const EPOCH_LONG: usize = 529;
const EPOCH_SHORT: usize = 537;
const KF_EPOCH_LONG: usize = 145;
const KF_EPOCH_SHORT: usize = 153;

const PORTFOLIO_LEN: usize = 9435;
/// Where a portfolio names its market: the first field of its provenance header.
pub const PORTFOLIO_MARKET_OFF: usize = HEADER_LEN;
const OWNER_OFF: usize = HEADER_LEN + 100;
const LEGS_OFF: usize = HEADER_LEN + 340;
const LEG_LEN: usize = 152;
pub const LEGS: usize = 16;
// leg fields
const LEG_ACTIVE: usize = 0;
const LEG_ASSET: usize = 1;
const LEG_MARKET_ID: usize = 5;
const LEG_SIDE: usize = 13;
const LEG_BASIS: usize = 14;
const LEG_A_BASIS: usize = 30;
const LEG_KF_EPOCH: usize = 78;
const LEG_EPOCH: usize = 86;

pub const ADL_ONE: u128 = 1_000_000_000_000_000;
pub const MIN_A_SIDE: u128 = 100_000_000_000_000;

fn u16_at(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes(d[o..o + 2].try_into().unwrap())
}
fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u128_at(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}
fn i128_at(d: &[u8], o: usize) -> i128 {
    i128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

fn header_is(d: &[u8], kind: u8) -> bool {
    d.len() >= HEADER_LEN && u64_at(d, 0) == MAGIC && u16_at(d, 8) == VERSION && d[10] == kind
}

/// What the matcher needs of one asset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    pub market_id: u64,
    pub effective_price: u64,
    pub a_long: u128,
    pub a_short: u128,
    pub oi_long_q: u128,
    pub oi_short_q: u128,
    pub epoch_long: u64,
    pub epoch_short: u64,
    /// The slot of each side's last K/F change: a leg settled before it is stale.
    pub kf_epoch_long: u64,
    pub kf_epoch_short: u64,
}

/// The asset at `asset_index` of a market account, or None for an account that is not a market or has no such slot.
pub fn asset(market: &[u8], asset_index: u16) -> Option<Asset> {
    if !header_is(market, KIND_MARKET) || market.len() < MARKET_GROUP_OFF + GROUP_HEADER_LEN {
        return None;
    }
    let i = asset_index as usize;
    if i >= u32_at(market, MAX_MARKET_SLOTS_OFF) as usize || i >= u32_at(market, SLOT_CAPACITY_OFF) as usize {
        return None;
    }
    let o = MARKET_GROUP_OFF + GROUP_HEADER_LEN + i * SLOT_LEN + ASSET_IN_SLOT;
    if market.len() < MARKET_GROUP_OFF + GROUP_HEADER_LEN + (i + 1) * SLOT_LEN {
        return None;
    }
    Some(Asset {
        market_id: u64_at(market, o + MARKET_ID),
        effective_price: u64_at(market, o + EFFECTIVE_PRICE),
        a_long: u128_at(market, o + A_LONG),
        a_short: u128_at(market, o + A_SHORT),
        oi_long_q: u128_at(market, o + OI_LONG),
        oi_short_q: u128_at(market, o + OI_SHORT),
        epoch_long: u64_at(market, o + EPOCH_LONG),
        epoch_short: u64_at(market, o + EPOCH_SHORT),
        kf_epoch_long: u64_at(market, o + KF_EPOCH_LONG),
        kf_epoch_short: u64_at(market, o + KF_EPOCH_SHORT),
    })
}

/// A portfolio account's owner, or None for an account that is not a portfolio.
pub fn portfolio_owner(portfolio: &[u8]) -> Option<[u8; 32]> {
    if !header_is(portfolio, KIND_PORTFOLIO) || portfolio.len() < PORTFOLIO_LEN {
        return None;
    }
    Some(portfolio[OWNER_OFF..OWNER_OFF + 32].try_into().unwrap())
}

/// Whether the portfolio holds a leg of the asset of this generation: such a portfolio has to be cranked after the
/// asset's mark moves before anyone can add risk to the asset.
pub fn holds_leg(portfolio: &[u8], asset_index: u16, market_id: u64) -> bool {
    portfolio_owner(portfolio).is_some()
        && (0..LEGS).any(|k| {
            let o = LEGS_OFF + k * LEG_LEN;
            portfolio[o + LEG_ACTIVE] == 1 && u32_at(portfolio, o + LEG_ASSET) == asset_index as u32 && u64_at(portfolio, o + LEG_MARKET_ID) == market_id
        })
}

/// Whether the portfolio holds a leg of the asset settled before its side's last K/F change: Percolator counts such a
/// leg stale and refuses any trade that adds risk on the asset until a crank of the portfolio settles it.
pub fn leg_stale(portfolio: &[u8], asset_index: u16, asset: &Asset) -> bool {
    portfolio_owner(portfolio).is_some()
        && (0..LEGS).any(|k| {
            let o = LEGS_OFF + k * LEG_LEN;
            if portfolio[o + LEG_ACTIVE] != 1 || u32_at(portfolio, o + LEG_ASSET) != asset_index as u32 || u64_at(portfolio, o + LEG_MARKET_ID) != asset.market_id {
                return false;
            }
            let side_epoch = if portfolio[o + LEG_SIDE] == 0 { asset.kf_epoch_long } else { asset.kf_epoch_short };
            u64_at(portfolio, o + LEG_KF_EPOCH) < side_epoch
        })
}

/// The portfolio's signed position in the asset (positive long) as the engine counts it when it trades: the raw
/// basis scaled by the side's ADL index since the leg was opened, rounded up (`effective_abs_quantity_for_leg`). A leg
/// from before a side reset holds nothing. None for an account that is not a portfolio or a leg the engine would
/// refuse as invalid.
pub fn position(portfolio: &[u8], asset_index: u16, asset: &Asset) -> Option<i128> {
    portfolio_owner(portfolio)?;
    for k in 0..LEGS {
        let o = LEGS_OFF + k * LEG_LEN;
        if portfolio[o + LEG_ACTIVE] != 1
            || u32_at(portfolio, o + LEG_ASSET) != asset_index as u32
            || u64_at(portfolio, o + LEG_MARKET_ID) != asset.market_id
        {
            continue;
        }
        let long = match portfolio[o + LEG_SIDE] {
            0 => true,
            1 => false,
            _ => return None,
        };
        let (current_a, epoch) = if long { (asset.a_long, asset.epoch_long) } else { (asset.a_short, asset.epoch_short) };
        if u64_at(portfolio, o + LEG_EPOCH) != epoch {
            return Some(0);
        }
        let raw = i128_at(portfolio, o + LEG_BASIS).unsigned_abs();
        let a_basis = u128_at(portfolio, o + LEG_A_BASIS);
        if !(MIN_A_SIDE..=ADL_ONE).contains(&a_basis) || !(1..=a_basis).contains(&current_a) {
            return None;
        }
        let q = i128::try_from(raw.checked_mul(current_a)?.div_ceil(a_basis)).ok()?;
        return Some(if long { q } else { -q });
    }
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};
    use percolator::{AssetStateV16Account, EngineAssetSlotV16Account, Market, MarketGroupV16HeaderAccount, PortfolioAccountV16Account, PortfolioLegV16Account, V16ConfigAccount};
    use percolator_prog::{constants, state};

    const MARKET: &[u8] = include_bytes!("../tests/fixtures/market.bin");
    const TRADER: &[u8] = include_bytes!("../tests/fixtures/trader.bin");
    const HOUSE: &[u8] = include_bytes!("../tests/fixtures/house.bin");

    #[test]
    fn the_offsets_are_percolators() {
        assert_eq!(MAGIC, constants::MAGIC);
        assert_eq!(VERSION, constants::VERSION);
        assert_eq!(MARKET_GROUP_OFF, constants::MARKET_GROUP_OFF);
        assert_eq!(GROUP_HEADER_LEN, size_of::<MarketGroupV16HeaderAccount>());
        assert_eq!(MAX_MARKET_SLOTS_OFF, MARKET_GROUP_OFF + offset_of!(MarketGroupV16HeaderAccount, config) + offset_of!(V16ConfigAccount, max_market_slots));
        assert_eq!(SLOT_CAPACITY_OFF, MARKET_GROUP_OFF + offset_of!(MarketGroupV16HeaderAccount, asset_slot_capacity));
        assert_eq!(SLOT_LEN, constants::MARKET_ASSET_SLOT_LEN);
        type Slot = Market<[u8; constants::ASSET_ORACLE_WRAPPER_LEN]>;
        assert_eq!(ASSET_IN_SLOT, offset_of!(Slot, engine) + offset_of!(EngineAssetSlotV16Account, asset));
        for (ours, theirs) in [
            (MARKET_ID, offset_of!(AssetStateV16Account, market_id)),
            (EFFECTIVE_PRICE, offset_of!(AssetStateV16Account, effective_price)),
            (A_LONG, offset_of!(AssetStateV16Account, a_long)),
            (A_SHORT, offset_of!(AssetStateV16Account, a_short)),
            (OI_LONG, offset_of!(AssetStateV16Account, oi_eff_long_q)),
            (OI_SHORT, offset_of!(AssetStateV16Account, oi_eff_short_q)),
            (EPOCH_LONG, offset_of!(AssetStateV16Account, epoch_long)),
            (EPOCH_SHORT, offset_of!(AssetStateV16Account, epoch_short)),
            (KF_EPOCH_LONG, offset_of!(AssetStateV16Account, kf_epoch_long)),
            (KF_EPOCH_SHORT, offset_of!(AssetStateV16Account, kf_epoch_short)),
            (OWNER_OFF, HEADER_LEN + offset_of!(PortfolioAccountV16Account, owner)),
            (LEGS_OFF, HEADER_LEN + offset_of!(PortfolioAccountV16Account, legs)),
            (LEG_LEN, size_of::<PortfolioLegV16Account>()),
            (LEG_ACTIVE, offset_of!(PortfolioLegV16Account, active)),
            (LEG_ASSET, offset_of!(PortfolioLegV16Account, asset_index)),
            (LEG_MARKET_ID, offset_of!(PortfolioLegV16Account, market_id)),
            (LEG_SIDE, offset_of!(PortfolioLegV16Account, side)),
            (LEG_BASIS, offset_of!(PortfolioLegV16Account, basis_pos_q)),
            (LEG_A_BASIS, offset_of!(PortfolioLegV16Account, a_basis)),
            (LEG_EPOCH, offset_of!(PortfolioLegV16Account, epoch_snap)),
            (LEG_KF_EPOCH, offset_of!(PortfolioLegV16Account, kf_epoch_snap)),
            (PORTFOLIO_LEN, constants::PORTFOLIO_ENGINE_ACCOUNT_LEN),
            (
                PORTFOLIO_MARKET_OFF,
                HEADER_LEN + offset_of!(PortfolioAccountV16Account, provenance_header) + offset_of!(percolator::ProvenanceHeaderV16Account, market_group_id),
            ),
        ] {
            assert_eq!(ours, theirs);
        }
        assert_eq!(LEGS, percolator::V16_MAX_PORTFOLIO_ASSETS_N);
        assert_eq!((ADL_ONE, MIN_A_SIDE), (percolator::ADL_ONE, percolator::MIN_A_SIDE));
    }

    #[test]
    fn a_devnet_market_reads_as_percolator_reads_it() {
        let (_, group) = state::read_market(MARKET).unwrap();
        let theirs = &group.assets[0];
        let ours = asset(MARKET, 0).unwrap();
        assert_eq!(ours.market_id, theirs.market_id);
        assert_eq!(ours.effective_price, theirs.effective_price);
        assert_eq!((ours.a_long, ours.a_short), (theirs.a_long, theirs.a_short));
        assert_eq!((ours.oi_long_q, ours.oi_short_q), (theirs.oi_eff_long_q, theirs.oi_eff_short_q));
        assert_eq!((ours.epoch_long, ours.epoch_short), (theirs.epoch_long, theirs.epoch_short));
        assert_eq!((ours.kf_epoch_long, ours.kf_epoch_short), (theirs.kf_epoch_long, theirs.kf_epoch_short));
        assert!(asset(MARKET, 1).is_none(), "one slot");
        assert!(asset(TRADER, 0).is_none(), "a portfolio is not a market");
    }

    #[test]
    fn devnet_portfolios_read_as_percolator_reads_them() {
        let a = asset(MARKET, 0).unwrap();
        for data in [TRADER, HOUSE] {
            let theirs = state::read_portfolio(data).unwrap();
            assert_eq!(portfolio_owner(data).unwrap(), theirs.owner);
            let leg = theirs.legs.iter().find(|l| l.active == 1 && l.asset_index.get() == 0 && l.market_id.get() == a.market_id);
            let raw = leg.map_or(0, |l| {
                let q = l.basis_pos_q.get().unsigned_abs() as i128;
                if l.side == 0 { q } else { -q }
            });
            // no ADL has run on devnet: the effective position is the raw basis
            assert_eq!(position(data, 0, &a).unwrap(), raw);
            println!("position {raw}");
        }
        assert!(position(MARKET, 0, &a).is_none(), "a market is not a portfolio");
    }

    /// A portfolio holding one leg of asset 0.
    fn with_leg(a: &Asset, long: bool, basis: i128, a_basis: u128, epoch: u64) -> Vec<u8> {
        let mut d = TRADER.to_vec();
        for k in 0..LEGS {
            d[LEGS_OFF + k * LEG_LEN + LEG_ACTIVE] = 0;
        }
        let o = LEGS_OFF;
        d[o + LEG_ACTIVE] = 1;
        d[o + LEG_ASSET..o + LEG_ASSET + 4].copy_from_slice(&0u32.to_le_bytes());
        d[o + LEG_MARKET_ID..o + LEG_MARKET_ID + 8].copy_from_slice(&a.market_id.to_le_bytes());
        d[o + LEG_SIDE] = if long { 0 } else { 1 };
        d[o + LEG_BASIS..o + LEG_BASIS + 16].copy_from_slice(&basis.to_le_bytes());
        d[o + LEG_A_BASIS..o + LEG_A_BASIS + 16].copy_from_slice(&a_basis.to_le_bytes());
        d[o + LEG_EPOCH..o + LEG_EPOCH + 8].copy_from_slice(&epoch.to_le_bytes());
        d
    }

    #[test]
    fn a_portfolio_names_its_market_and_its_legs() {
        let (_, group) = state::read_market(MARKET).unwrap();
        let a = asset(MARKET, 0).unwrap();
        let theirs = state::read_portfolio(TRADER).unwrap();
        assert_eq!(TRADER[PORTFOLIO_MARKET_OFF..PORTFOLIO_MARKET_OFF + 32], theirs.provenance_header.market_group_id);
        assert_eq!(group.market_group_id, theirs.provenance_header.market_group_id);
        assert!(!holds_leg(TRADER, 0, a.market_id), "the devnet trader held nothing when it was read");
        let d = with_leg(&a, true, 5_000_000, ADL_ONE, a.epoch_long);
        assert!(holds_leg(&d, 0, a.market_id));
        assert!(!holds_leg(&d, 0, a.market_id + 1));
        assert!(!holds_leg(MARKET, 0, a.market_id));
    }

    #[test]
    fn a_leg_settled_before_its_sides_last_kf_change_is_stale() {
        let mut a = asset(MARKET, 0).unwrap();
        a.kf_epoch_long = 1_000;
        let mut d = with_leg(&a, true, 5_000_000, ADL_ONE, a.epoch_long);
        let o = LEGS_OFF;
        d[o + LEG_KF_EPOCH..o + LEG_KF_EPOCH + 8].copy_from_slice(&999u64.to_le_bytes());
        assert!(leg_stale(&d, 0, &a));
        d[o + LEG_KF_EPOCH..o + LEG_KF_EPOCH + 8].copy_from_slice(&1_000u64.to_le_bytes());
        assert!(!leg_stale(&d, 0, &a));
        // the short side's epoch does not bear on a long leg
        a.kf_epoch_short = 5_000;
        assert!(!leg_stale(&d, 0, &a));
    }

    #[test]
    fn adl_scales_the_position_down_rounding_up() {
        let mut a = asset(MARKET, 0).unwrap();
        a.a_long = ADL_ONE / 3;
        let d = with_leg(&a, true, 1_000_001, ADL_ONE, a.epoch_long);
        // ceil(1_000_001 / 3)
        assert_eq!(position(&d, 0, &a), Some(333_334));
        a.a_short = ADL_ONE / 2;
        let d = with_leg(&a, false, -1_000_001, ADL_ONE, a.epoch_short);
        assert_eq!(position(&d, 0, &a), Some(-500_001));
    }

    #[test]
    fn a_leg_from_before_a_side_reset_holds_nothing() {
        let a = asset(MARKET, 0).unwrap();
        let d = with_leg(&a, true, 5_000_000, ADL_ONE, a.epoch_long.wrapping_sub(1));
        assert_eq!(position(&d, 0, &a), Some(0));
    }

    #[test]
    fn a_leg_of_another_generation_is_not_counted() {
        let mut a = asset(MARKET, 0).unwrap();
        let d = with_leg(&a, true, 5_000_000, ADL_ONE, a.epoch_long);
        a.market_id += 1;
        assert_eq!(position(&d, 0, &a), Some(0));
    }
}
