//! What the house fills, in plain integers so it can be tested off chain.
//!
//! A request first closes what it can of the trader's position: that part is always filled, so nobody is ever kept
//! in a position. The rest opens new risk and is filled only while
//! - the pool can be read (open for swaps, a fresh quote price);
//! - open interest on that side stays within `oi_cap_bps` of the pool's depth: its coin reserve valued at the lower of
//!   the mark and the pool price, which pushing the pool either way can only shrink in one of its two terms; and
//! - on the side that would gain as the mark catches up to the pool, the gap is within `max_gap_bps`, or the trader
//!   has paid it. Percolator charges an unsigned house only the market's base fee, so the gap is paid as a token
//!   transfer to the house's fee account, the instruction right before the matcher's `Prepare` in the same
//!   transaction: it buys an entry as if at the pool's price, up to `paid_max_gap_bps`.
//!
//! The gap is not read from the pool's price in the trade's own transaction alone, which the trader could move there
//! and move back. It is the widest gap over the pool prices sampled in the last `window` seconds, in transactions of
//! of the keeper's own, and the price now: each of `BUCKETS` buckets keeps the highest and lowest
//! price it saw in its span of `window / BUCKETS` seconds, and adding prices cannot lower a highest or raise a lowest,
//! so hiding a move means faking every sample in the window. A new bucket takes the slot of the oldest, so the window
//! holds the last 50 to 60 seconds of a 60-second setting. What fits under the cap, and under the fee paid, is filled and the rest is left, as a partial fill.

/// Positions are in 1e-6 contract.
pub const POS_SCALE: u128 = 1_000_000;
/// Sample buckets in a window.
pub const BUCKETS: usize = 6;

/// The pool as read in the same transaction (its coin reserve in base atoms, its price in collateral atoms per
/// contract), with the highest and lowest prices sampled in the window, the price now included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pool {
    pub base_reserve: u64,
    pub spot: u64,
    pub high: u64,
    pub low: u64,
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
    /// Collateral atoms the transaction paid the house for the gap.
    pub fee_paid: u64,
    /// Whether the keeper sampled the pool within the window: without a sample of its own the gap could only be read
    /// in the trade's transaction, so nothing opens.
    pub sampled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    pub oi_cap_bps: u64,
    /// The gap the house leaves unpaid: the market's base fee covers it.
    pub max_gap_bps: u64,
    /// The widest gap the house sells.
    pub paid_max_gap_bps: u64,
}

/// Why nothing was filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    BadRequest,
    PoolUnreadable,
    Unsampled,
    Gap,
    OiCap,
}

/// One sample bucket: the start of its time span and the highest and lowest pool price seen in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    pub start: i64,
    pub high: u64,
    pub low: u64,
}

/// Adds a price seen at `now` to its bucket, starting the bucket afresh when its span has moved on. A bucket spans
/// `window / BUCKETS` seconds.
pub fn record(buckets: &mut [Bucket; BUCKETS], window: u64, now: i64, spot: u64) {
    let span = (window / BUCKETS as u64).max(1) as i64;
    let start = now.div_euclid(span) * span;
    let b = &mut buckets[(now.div_euclid(span)).rem_euclid(BUCKETS as i64) as usize];
    if b.start != start || b.high == 0 {
        *b = Bucket { start, high: spot, low: spot };
    } else {
        b.high = b.high.max(spot);
        b.low = b.low.min(spot);
    }
}

/// The highest and lowest prices of the buckets that overlap the last `window` seconds, and `spot`.
pub fn range(buckets: &[Bucket; BUCKETS], window: u64, now: i64, spot: u64) -> (u64, u64) {
    let span = (window / BUCKETS as u64).max(1) as i64;
    let oldest = now.saturating_sub(window as i64);
    buckets
        .iter()
        .filter(|b| b.high != 0 && b.start + span > oldest && b.start <= now)
        .fold((spot, spot), |(hi, lo), b| (hi.max(b.high), lo.min(b.low)))
}

