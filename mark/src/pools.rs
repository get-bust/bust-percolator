//! Pool layouts: where each kind of pool keeps its vaults and mints, and how its reserves are read from them.
//!
//! PumpSwap: the vault balances are the reserves. Offsets from the pool account: discriminator, bump, index, creator,
//! base mint (43), quote mint (75), LP mint, base vault (139), quote vault (171).
//!
//! Raydium CPMM (`raydium-cp-swap`, `PoolState`, packed, 637 bytes): token 0 and 1 vaults at 72 and 104, mints at 168
//! and 200, status at 329 (bit 2: swaps disabled), then the fees the pool owes and keeps in its vaults until they are
//! collected: protocol at 341 and 349, fund at 357 and 365, creator at 397 and 405. Its reserves are each vault less
//! those three fees, as `PoolState::vault_amount_without_fee` computes them; open_time at 373.

use solana_program::pubkey::Pubkey;

pub const KIND_PUMPSWAP: u8 = 0;
pub const KIND_RAYDIUM_CPMM: u8 = 1;

pub const PUMPSWAP: Pubkey = solana_program::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const RAYDIUM_CPMM: Pubkey = solana_program::pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const RAYDIUM_CPMM_DEVNET: Pubkey = solana_program::pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
pub const CPMM_LEN: usize = 637;

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
    } else {
        None
    }
}

/// The pool's (base vault, quote vault, base side) with `quote_mint` as the quote, or None if the pool does not hold
/// that mint. The base side is 0 or 1 for a CPMM pool, whose tokens are ordered by mint, and 0 for PumpSwap.
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
        _ => None,
    }
}

/// The pool's (base, quote) reserves from its vault balances. None for a CPMM pool that is not open for swaps or whose
/// fees exceed a vault.
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
