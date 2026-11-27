//! What the house fills, in plain integers so it can be tested off chain.
//!
//! A request first closes what it can of the trader's position: that part is always filled, so nobody is ever kept
//! in a position. The rest opens new risk and is filled only while
//! - the pool can be read (open for swaps, a fresh quote price),
//! - the pool and the mark agree within `max_gap_bps` on the side that would gain as the mark catches up to the pool
//!   (Percolator charges an unsigned LP only the market's base fee, so this gap cannot be priced: it is refused
//!   instead, and a wider entry goes through the house's own signed path with a fee), and
//! - open interest on that side stays within `oi_cap_bps` of the pool's depth: its coin reserve valued at the lower of
//!   the mark and the pool price, which pushing the pool either way can only shrink in one of its two terms.
//! What fits under the cap is filled and the rest is left, as a partial fill.

/// Positions are in 1e-6 contract.
pub const POS_SCALE: u128 = 1_000_000;

/// The pool as read in the same transaction: its coin reserve in base atoms and its price in collateral atoms per
/// contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    pub base_reserve: u64,
    pub spot: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The trader's position, positive long.
    pub position_q: i128,
    pub oi_long_q: u128,
    pub oi_short_q: u128,
    /// None when the pool cannot be read: closing still works, opening does not.
    pub pool: Option<Pool>,
    pub contract_base_atoms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub oi_cap_bps: u64,
    pub max_gap_bps: u64,
}

/// Why nothing was filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadRequest,
    PoolUnreadable,
    Gap,
    OiCap,
}

/// `a * b / c` rounded down, without overflowing when `a * b` would (`b` and `c` are prices or basis points).
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    (a / c).saturating_mul(b).saturating_add((a % c) * b / c)
}

/// The pool's price against the mark in basis points, and whether a position opened on `long`'s side gains as the
/// mark moves toward the pool.
pub fn gap(spot: u64, mark: u64, long: bool) -> (u64, bool) {
    let bps = (spot.abs_diff(mark) as u128 * 10_000 / mark as u128) as u64;
    (bps, if long { spot > mark } else { spot < mark })
}

/// The open interest a side may reach, in position units.
pub fn oi_cap_q(pool: Pool, mark: u64, contract_base_atoms: u64, oi_cap_bps: u64) -> u128 {
    // the reserve in contracts (1e-6), at the lower of the two prices, as a share of the reserve's value at the mark
    let reserve_q = mul_div(pool.base_reserve as u128, POS_SCALE, contract_base_atoms as u128);
    let at_lower = mul_div(reserve_q, pool.spot.min(mark) as u128, mark as u128);
    mul_div(at_lower, oi_cap_bps as u128, 10_000)
}

/// The signed size to fill for a request of `req_q` at `mark`, or why nothing is filled.
pub fn fill(snap: &Snapshot, params: &Params, mark: u64, req_q: i128) -> Result<i128, Refusal> {
    if req_q == 0 || req_q == i128::MIN || mark == 0 || snap.contract_base_atoms == 0 {
        return Err(Refusal::BadRequest);
    }
    let long = req_q > 0;
    let want = req_q.unsigned_abs();
    let closing = if snap.position_q != 0 && (snap.position_q > 0) != long { want.min(snap.position_q.unsigned_abs()) } else { 0 };
    let opening = want - closing;

    let mut opened = 0u128;
    let mut refusal = None;
    if opening > 0 {
        match snap.pool {
            None => refusal = Some(Refusal::PoolUnreadable),
            Some(pool) => {
                let (gap_bps, gains) = gap(pool.spot, mark, long);
                if gains && gap_bps > params.max_gap_bps {
                    refusal = Some(Refusal::Gap);
                } else {
                    let side = if long { snap.oi_long_q } else { snap.oi_short_q };
                    let room = oi_cap_q(pool, mark, snap.contract_base_atoms, params.oi_cap_bps).saturating_sub(side);
                    opened = opening.min(room);
                    if opened == 0 {
                        refusal = Some(Refusal::OiCap);
                    }
                }
            }
        }
    }
    let filled = closing + opened;
    if filled == 0 {
        return Err(refusal.unwrap_or(Refusal::BadRequest));
    }
    // filled is at most |req_q|, which fits
    let filled = filled as i128;
    Ok(if long { filled } else { -filled })
}

