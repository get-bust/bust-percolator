//! The pool's price and its time-weighted average, in plain integers so they can be tested off chain.
//!
//! The average is kept the way Uniswap v2 keeps it: a running sum of price times seconds. Each observation stores the
//! time and the sum up to that time. The price a sample reads holds from that sample until the next one, so a price
//! read in a transaction carries no weight in the mark that transaction pushes; it starts to count from then on.

/// One observation: a unix time and the sum of price times seconds up to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Obs {
    pub ts: i64,
    pub cum: u128,
}

/// The pool's price in collateral atoms per contract: quote reserve over base reserve, times the base atoms in one
/// contract. None for an empty side or a price that does not fit a u64.
pub fn spot(base: u64, quote: u64, contract_base_atoms: u64) -> Option<u64> {
    if base == 0 || quote == 0 {
        return None;
    }
    let p = (quote as u128).checked_mul(contract_base_atoms as u128)? / base as u128;
    u64::try_from(p).ok().filter(|&p| p > 0)
}

/// Holds a sample within `max_step_bps` of the price before it, so one transaction that pushes the pool and samples
/// it can move the average only one step. Zero turns the limit off.
pub fn clamp_step(spot: u64, last: u64, max_step_bps: u64) -> u64 {
    if max_step_bps == 0 {
        return spot;
    }
    let step = (last as u128 * max_step_bps as u128 / 10_000) as u64;
    spot.clamp(last.saturating_sub(step), last.saturating_add(step))
}

/// The sum at `now` from the last observation and the price held since it.
pub fn cum_at_now(last: Obs, last_price: u64, now: i64) -> Option<u128> {
    let dt = u128::try_from(now.checked_sub(last.ts)?).ok()?;
    last.cum.checked_add((last_price as u128).checked_mul(dt)?)
}

/// The average over the `window` seconds ending at the newest observation. `obs` runs oldest first and must reach
/// back to the start of the window.
pub fn average(obs: &[Obs], window: i64) -> Option<u64> {
    let newest = *obs.last()?;
    if window <= 0 {
        return None;
    }
    let start = newest.ts.checked_sub(window)?;
    // the last observation at or before the start, and the one after it
    let i = obs.iter().rposition(|o| o.ts <= start)?;
    let (a, b) = (obs[i], *obs.get(i + 1)?);
    let span = u128::try_from(b.ts - a.ts).ok().filter(|&s| s > 0)?;
    let into = u128::try_from(start - a.ts).ok()?;
    let cum_start = a.cum.checked_add(b.cum.checked_sub(a.cum)?.checked_mul(into)? / span)?;
    u64::try_from(newest.cum.checked_sub(cum_start)? / window as u128).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Observations for a price path given as (seconds held, price), starting at t = 0.
    fn path(steps: &[(i64, u64)]) -> Vec<Obs> {
        let mut obs = vec![Obs { ts: 0, cum: 0 }];
        for &(secs, price) in steps {
            let last = *obs.last().unwrap();
            obs.push(Obs { ts: last.ts + secs, cum: cum_at_now(last, price, last.ts + secs).unwrap() });
        }
        obs
    }

    #[test]
    fn spot_is_quote_over_base_per_contract() {
        // 500M coins (6 decimals) against 2,000 USDC: 4 USDC per 1M coins
        assert_eq!(spot(500_000_000_000_000, 2_000_000_000, 1_000_000_000_000), Some(4_000_000));
        assert_eq!(spot(0, 1, 1), None);
        assert_eq!(spot(10, 1, 1), None); // rounds to zero
    }

    #[test]
    fn a_flat_price_averages_to_itself() {
        assert_eq!(average(&path(&[(1800, 4_000_000)]), 1800), Some(4_000_000));
        assert_eq!(average(&path(&[(600, 4_000_000), (600, 4_000_000), (600, 4_000_000)]), 1800), Some(4_000_000));
    }

    #[test]
    fn a_pump_counts_for_the_time_it_is_held() {
        // 1,790 s at 4, then 10 s at 8: the average moves by 4 * 10 / 1800
        let avg = average(&path(&[(1790, 4_000_000), (10, 8_000_000)]), 1800).unwrap();
        assert_eq!(avg, 4_022_222);
    }

    #[test]
    fn the_window_starts_inside_an_interval() {
        // 1,000 s at 2, then 1,800 s at 4: a 1,800 s window sees only the 4
        assert_eq!(average(&path(&[(1000, 2_000_000), (1800, 4_000_000)]), 1800), Some(4_000_000));
        // a 2,000 s window sees 200 s at 2 and 1,800 s at 4
        assert_eq!(average(&path(&[(1000, 2_000_000), (1800, 4_000_000)]), 2000), Some(3_800_000));
    }

    #[test]
    fn history_short_of_the_window_is_refused() {
        assert_eq!(average(&path(&[(100, 4_000_000)]), 1800), None);
    }

    #[test]
    fn a_step_is_held_to_its_limit() {
        assert_eq!(clamp_step(8_000_000, 4_000_000, 100), 4_040_000);
        assert_eq!(clamp_step(1_000_000, 4_000_000, 100), 3_960_000);
        assert_eq!(clamp_step(4_010_000, 4_000_000, 100), 4_010_000);
        assert_eq!(clamp_step(8_000_000, 4_000_000, 0), 8_000_000);
    }
}
