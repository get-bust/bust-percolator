//! Pool layouts: where each kind of pool keeps its vaults and mints, and how its reserves are read from them.
//!
//! PumpSwap: the vault balances are the reserves. Offsets from the pool account: discriminator, bump, index, creator,
//! base mint (43), quote mint (75), LP mint, base vault (139), quote vault (171).
//!
//! Raydium CPMM (`raydium-cp-swap`, `PoolState`, packed, 637 bytes): token 0 and 1 vaults at 72 and 104, mints at 168
//! and 200, status at 329 (bit 2: swaps disabled), then the fees the pool owes and keeps in its vaults until they are
//! collected: protocol at 341 and 349, fund at 357 and 365, creator at 397 and 405. Its reserves are each vault less
//! those three fees, as `PoolState::vault_amount_without_fee` computes them; open_time at 373.
//!
//! Raydium AMM v4 (`raydium-amm`, `AmmInfo`, 752 bytes, no discriminator, u64 fields aligned to 8): status at 0,
//! `need_take_pnl_coin` and `need_take_pnl_pc` at 192 and 200, `pool_open_time` at 224, coin and pc vaults at 336 and
//! 368, their mints at 400 and 432. Its reserves are each vault less the PnL it has yet to take, as
//! `calc_total_without_take_pnl_no_orderbook` computes them for a swap. Older Pump coins graduated to these pools.

use solana_program::pubkey::Pubkey;

pub const KIND_PUMPSWAP: u8 = 0;
pub const KIND_RAYDIUM_CPMM: u8 = 1;
pub const KIND_RAYDIUM_AMM_V4: u8 = 2;

pub const PUMPSWAP: Pubkey = solana_program::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const RAYDIUM_CPMM: Pubkey = solana_program::pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_CPMM_DEVNET: Pubkey = solana_program::pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
pub const CPMM_LEN: usize = 637;
pub const RAYDIUM_AMM_V4: Pubkey = solana_program::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_AMM_V4_DEVNET: Pubkey = solana_program::pubkey!("DRaya7Kj3aMWQSy19kSjvmuwq9docCHofyP9kanQGaav");
pub const AMM_V4_LEN: usize = 752;

fn key_at(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}

fn u64_at(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}

/// A pool's kind from its owner program.
pub fn kind_of(owner: &Pubkey) -> Option<u8> {
    if *owner == PUMPSWAP {
        Some(KIND_PUMPSWAP)
    } else if *owner == RAYDIUM_CPMM || *owner == RAYDIUM_CPMM_DEVNET {
        Some(KIND_RAYDIUM_CPMM)
    } else if *owner == RAYDIUM_AMM_V4 || *owner == RAYDIUM_AMM_V4_DEVNET {
        Some(KIND_RAYDIUM_AMM_V4)
    } else {
        None
    }
}