/// The house's own quote, for a trade it signs (`TradeNoCpi`): an open on the gaining side past the matcher's gap
/// limit, with the gap as its fee, so the trader enters as if at the pool's price. Everything else goes through the
/// matcher, at no fee of the house's: a request that closes any part of the position is refused here, as is one the
/// matcher would fill whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaidEntry {
    /// The signed size the house signs for: what fits under the open interest cap.
    pub size_q: i128,
    /// The fee the trade carries: the market's base fee plus the gap.
    pub fee_bps: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaidRefusal {
    /// The request closes some of the position: close through the matcher first.
    Closes,
    /// The matcher fills this request whole, at no fee of the house's.
    MatcherFills,
    PoolUnreadable,
    /// The gap is past what the house sells: `paid_max_gap_bps`, or the market's fee ceiling less its base fee.
    GapTooWide,
    OiCap,
}

/// The house's quote for a paid entry; `base_fee_bps` and `max_fee_bps` are the market's base fee and ceiling.
pub fn paid_entry(snap: &Snapshot, params: &Params, paid_max_gap_bps: u64, base_fee_bps: u64, max_fee_bps: u64, mark: u64, req_q: i128) -> Result<PaidEntry, PaidRefusal> {
    if req_q == 0 || req_q == i128::MIN || mark == 0 || snap.contract_base_atoms == 0 {
        return Err(PaidRefusal::MatcherFills);
    }
    let long = req_q > 0;
    if snap.position_q != 0 && (snap.position_q > 0) != long {
        return Err(PaidRefusal::Closes);
    }
    let pool = snap.pool.ok_or(PaidRefusal::PoolUnreadable)?;
    let (gap_bps, gains) = gap(pool.spot, mark, long);
    if !gains || gap_bps <= params.max_gap_bps {
        return Err(PaidRefusal::MatcherFills);
    }
    let fee_bps = base_fee_bps.saturating_add(gap_bps);
    if gap_bps > paid_max_gap_bps || fee_bps > max_fee_bps {
        return Err(PaidRefusal::GapTooWide);
    }
    let side = if long { snap.oi_long_q } else { snap.oi_short_q };
    let room = oi_cap_q(pool, mark, snap.contract_base_atoms, params.oi_cap_bps).saturating_sub(side);
    let filled = req_q.unsigned_abs().min(room);
    if filled == 0 {
        return Err(PaidRefusal::OiCap);
    }
    let filled = filled as i128;
    Ok(PaidEntry { size_q: if long { filled } else { -filled }, fee_bps })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paid_entry_carries_the_gap_as_its_fee() {
        // the pool 0.5% above the mark, a matcher that fills no gap at all
        let pumped = Some(Pool { spot: 4_020_000, ..POOL });
        let free = Params { max_gap_bps: 0, ..PARAMS };
        assert_eq!(fill(&snap(0, pumped), &free, MARK, POS_SCALE as i128), Err(Refusal::Gap));
        assert_eq!(
            paid_entry(&snap(0, pumped), &free, 2_000, 10, 100, MARK, POS_SCALE as i128),
            Ok(PaidEntry { size_q: POS_SCALE as i128, fee_bps: 60 })
        );
        // adding to a long is an entry too
        assert!(paid_entry(&snap(POS_SCALE as i128, pumped), &free, 2_000, 10, 100, MARK, POS_SCALE as i128).is_ok());
    }

    #[test]
    fn the_house_sells_only_what_the_matcher_refuses() {
        let pumped = Some(Pool { spot: 4_020_000, ..POOL });
        let free = Params { max_gap_bps: 0, ..PARAMS };
        // the losing side, or a gap within the matcher's limit: the matcher's, at no fee
        assert_eq!(paid_entry(&snap(0, pumped), &free, 2_000, 10, 100, MARK, -(POS_SCALE as i128)), Err(PaidRefusal::MatcherFills));
        assert_eq!(paid_entry(&snap(0, pumped), &PARAMS, 2_000, 10, 100, MARK, POS_SCALE as i128), Err(PaidRefusal::MatcherFills));
        // a request that closes: through the matcher first
        assert_eq!(paid_entry(&snap(-(POS_SCALE as i128), pumped), &free, 2_000, 10, 100, MARK, 2 * POS_SCALE as i128), Err(PaidRefusal::Closes));
    }

    #[test]
    fn a_paid_entry_stops_at_the_houses_limit_the_markets_ceiling_and_the_cap() {
        let free = Params { max_gap_bps: 0, ..PARAMS };
        let far = Some(Pool { spot: 4_400_000, ..POOL }); // 10%
        assert_eq!(paid_entry(&snap(0, far), &free, 500, 0, 10_000, MARK, POS_SCALE as i128), Err(PaidRefusal::GapTooWide));
        assert_eq!(paid_entry(&snap(0, far), &free, 2_000, 10, 1_000, MARK, POS_SCALE as i128), Err(PaidRefusal::GapTooWide));
        assert!(paid_entry(&snap(0, far), &free, 2_000, 10, 1_010, MARK, POS_SCALE as i128).is_ok());
        let mut s = snap(0, far);
        s.oi_long_q = 20 * POS_SCALE;
        assert_eq!(paid_entry(&s, &free, 2_000, 0, 10_000, MARK, 10 * POS_SCALE as i128).unwrap().size_q, 5 * POS_SCALE as i128);
        assert_eq!(paid_entry(&snap(0, None), &free, 2_000, 0, 10_000, MARK, POS_SCALE as i128), Err(PaidRefusal::PoolUnreadable));
    }

    /// 1M coins (6 decimals) a contract; a pool of 500M coins at $4 a contract.
    const CONTRACT: u64 = 1_000_000_000_000;
    const MARK: u64 = 4_000_000;
    const POOL: Pool = Pool { base_reserve: 500_000_000_000_000, spot: 4_000_000 };
    const PARAMS: Params = Params { oi_cap_bps: 500, max_gap_bps: 50 };

    fn snap(position_q: i128, pool: Option<Pool>) -> Snapshot {
        Snapshot { position_q, oi_long_q: 0, oi_short_q: 0, pool, contract_base_atoms: CONTRACT }
    }

    #[test]
    fn the_cap_is_a_share_of_the_pools_coin_reserve() {
        // 500 contracts in the pool, 5% of it: 25 contracts
        assert_eq!(oi_cap_q(POOL, MARK, CONTRACT, 500), 25 * POS_SCALE);
        // pumped to $5: the mark is lower, the reserve counts at the mark
        assert_eq!(oi_cap_q(Pool { spot: 5_000_000, ..POOL }, MARK, CONTRACT, 500), 25 * POS_SCALE);
        // dumped to $2: the reserve counts at $2 against a $4 mark, half
        assert_eq!(oi_cap_q(Pool { spot: 2_000_000, ..POOL }, MARK, CONTRACT, 500), 12_500_000);
    }

    #[test]
    fn an_open_within_the_rules_is_filled_whole() {
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, 10 * POS_SCALE as i128), Ok(10 * POS_SCALE as i128));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, -10 * POS_SCALE as i128), Ok(-10 * POS_SCALE as i128));
    }

    #[test]
    fn an_open_past_the_cap_is_filled_up_to_it() {
        let mut s = snap(0, Some(POOL));
        s.oi_long_q = 20 * POS_SCALE;
        assert_eq!(fill(&s, &PARAMS, MARK, 10 * POS_SCALE as i128), Ok(5 * POS_SCALE as i128));
        s.oi_long_q = 25 * POS_SCALE;
        assert_eq!(fill(&s, &PARAMS, MARK, 10 * POS_SCALE as i128), Err(Refusal::OiCap));
        // the other side has its own room
        assert_eq!(fill(&s, &PARAMS, MARK, -10 * POS_SCALE as i128), Ok(-10 * POS_SCALE as i128));
    }

    #[test]
    fn the_gaining_side_is_refused_past_the_gap_and_the_other_side_is_not() {
        // the pool 1% above the mark: longs gain as the mark catches up
        let pumped = Some(Pool { spot: 4_040_000, ..POOL });
        assert_eq!(fill(&snap(0, pumped), &PARAMS, MARK, POS_SCALE as i128), Err(Refusal::Gap));
        assert_eq!(fill(&snap(0, pumped), &PARAMS, MARK, -(POS_SCALE as i128)), Ok(-(POS_SCALE as i128)));
        // within the gap, both open
        let near = Some(Pool { spot: 4_020_000, ..POOL });
        assert_eq!(fill(&snap(0, near), &PARAMS, MARK, POS_SCALE as i128), Ok(POS_SCALE as i128));
        // the pool 1% below the mark: shorts gain
        let dumped = Some(Pool { spot: 3_960_000, ..POOL });
        assert_eq!(fill(&snap(0, dumped), &PARAMS, MARK, -(POS_SCALE as i128)), Err(Refusal::Gap));
    }

    #[test]
    fn closing_is_always_filled() {
        let pumped = Pool { spot: 8_000_000, ..POOL };
        for pool in [None, Some(pumped)] {
            let mut s = snap(-10 * POS_SCALE as i128, pool);
            s.oi_long_q = u128::MAX; // no room at all
            // a short buys back in a pump, with the pool unreadable, past the cap: all of it
            assert_eq!(fill(&s, &PARAMS, MARK, 10 * POS_SCALE as i128), Ok(10 * POS_SCALE as i128));
            assert_eq!(fill(&s, &PARAMS, MARK, 4 * POS_SCALE as i128), Ok(4 * POS_SCALE as i128));
        }
    }

    #[test]
    fn a_close_that_flips_fills_the_close_and_opens_by_the_rules() {
        // short 10, buys 15 in a pump: the 10 close, the 5 long would gain from the gap
        let pumped = Some(Pool { spot: 4_400_000, ..POOL });
        assert_eq!(fill(&snap(-10 * POS_SCALE as i128, pumped), &PARAMS, MARK, 15 * POS_SCALE as i128), Ok(10 * POS_SCALE as i128));
        // with the pool at the mark, all 15
        assert_eq!(fill(&snap(-10 * POS_SCALE as i128, Some(POOL)), &PARAMS, MARK, 15 * POS_SCALE as i128), Ok(15 * POS_SCALE as i128));
    }

    #[test]
    fn adding_to_a_position_is_an_open() {
        let pumped = Some(Pool { spot: 4_400_000, ..POOL });
        assert_eq!(fill(&snap(10 * POS_SCALE as i128, pumped), &PARAMS, MARK, POS_SCALE as i128), Err(Refusal::Gap));
    }

    #[test]
    fn an_unreadable_pool_refuses_opens() {
        assert_eq!(fill(&snap(0, None), &PARAMS, MARK, POS_SCALE as i128), Err(Refusal::PoolUnreadable));
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, 0), Err(Refusal::BadRequest));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, i128::MIN), Err(Refusal::BadRequest));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, 0, 1), Err(Refusal::BadRequest));
    }

    #[test]
    fn no_overflow_at_the_extremes() {
        let huge = Pool { base_reserve: u64::MAX, spot: u64::MAX };
        let _ = oi_cap_q(huge, u64::MAX, 1, u64::MAX);
        let _ = oi_cap_q(huge, 1, 1, u64::MAX);
        let _ = gap(u64::MAX, 1, true);
    }
}
