# Devnet runs

Four runs on 23 November 2026 asked one question: if the price of a perp follows a Pump coin's pool, can the trader
holding the perp move the pool far enough, for long enough, to take more from the house than moving the pool costs?

Runs 1 and 2 had no defenses beyond the engine's price cap, and the attack paid. Runs 3 and 4 had the defenses below. In
run 3 nobody traded against the pump, and the attack still paid. In run 4 holders sold into it, and the attack lost.

## Setup

| | |
|---|---|
| Program | [`7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo`](https://explorer.solana.com/address/7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo?cluster=devnet), percolator-prog `aac89b9` |
| Market group | runs 1-2 [`B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF`](https://explorer.solana.com/address/B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF?cluster=devnet), runs 3-4 [`DjBxSVasLpCbgTubnSDtVZeSwc4kRW8nUePvBkJdMdLL`](https://explorer.solana.com/address/DjBxSVasLpCbgTubnSDtVZeSwc4kRW8nUePvBkJdMdLL?cluster=devnet) |
| Test coin | TMEME [`Gi9X3LhFeMarqEBB19mdHqErP6ZEh9U2GtsKLThbECB1`](https://explorer.solana.com/address/Gi9X3LhFeMarqEBB19mdHqErP6ZEh9U2GtsKLThbECB1?cluster=devnet) |
| Pool | PumpSwap [`JA8UWBKQ2cpfycCMzxsWJtrsBb2SbrxZTFDKM65Sjcy5`](https://explorer.solana.com/address/JA8UWBKQ2cpfycCMzxsWJtrsBb2SbrxZTFDKM65Sjcy5?cluster=devnet), opened at 500M TMEME against 2,000 USDC, 4 USDC per 1M TMEME |
| Margin | 10% initial (10x), 5% maintenance |
| Price cap | 10 bps per slot, at most 20 slots accrued per crank: one crank moves the effective price at most 2% |
| House | 10,000 USDC of capital as the counterparty, 1,000 USDC of insurance |
| Trader | 100 USDC |

The house played the attacker: the same wallet opened the trader's long, bought the pool up, held it, closed the long
and sold the coins back. Each run is a fresh trader, recorded tick by tick (`perc record`, `scripts/attack.cjs`) and
exported with `scripts/export-run.cjs`.

## Runs 1 and 2: the engine alone

The mark was the pool's 60 s time-weighted average, pushed every 3-4 s. The house had no backing and quoted anything.
The trader went long 200 contracts (200M TMEME, about 800 USDC, 8x).

| | Run 1 | Run 2 |
|---|---|---|
| Pump | 1,000 USDC, pool 4.00 → 8.99 (+125%) | 1,000 USDC, pool 4.01 → 9.00 (+124%) |
| Held | about 2 minutes, cranked by a loop every few seconds | 121.5 s, cranked by the recorder on every tick |
| Perp, entry → close | 4.00 → 4.84 (+21%) | 4.01 → 5.33 (+33%) |
| Trader's profit | 167.82 USDC | 299.76 USDC |
| Pool round trip | −5.16 USDC | −5.16 USDC |
| **Attacker, net** | **+162.66 USDC** | **+294.60 USDC** |

Run 2's transactions:
[open](https://explorer.solana.com/tx/44ptrm2Pf7jXVo6sRpCxchANBH2jZSRpR9waCb9f2GxLewTcuH42B5dPdGmEEh1igg2yDwaNvBURqU9TDWQ664kj?cluster=devnet),
[pump](https://explorer.solana.com/tx/2NmztwZBLQyRK29kd5vVNLtEw4HVqXFr4idjoXoMe6ZroRBhV6gRQ7JyR3hsFTwAfdcFoz7uGKbtjtxqNg2us9m3?cluster=devnet),
[close](https://explorer.solana.com/tx/2J43rdkcEsW22Y58Cqx1m5RFScGhXMyNKVFYfLWr68FgenBsjkTRUrv2z3nCuKA448H3oNov1yh8iNh4TDCMFfe6?cluster=devnet),
[sell back](https://explorer.solana.com/tx/63PDpfqc9WUnMs55FQohSUW6kS2BMLM4SWMQuW5xCeyGaRsZYGFYxmno7G9pjdk2SPiH5gsHqTyy7RjeSdePYHby?cluster=devnet).
The perp's effective price rose by one crank's step at a time, about 0.09 USDC per crank, for the whole two minutes.
Run 2 cranked more often than run 1, which is why it moved further.

What they showed:

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

## The defenses

Runs 3 and 4 ran on a new market group with four changes.

- **The house backs both sides.** It tops up the backing of the asset's long winners and of its short winners
  (`perc back`), so profit converts into capital and can be withdrawn. An honest winner did: a long of 10 contracts
  closed 1.8337 USDC ahead, converted, and withdrew 101.8 USDC.
- **The house quotes every trade** (`client/src/trade.rs`, limits in `devnet.json`). It co-signs as the counterparty,
  so its rules decide what can be opened:
  - after a trade that adds risk, a side's open interest may not exceed 5% of the pool's collateral depth;
  - when the pool and the mark disagree, the side that would profit as the mark catches up pays the gap as a fee, and
    past 1% the house does not open that side at all;
  - a trade that reduces risk is always quoted, at no fee.

  Tested on the new market: 30 contracts (126 USDC against a cap of 100.42) refused; shorts refused at gaps of 437 and
  107 bps; a short at an 83 bps gap paid 83 bps; the close paid nothing.
- **The mark is the pool's 30-minute time-weighted average**, still pushed by an off-chain pusher.
- **Holders** (`scripts/holders.cjs`). On devnet nobody else trades the pool. The holders stand in for the coin's
  holders and arbitrageurs on mainnet: when the pool trades above its 30-minute average they sell into it, below it they
  buy, pulling the price back with a half-life of 120 s. The half-life is an assumption, not a measurement.

The open interest cap allowed 24 contracts (about 97 USDC, 1x on the trader's 100) against the 2,000 USDC pool.

## Runs 3 and 4: with the defenses

| | Run 3, no holders | Run 4, holders |
|---|---|---|
| Open | 24 contracts at 4.0250, fee 0 | 24 contracts at 4.0334, fee 0 |
| Pump | 1,000 USDC, pool 4.03 → 9.01 (+124%) | 1,000 USDC, pool 4.03 → 9.01 (+124%) |
| Held | 78 minutes (see below) | 124.7 s; the holders sold into the pump 25 times, the pool was at 6.52 at the close |
| Perp, entry → close | 4.03 → 9.01 (+124%) | 4.03 → 4.26 (+5.7%) |
| Trader's profit | 119.60 USDC, converted and withdrawn | 5.76 USDC, converted in the close |
| Pool round trip | −5.16 USDC | −242.92 USDC: the coins went back into a pool the holders had sold into |
| **Attacker, net** | **+114.44 USDC** | **−237.16 USDC** |

Run 3:
[open](https://explorer.solana.com/tx/5roxnNTNeQrXC7WHFyzikmnVeQME3NftiUvwAZZynP5mGL7yNuUktNdkc1j7hCbJznRUfy9VwtZuUAuPuTdt72pX?cluster=devnet),
[pump](https://explorer.solana.com/tx/4V84z8BTyWzx4L7nCsiSF9vHRLhdgfUXHCtyrkCnV3bHnvDYTwvKZ1mMU7phCw2f1iEm5omUu1e9scbBsAJURT5f?cluster=devnet),
[close](https://explorer.solana.com/tx/4dUviYLfJKU54aRkkfZPWeKcrMwZsEZExEgDzQHaagVzX2WiH679u1tAisMFDCZLKAq1JLhhk8YmLaYoYqXseczB?cluster=devnet),
[sell back](https://explorer.solana.com/tx/2i5msD4jG4wafEuycn5B2gfE6KJ5NbXEx3Mx4Xbi5BAnh7bdP51trD2YNUfPtVGgkva3wCtZuCT9QoLxhV31HQA4?cluster=devnet),
[convert](https://explorer.solana.com/tx/5G8QTsr32oJL4xouYCbggd7ozZDESwfVQvqKrG4gbgTzHCkERn8gTHAFe2ToJtJER7P6aYMTG7gvYA1p8DybYBhM?cluster=devnet),
[withdraw](https://explorer.solana.com/tx/1oAdREonmZ3UG7rquMMMvrfjS4FPpAubvJNcnA8WN1W4H2Jha1FPFEVfbLLetksdEdQnjFGnUbqjZPBGg7QFpfu?cluster=devnet).
Run 4:
[open](https://explorer.solana.com/tx/8X6S4xkio3HBzcdLzivxCDvMpjdwh1MGpBytgSB8nFfkJ86n6xjAwZrNNJ88tzsSz84Q2gA4cZQsuhGY3fNSkqZ?cluster=devnet),
[pump](https://explorer.solana.com/tx/2v8cY58RxnXq61C1RsjAFiDgZhG9rv1xJyrVcqggudwWWMpveuTGF5cZhNVrHG5SSZUCmFJGgYskKaaoXi9JART9?cluster=devnet),
[close and convert](https://explorer.solana.com/tx/4bSCyRCsoKNMSyu7LerWf2wBr7rRAyWitq8fbvBdu5nBq9op5vGiATEuLxcL7EgqtzXDzK5w7ncM2NQaHgkJKQL5?cluster=devnet),
[sell back](https://explorer.solana.com/tx/5pSGWHfqszien6g7k1HrGRHR9swis2ZMUZ7NUGhrMCxgDmH2DmmT2fTCFEr6EndM4aYP9xdwKGp73MdgRdMr8Y5c?cluster=devnet).

Run 3 was meant to hold the pump for 120 s. The script waited on the buy's websocket confirmation, which never came,
though the buy had landed, and the pool stayed pumped for 78 minutes before the long was closed by hand. The scripts now
confirm by polling over HTTP and give up on a step after three minutes. The accident made run 3 the devnet worst case:
with nobody trading against the pump, holding it cost nothing, and the 30-minute average caught up all the way.

What they showed:

1. **The open interest cap bounds the prize, not the attack.** Against a 2,000 USDC pool the cap allowed about 97 USDC
   of open interest, so a 124% move could pay at most about 120 USDC. Run 3 took all of it. In a constant-product pool a
   buy sold straight back costs only the pool's fees; what makes a pump expensive is the time it must be held and the
   sellers who meet it there.
2. **A longer average makes that time long.** With a 30-minute average the perp moved 5.7% in two minutes against
   run 2's 33% with a 60 s average. In run 4 the holders sold into the pump while the average lagged, and selling the coins
   back cost 42 times what the perp paid.
3. **The result depends on there being sellers.** Run 3 and run 4 differ only in the holders. A coin whose pool nobody
   trades is the case run 3 shows, and such a coin should not be listed.
4. **Profit converts only while the portfolio's certificate is current.** Conversion needs the portfolio's health
   certificate stamped at the market's current oracle epoch. A crank does not refresh the certificate of a portfolio with
   no position, so once a new mark was pushed after run 3's close, every conversion was refused (`EngineStale`). The
   trader got it out by trading 1 contract, closing it and converting at once. The client now converts in the closing
   trade's own transaction, as run 4 did.
5. **Backing that lapses locks its side.** On the first market group a loser's payment left a backing bucket marked
   fresh past its expiry; until something expires it, the bucket refuses both conversions against it (`EngineStale`) and
   new top-ups (`EngineLockActive`), and cranking the portfolios that held claims on it did not. The house's backing on
   the new market group lasts 2,000,000 slots, about nine days, and has to be renewed before it lapses.

## Before mainnet

- Done on devnet: open interest per coin capped by the pool's depth, a gap fee and limit, the house as backing provider,
  a 30-minute average, and the attack again until it lost money (run 4).
- The house's quoting rules move from the client into an on-chain matcher, and the average from the off-chain pusher
  into an adapter program that reads the pool.
- A listing floor: only coins whose pools trade enough that a pump meets sellers. Run 4's half-life of 120 s is an
  assumption; each coin's has to be measured from its pool's own trades before it is listed.
- An attacker who keeps buying through the hold, instead of pumping once, has not been run.
- Renewing the backing before it lapses, and the house's exposure to the backing it provides.