/// The pool's (base vault, quote vault, base side) with `quote_mint` as the quote, or None if the pool does not hold
/// that mint. The base side is which of a Raydium pool's two tokens is the coin (CPMM orders them by mint, AMM v4
/// keeps a coin and a pc side that either may be), and 0 for PumpSwap.
pub fn vaults(kind: u8, pool: &[u8], quote_mint: &Pubkey) -> Option<(Pubkey, Pubkey, u8)> {
    match kind {
        KIND_PUMPSWAP if pool.len() >= 203 => (key_at(pool, 75) == *quote_mint).then(|| (key_at(pool, 139), key_at(pool, 171), 0)),
        KIND_RAYDIUM_CPMM if pool.len() == CPMM_LEN => {
            if key_at(pool, 200) == *quote_mint {
                Some((key_at(pool, 72), key_at(pool, 104), 0))
            } else if key_at(pool, 168) == *quote_mint {
                Some((key_at(pool, 104), key_at(pool, 72), 1))
            } else {
                None
            }
        }
        KIND_RAYDIUM_AMM_V4 if pool.len() == AMM_V4_LEN => {
            if key_at(pool, 432) == *quote_mint {
                Some((key_at(pool, 336), key_at(pool, 368), 0))
            } else if key_at(pool, 400) == *quote_mint {
                Some((key_at(pool, 368), key_at(pool, 336), 1))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The pool's (base, quote) reserves from its vault balances. None for a Raydium pool that is not open for swaps or
/// whose fees or PnL exceed a vault.
pub fn reserves(kind: u8, pool: &[u8], base_side: u8, base_vault: u64, quote_vault: u64, now: i64) -> Option<(u64, u64)> {
    match kind {
        KIND_PUMPSWAP => Some((base_vault, quote_vault)),
        KIND_RAYDIUM_CPMM if pool.len() == CPMM_LEN => {
            if pool[329] & 4 != 0 || u64_at(pool, 373) > u64::try_from(now).ok()? {
                return None;
            }
            let fees = |side: u8| -> Option<u64> {
                let (p, f, c) = if side == 0 { (341, 357, 397) } else { (349, 365, 405) };
                u64_at(pool, p).checked_add(u64_at(pool, f))?.checked_add(u64_at(pool, c))
            };
            let quote_side = 1 - base_side;
            Some((base_vault.checked_sub(fees(base_side)?)?, quote_vault.checked_sub(fees(quote_side)?)?))
        }
        KIND_RAYDIUM_AMM_V4 if pool.len() == AMM_V4_LEN => {
            // swaps are allowed when initialized (1), swap-only (6), or waiting for the open time (7) once it is past
            let status = u64_at(pool, 0);
            let open = u64_at(pool, 224);
            if !(status == 1 || status == 6 || status == 7) || (status == 7 && open > u64::try_from(now).ok()?) {
                return None;
            }
            // side 0 is the coin side
            let pnl = |side: u8| u64_at(pool, if side == 0 { 192 } else { 200 });
            Some((base_vault.checked_sub(pnl(base_side))?, quote_vault.checked_sub(pnl(1 - base_side))?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WSOL: Pubkey = solana_program::pubkey!("So11111111111111111111111111111111111111112");

    /// The USELESS/SOL CPMM pool (Q2sPHPdU…) as read on mainnet on 2026-11-26: token 0 is SOL, token 1 the coin.
    fn useless_pool() -> (Vec<u8>, Pubkey) {
        let coin = solana_program::pubkey!("Dz9mQ9NzkBcCsuGPFJ3r1bS4wgqKMHBPiVuniW8Mbonk");
        let mut d = vec![0u8; CPMM_LEN];
        d[72..104].copy_from_slice(&[1; 32]);
        d[104..136].copy_from_slice(&[2; 32]);
        d[168..200].copy_from_slice(WSOL.as_ref());
        d[200..232].copy_from_slice(coin.as_ref());
        // fees owed in SOL (side 0) and in the coin (side 1), split across protocol, fund and creator
        for (o, v) in [(341u64, 100_000_000u64), (357, 4_134_040), (397, 0), (349, 70_000_000), (365, 875_829), (405, 0)] {
            d[o as usize..o as usize + 8].copy_from_slice(&v.to_le_bytes());
        }
        (d, coin)
    }

    #[test]
    fn a_cpmm_pool_finds_its_base_on_either_side() {
        let (d, coin) = useless_pool();
        let (base, quote, side) = vaults(KIND_RAYDIUM_CPMM, &d, &WSOL).unwrap();
        assert_eq!((base, quote, side), (Pubkey::new_from_array([2; 32]), Pubkey::new_from_array([1; 32]), 1));
        // quoted in the coin instead, the sides swap
        assert_eq!(vaults(KIND_RAYDIUM_CPMM, &d, &coin).unwrap().2, 0);
        assert_eq!(vaults(KIND_RAYDIUM_CPMM, &d, &Pubkey::new_unique()), None);
    }

    #[test]
    fn cpmm_reserves_leave_out_the_fees_it_owes() {
        let (d, _) = useless_pool();
        // vaults: 15,847,983.304499 coins and 16,666.323041047 SOL
        let (base, quote) = reserves(KIND_RAYDIUM_CPMM, &d, 1, 15_847_983_304_499, 16_666_323_041_047, 1_800_000_000).unwrap();
        assert_eq!((base, quote), (15_847_983_304_499 - 70_875_829, 16_666_323_041_047 - 104_134_040));
        // 0.00105163 SOL a coin, against GeckoTerminal's 0.00104934 from the last trade
        let price = (quote as f64 / 1e9) / (base as f64 / 1e6);
        assert!((price - 0.0010516).abs() < 1e-7, "{price}");
    }

    /// The pippin/SOL AMM v4 pool (8WwcNqdZ…) as read on mainnet on 2026-11-26: its coin side is SOL, its pc side
    /// the Pump coin.
    fn pippin_pool() -> Vec<u8> {
        let pippin = solana_program::pubkey!("Dfh5DzRgSvvCFDoYc2ciTkMrbDfRKybA4SoFbPmApump");
        let mut d = vec![0u8; AMM_V4_LEN];
        d[0..8].copy_from_slice(&6u64.to_le_bytes());
        d[336..368].copy_from_slice(&[1; 32]);
        d[368..400].copy_from_slice(&[2; 32]);
        d[400..432].copy_from_slice(WSOL.as_ref());
        d[432..464].copy_from_slice(pippin.as_ref());
        d
    }

    #[test]
    fn an_amm_v4_pool_with_the_coin_on_its_pc_side() {
        let mut d = pippin_pool();
        assert_eq!(vaults(KIND_RAYDIUM_AMM_V4, &d, &WSOL).unwrap(), (Pubkey::new_from_array([2; 32]), Pubkey::new_from_array([1; 32]), 1));
        // PnL owed on the coin side (SOL) and the pc side (the coin) comes off the matching reserve
        d[192..200].copy_from_slice(&5u64.to_le_bytes());
        d[200..208].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(reserves(KIND_RAYDIUM_AMM_V4, &d, 1, 1_000, 2_000, 1_800_000_000), Some((993, 1_995)));
    }

    #[test]
    fn an_amm_v4_pool_that_cannot_swap_is_not_read() {
        let mut d = pippin_pool();
        for status in [0u64, 2, 3, 4, 5] {
            d[0..8].copy_from_slice(&status.to_le_bytes());
            assert_eq!(reserves(KIND_RAYDIUM_AMM_V4, &d, 1, 1_000, 2_000, 1_800_000_000), None, "status {status}");
        }
        d[0..8].copy_from_slice(&7u64.to_le_bytes());
        d[224..232].copy_from_slice(&1_900_000_000u64.to_le_bytes());
        assert_eq!(reserves(KIND_RAYDIUM_AMM_V4, &d, 1, 1_000, 2_000, 1_800_000_000), None);
        assert!(reserves(KIND_RAYDIUM_AMM_V4, &d, 1, 1_000, 2_000, 1_900_000_000).is_some());
    }

    #[test]
    fn a_closed_cpmm_pool_is_not_read() {
        let (mut d, _) = useless_pool();
        d[329] = 4;
        assert_eq!(reserves(KIND_RAYDIUM_CPMM, &d, 1, 10_000_000_000, 10_000_000_000, 1_800_000_000), None);
        d[329] = 0;
        d[373..381].copy_from_slice(&1_900_000_000u64.to_le_bytes());
        assert_eq!(reserves(KIND_RAYDIUM_CPMM, &d, 1, 10_000_000_000, 10_000_000_000, 1_800_000_000), None);
    }
}
