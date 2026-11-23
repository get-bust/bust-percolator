# Devnet runs

Two runs on 23 November 2026 asked one question: if the price of a perp follows a Pump coin's pool, can the trader
holding the perp move the pool far enough, for long enough, to take more from the house than moving the pool costs?

At the parameters below, yes. The engine slowed the perp down a lot, and the attack still paid.

## Setup

| | |
|---|---|
| Program | [`7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo`](https://explorer.solana.com/address/7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo?cluster=devnet), percolator-prog `aac89b9` |
| Market group | [`B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF`](https://explorer.solana.com/address/B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF?cluster=devnet) |
| Test coin | TMEME [`Gi9X3LhFeMarqEBB19mdHqErP6ZEh9U2GtsKLThbECB1`](https://explorer.solana.com/address/Gi9X3LhFeMarqEBB19mdHqErP6ZEh9U2GtsKLThbECB1?cluster=devnet) |
| Pool | PumpSwap [`JA8UWBKQ2cpfycCMzxsWJtrsBb2SbrxZTFDKM65Sjcy5`](https://explorer.solana.com/address/JA8UWBKQ2cpfycCMzxsWJtrsBb2SbrxZTFDKM65Sjcy5?cluster=devnet), 500M TMEME against 2,000 USDC, 4 USDC per 1M TMEME |
| Margin | 10% initial (10x), 5% maintenance |
| Mark | AuthMark, pushed every 3-4 s as the pool's 60 s time-weighted average |
| Price cap | 10 bps per slot, at most 20 slots accrued per crank: one crank moves the effective price at most 2% |
| House | 10,000 USDC of capital as the counterparty, 1,000 USDC of insurance, no backing |
| Trader | 100 USDC, long 200 contracts (200M TMEME, about 800 USDC, 8x) |

The house played the attacker: the same wallet opened the trader's long, bought the pool up, held it, closed the long
and sold the coins back. On devnet nobody else trades the pool, so a pump costs only the pool's fees; on mainnet other
traders sell into a pump and make holding it expensive. That makes these runs a lower bound on the attack's cost.

## Results

| | Run 1 | Run 2 |
|---|---|---|
| Pump | 1,000 USDC, pool 4.00 → 8.99 (+125%) | 1,000 USDC, pool 4.01 → 9.00 (+124%) |
| Held | about 2 minutes, cranked by a loop every few seconds | 121.5 s, cranked by the recorder on every tick |
| Perp, entry → close | 4.00 → 4.84 (+21%) | 4.01 → 5.33 (+33%) |
| Trader's profit | 167.82 USDC | 299.76 USDC |
| Pool round trip | −5.16 USDC | −5.16 USDC |
| **Attacker, net** | **+162.66 USDC** | **+294.60 USDC** |

Run 2 was recorded tick by tick (`perc record`, `scripts/attack.cjs`). Its transactions:
[open](https://explorer.solana.com/tx/44ptrm2Pf7jXVo6sRpCxchANBH2jZSRpR9waCb9f2GxLewTcuH42B5dPdGmEEh1igg2yDwaNvBURqU9TDWQ664kj?cluster=devnet),
[pump](https://explorer.solana.com/tx/2NmztwZBLQyRK29kd5vVNLtEw4HVqXFr4idjoXoMe6ZroRBhV6gRQ7JyR3hsFTwAfdcFoz7uGKbtjtxqNg2us9m3?cluster=devnet),
[close](https://explorer.solana.com/tx/2J43rdkcEsW22Y58Cqx1m5RFScGhXMyNKVFYfLWr68FgenBsjkTRUrv2z3nCuKA448H3oNov1yh8iNh4TDCMFfe6?cluster=devnet),
[sell back](https://explorer.solana.com/tx/63PDpfqc9WUnMs55FQohSUW6kS2BMLM4SWMQuW5xCeyGaRsZYGFYxmno7G9pjdk2SPiH5gsHqTyy7RjeSdePYHby?cluster=devnet).
The perp's effective price rose by one crank's step at a time, about 0.09 USDC per crank, for the whole two minutes.
Run 2 cranked more often than run 1, which is why it moved further.

## What the runs showed

1. **The price cap slows the perp but does not stop the attack.** The pool moved 124% at once; the perp moved 21-33%
   over two minutes. The attacker's open interest was 40% of the pool's depth on the collateral side, so even a
   third of the pool's move paid 30-60 times the cost of making it.
2. **The attacker could not withdraw the profit.** In a live market Percolator converts profit into capital only
   against the asset's backing; with no backing, the 167.82 USDC of run 1 stayed junior. The same holds for an honest
   winner, and a portfolio left with unconverted profit could not trade again: every trade it signed was refused.
3. **The execution price does not set the entry.** The engine books a position at its effective mark and uses the
   trade's execution price only to size its notional. Charging a trader the worse of the pool and the mark has to be a
   fee, not a price.
4. **The mark lags both ways.** After the coins were sold back the pool was at 4.02, while the 60 s average was still
   high and the effective price kept rising toward it. A trader opening a short at that moment would take the gap.
5. **A trade needs the asset accrued to its slot.** A trade that adds risk is refused while the asset's losses are
   not accrued to the current slot, so the client cranks both portfolios in the trade's own transaction.

## Before mainnet

- Open interest per coin capped by the pool's depth, so the cost of moving the pool always exceeds what moving it pays.
- A longer average (15-30 minutes) for the mark, read on-chain by an adapter program instead of an off-chain pusher.
- A fee for the gap between the pool and the mark, charged to the side that would profit from the lag.
- The house as each coin's backing provider, so honest winners can withdraw.
- The same attack again, until it loses money.