/// `a * b / c` rounded down, without overflowing when `a * b` would (`b` and `c` are prices or basis points).
fn mul_div(a: u128, b: u128, c: u128) -> u128 {
    (a / c).saturating_mul(b).saturating_add((a % c) * b / c)
}

/// The gap a position opened on `long`'s side would gain as the mark catches up, in collateral atoms per contract
/// and in basis points of the mark: zero when that side would not gain.
pub fn gap(pool: &Pool, mark: u64, long: bool) -> (u64, u64) {
    let atoms = if long { pool.high.saturating_sub(mark) } else { mark.saturating_sub(pool.low) };
    (atoms, (atoms as u128 * 10_000 / mark as u128) as u64)
}

/// The fee, in collateral atoms, that pays a gap of `gap_atoms` a contract on `q` position units, rounded up.
pub fn fee_for(q: u128, gap_atoms: u64) -> u64 {
    q.checked_mul(gap_atoms as u128).map_or(u64::MAX, |v| u64::try_from(v.div_ceil(POS_SCALE)).unwrap_or(u64::MAX))
}

/// The open interest a side may reach, in position units.
pub fn oi_cap_q(pool: &Pool, mark: u64, contract_base_atoms: u64, oi_cap_bps: u64) -> u128 {
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
            Some(_) if !snap.sampled => refusal = Some(Refusal::Unsampled),
            Some(pool) => {
                let side = if long { snap.oi_long_q } else { snap.oi_short_q };
                let room = oi_cap_q(&pool, mark, snap.contract_base_atoms, params.oi_cap_bps).saturating_sub(side);
                let (gap_atoms, gap_bps) = gap(&pool, mark, long);
                // what the gap lets open: all of it within the free gap, what the fee pays for past it
                let paid_for = if gap_bps <= params.max_gap_bps {
                    u128::MAX
                } else if gap_bps <= params.paid_max_gap_bps {
                    snap.fee_paid as u128 * POS_SCALE / gap_atoms as u128
                } else {
                    0
                };
                opened = opening.min(room).min(paid_for);
                if opened == 0 {
                    refusal = Some(if room == 0 { Refusal::OiCap } else { Refusal::Gap });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 1M coins (6 decimals) a contract; a pool of 500M coins at $4 a contract.
    const CONTRACT: u64 = 1_000_000_000_000;
    const MARK: u64 = 4_000_000;
    const POOL: Pool = Pool { base_reserve: 500_000_000_000_000, spot: 4_000_000, high: 4_000_000, low: 4_000_000 };
    const PARAMS: Params = Params { oi_cap_bps: 500, max_gap_bps: 50, paid_max_gap_bps: 2_000 };
    const ONE: i128 = POS_SCALE as i128;

    fn snap(position_q: i128, pool: Option<Pool>) -> Snapshot {
        Snapshot { position_q, oi_long_q: 0, oi_short_q: 0, pool, contract_base_atoms: CONTRACT, fee_paid: 0, sampled: true }
    }

    fn at(spot: u64) -> Option<Pool> {
        Some(Pool { spot, high: spot, low: spot, ..POOL })
    }

    #[test]
    fn the_cap_is_a_share_of_the_pools_coin_reserve() {
        // 500 contracts in the pool, 5% of it: 25 contracts
        assert_eq!(oi_cap_q(&POOL, MARK, CONTRACT, 500), 25 * POS_SCALE);
        // pumped to $5: the mark is lower, the reserve counts at the mark
        assert_eq!(oi_cap_q(&at(5_000_000).unwrap(), MARK, CONTRACT, 500), 25 * POS_SCALE);
        // dumped to $2: the reserve counts at $2 against a $4 mark, half
        assert_eq!(oi_cap_q(&at(2_000_000).unwrap(), MARK, CONTRACT, 500), 12_500_000);
    }

    #[test]
    fn an_open_within_the_rules_is_filled_whole() {
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, 10 * ONE), Ok(10 * ONE));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, -10 * ONE), Ok(-10 * ONE));
    }

    #[test]
    fn an_open_past_the_cap_is_filled_up_to_it() {
        let mut s = snap(0, Some(POOL));
        s.oi_long_q = 20 * POS_SCALE;
        assert_eq!(fill(&s, &PARAMS, MARK, 10 * ONE), Ok(5 * ONE));
        s.oi_long_q = 25 * POS_SCALE;
        assert_eq!(fill(&s, &PARAMS, MARK, 10 * ONE), Err(Refusal::OiCap));
        // the other side has its own room
        assert_eq!(fill(&s, &PARAMS, MARK, -10 * ONE), Ok(-10 * ONE));
    }

    #[test]
    fn the_gaining_side_is_refused_past_the_gap_and_the_other_side_is_not() {
        // the pool 1% above the mark: longs gain as the mark catches up
        assert_eq!(fill(&snap(0, at(4_040_000)), &PARAMS, MARK, ONE), Err(Refusal::Gap));
        assert_eq!(fill(&snap(0, at(4_040_000)), &PARAMS, MARK, -ONE), Ok(-ONE));
        // within the free gap, both open
        assert_eq!(fill(&snap(0, at(4_020_000)), &PARAMS, MARK, ONE), Ok(ONE));
        // the pool 1% below the mark: shorts gain
        assert_eq!(fill(&snap(0, at(3_960_000)), &PARAMS, MARK, -ONE), Err(Refusal::Gap));
    }

    #[test]
    fn a_price_sampled_in_the_window_counts_though_the_pool_is_back_at_the_mark() {
        // the trader moved the pool back to the mark in its own transaction; the keeper saw it 1% up 20 s ago
        let hidden = Some(Pool { high: 4_040_000, ..POOL });
        assert_eq!(fill(&snap(0, hidden), &PARAMS, MARK, ONE), Err(Refusal::Gap));
        assert_eq!(fill(&snap(0, hidden), &PARAMS, MARK, -ONE), Ok(-ONE));
    }

    #[test]
    fn a_paid_gap_buys_the_entry_it_pays_for() {
        // 1% up: 40,000 atoms a contract. 40,000 paid buys one contract, 20,000 half of one
        let mut s = snap(0, at(4_040_000));
        assert_eq!(fee_for(ONE as u128, 40_000), 40_000);
        s.fee_paid = 40_000;
        assert_eq!(fill(&s, &PARAMS, MARK, ONE), Ok(ONE));
        assert_eq!(fill(&s, &PARAMS, MARK, 2 * ONE), Ok(ONE));
        s.fee_paid = 20_000;
        assert_eq!(fill(&s, &PARAMS, MARK, ONE), Ok(ONE / 2));
        // paid, and still under the cap
        s.fee_paid = u64::MAX;
        s.oi_long_q = 24 * POS_SCALE;
        assert_eq!(fill(&s, &PARAMS, MARK, 10 * ONE), Ok(ONE));
    }

    #[test]
    fn a_gap_past_what_the_house_sells_is_refused_whatever_is_paid() {
        let mut s = snap(0, at(5_000_000)); // 25%
        s.fee_paid = u64::MAX;
        assert_eq!(fill(&s, &PARAMS, MARK, ONE), Err(Refusal::Gap));
    }

    #[test]
    fn closing_is_always_filled() {
        for pool in [None, at(8_000_000)] {
            let mut s = snap(-10 * ONE, pool);
            s.oi_long_q = u128::MAX; // no room at all
            // a short buys back in a pump, with the pool unreadable, past the cap: all of it
            assert_eq!(fill(&s, &PARAMS, MARK, 10 * ONE), Ok(10 * ONE));
            assert_eq!(fill(&s, &PARAMS, MARK, 4 * ONE), Ok(4 * ONE));
        }
    }

    #[test]
    fn a_close_that_flips_fills_the_close_and_opens_by_the_rules() {
        // short 10, buys 15 in a pump: the 10 close, the 5 long would gain from the gap
        assert_eq!(fill(&snap(-10 * ONE, at(4_400_000)), &PARAMS, MARK, 15 * ONE), Ok(10 * ONE));
        // with the pool at the mark, all 15
        assert_eq!(fill(&snap(-10 * ONE, Some(POOL)), &PARAMS, MARK, 15 * ONE), Ok(15 * ONE));
    }

    #[test]
    fn adding_to_a_position_is_an_open() {
        assert_eq!(fill(&snap(10 * ONE, at(4_400_000)), &PARAMS, MARK, ONE), Err(Refusal::Gap));
    }

    #[test]
    fn an_unreadable_pool_refuses_opens() {
        assert_eq!(fill(&snap(0, None), &PARAMS, MARK, ONE), Err(Refusal::PoolUnreadable));
    }

    #[test]
    fn without_a_keeper_sample_in_the_window_nothing_opens_and_closes_still_fill() {
        let mut s = snap(-10 * ONE, Some(POOL));
        s.sampled = false;
        assert_eq!(fill(&s, &PARAMS, MARK, -ONE), Err(Refusal::Unsampled));
        assert_eq!(fill(&s, &PARAMS, MARK, 4 * ONE), Ok(4 * ONE));
        assert_eq!(fill(&s, &PARAMS, MARK, 12 * ONE), Ok(10 * ONE));
    }

    #[test]
    fn nonsense_is_refused() {
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, 0), Err(Refusal::BadRequest));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, MARK, i128::MIN), Err(Refusal::BadRequest));
        assert_eq!(fill(&snap(0, Some(POOL)), &PARAMS, 0, 1), Err(Refusal::BadRequest));
    }

    #[test]
    fn buckets_keep_the_extremes_of_their_span_and_forget_old_ones() {
        let mut b = [Bucket::default(); BUCKETS];
        // a 60 s window: 10 s buckets
        record(&mut b, 60, 1_000, 100);
        record(&mut b, 60, 1_003, 140);
        record(&mut b, 60, 1_005, 90);
        // more prices cannot lower the high or raise the low
        record(&mut b, 60, 1_009, 100);
        assert_eq!(range(&b, 60, 1_009, 100), (140, 90));
        record(&mut b, 60, 1_030, 100);
        assert_eq!(range(&b, 60, 1_030, 100), (140, 90));
        // the bucket that saw 140 starts at 1,000 and ends at 1,010: gone from the window at 1,070
        assert_eq!(range(&b, 60, 1_069, 100), (140, 90));
        assert_eq!(range(&b, 60, 1_070, 100), (100, 100));
        // its slot comes round again at 1,060 and starts afresh: the window keeps the last 50 to 60 seconds
        record(&mut b, 60, 1_061, 105);
        assert_eq!(range(&b, 60, 1_061, 100), (105, 100));
        assert_eq!(b[(1_061 / 10 % BUCKETS as i64) as usize], Bucket { start: 1_060, high: 105, low: 105 });
    }

    #[test]
    fn no_overflow_at_the_extremes() {
        let huge = Pool { base_reserve: u64::MAX, spot: u64::MAX, high: u64::MAX, low: 0 };
        let _ = oi_cap_q(&huge, u64::MAX, 1, u64::MAX);
        let _ = oi_cap_q(&huge, 1, 1, u64::MAX);
        let _ = gap(&huge, 1, true);
        let _ = fee_for(u128::MAX / 2, u64::MAX);
        let s = Snapshot { position_q: 0, oi_long_q: 0, oi_short_q: 0, pool: Some(huge), contract_base_atoms: 1, fee_paid: u64::MAX, sampled: true };
        let _ = fill(&s, &Params { oi_cap_bps: u64::MAX, max_gap_bps: 0, paid_max_gap_bps: u64::MAX }, 1, i128::MAX);
    }
}
