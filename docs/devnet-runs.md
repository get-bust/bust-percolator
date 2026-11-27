# Devnet runs

Five runs on 23 and 24 November 2026 asked one question: if the price of a perp follows a Pump coin's pool, can the
trader holding the perp move the pool far enough, for long enough, to take more from the house than moving the pool
costs?

Runs 1 and 2 had no defenses beyond the engine's price cap, and the attack paid. Runs 3 to 5 had the defenses below. In
run 3 nobody traded against the pump, and the attack still paid. In run 4 holders sold into it, and the attack lost. In
run 5 the attacker bought back everything the holders sold and held the pump until the perp had taken the whole move,
and the attack lost more.

## Setup

| | |
|---|---|
| Program | [`7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo`](https://explorer.solana.com/address/7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo?cluster=devnet), percolator-prog `aac89b9` |
| Market group | runs 1-2 [`B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF`](https://explorer.solana.com/address/B478hYUsfcNixoqWoAnrAVYJrQN63uJpeDC2K9cJVdiF?cluster=devnet), runs 3-5 [`DjBxSVasLpCbgTubnSDtVZeSwc4kRW8nUePvBkJdMdLL`](https://explorer.solana.com/address/DjBxSVasLpCbgTubnSDtVZeSwc4kRW8nUePvBkJdMdLL?cluster=devnet) |
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

Runs 3 to 5 ran on a new market group with four changes.

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

## Run 5: the attacker holds the pump

Run 4's attacker left after two minutes. Run 5's stayed: the same market, trader, pump and holders as run 4 (half-life
120 s, starting with 150M TMEME and 3,000 USDC), but the long was held for 1,900 s, longer than the average's 30
minutes, and whenever the pool slipped more than 0.5% below the pumped price the attacker bought it back up
(`scripts/defend.cjs`). At the close every coin the attacker had bought, the pump's and the defense's, was sold back.

| | Run 5, holders, pump defended |
|---|---|
| Open | 24 contracts at 4.0007, fee 0 |
| Pump | 1,000 USDC, pool 4.00 → 8.96 (+124%) |
| Held | 31.7 minutes. The holders sold into the pump 57 times, all 150M TMEME they had, 149.4M of it in the first five minutes; the attacker bought the pool back up in 54 buys, 146.4M TMEME for 1,300.50 USDC |
| Perp, entry → close | 4.00 → 8.94 (+123%) |
| Trader's profit | 118.49 USDC, converted in the close |
| Pool round trip | −855.94 USDC: 2,300.50 in, 1,444.56 back. Selling 313.1M TMEME took the pool from 8.93 to 2.41 |
| **Attacker, net** | **−737.45 USDC** |

[open](https://explorer.solana.com/tx/37EDBQjhuRJbNJdRiMAFKJLJTZDhGnnCtUAWmv6ikSjiKRqpVnMVDjhcoShW7M5aMmAiDFSz1SDGTFZMpc83A3CY?cluster=devnet),
[pump](https://explorer.solana.com/tx/3jerPrhbtDA56K7rH4VHXrhAaUtWKiTkmmXMHEjnVrKouVfcWvR4qXJsdf26AFTfU4Gg1USj8yVKTHSo1v1v2AJm?cluster=devnet),
[close and convert](https://explorer.solana.com/tx/2pndw4PbKobCmJwoPXs7a1GfJ5n46q9n3FJYJRxLqnRGq8NaBphB39GeJTrCafmnjnwBwebNDYdp5b43uqyjMnNt?cluster=devnet),
[sell back](https://explorer.solana.com/tx/5UFSLLBBUHVVE8yGP1xE3kTfPjMnSFN4dHtByaQhKPynu24C6y5bsQnFajJKEkeX33hrF9yjCyGdQa4gzr1ReyYM?cluster=devnet).

6. **Holding a pump costs what the sellers hold, not how long it is held.** The perp took the whole move and paid its
   cap, 118 USDC. To get there the attacker had to buy back every coin the holders sold at the pumped price and sell it
   all again at the end into a pool that could not take it. The holders ran out of coins in about five minutes; after
   that the pump cost nothing more to hold. Two parts of the model favor the attacker and should be read with the
   result: the holders compare the price with their own 30-minute average, so their selling fades as that average
   rises, and their supply was finite, 150M TMEME, 30% of the coins the pool started with. A coin whose sellers hold
   little would cost little to defend.

## Renewing the backing

Both backing buckets were set to lapse after 2,000,000 slots, about nine days. Renewing them showed:

7. **A backing's expiry cannot be pushed out while it runs.** A bucket that is still fresh takes more backing only
   at the expiry it already has; a top-up with a later expiry is refused (`EngineLockActive`). A new expiry needs the
   bucket emptied first, by withdrawing its backing and topping it back up in one transaction (`perc renew`). The
   engine lets backing leave only while no loss is outstanding anywhere in the market: the first try was refused
   because a portfolio's loss had not been accrued, and cranking the portfolios cleared it. The short winners' bucket
   (domain 0), which had never paid a winner, then moved from slot 527,453,029 to 604,463,341, about a year out. The
   long winners' bucket (domain 1), which had paid run 3's winner 119.60 USDC and been paid into by losers since, was
   refused even with every position closed: the engine lets the part the losers paid in leave only once every position
   is settled and the other side's claims are backed, and which of the two held it was not pinned down. It still lapses
   at slot 527,453,036.

   A live market puts no cap on the expiry, so the client now backs for about a year from the first top-up
   (`BACKING_SLOTS`), and `perc back` adds to a fresh bucket at its own expiry. On mainnet the house's backing should be
   set far out from the start rather than renewed.

## Measuring the listing floor on mainnet

Runs 4 and 5 assumed how a pool answers a pump. `scripts/measure-pools.cjs` reads that from real graduated Pump coins:
GeckoTerminal's one-minute candles (the last 1,000 per pool) for PumpSwap's busiest pools by 24 h volume. A pump is a
minute whose high is at least 10% above the lowest low of that minute and the one before; for each pump the script
counts the minutes until the pool gives back half the rise. Minute candles cannot see inside a minute, so anything
faster reads as one minute. Read on 2026-11-26.

| pools | pumps | half back within 2 min | never half back |
|---|---|---|---|
| 58 of the 60 busiest, all under 10 hours old | 201 | 83 (41%) | 49 (24%) |
| Pompom, the one other memecoin in the 60 | 79 | 59 (75%) | 0 |
| the 8 of the 200 busiest that are at least 3 days old | 8 | 4 | 0 |

Findings:

8. **The busiest pools are coins hours old, and their pumps often stay.** Almost all are coins launched hours before, and a quarter of their pumps never come back inside the
   candles read: the price has moved, not been pushed. A 30-minute average marks such a coin to the new price late but
   correctly, and a short there loses for real. These coins are not candidates.
9. **Older pools pump rarely and give it back.** Only 8 of the 200 busiest PumpSwap pools are three days old or more, and two of them are PUMP/USDC and USDC/SOL.
   In about 16 hours each the older memecoins pumped 10% inside a minute 8 times in total, and every pump gave half
   back, four within two minutes and one only after 35 minutes. A pool where a 10% jump is rare and fades is what
   a perp on a 30-minute average can be listed on; the count is too small to set one half-life for all of them.

So the floor is two checks per coin: an age and volume gate, and the coin's own pump and fade measured from its own
candles before it is listed, with the open interest cap set from that.

## The mark read on chain

Runs 3 to 5 marked the asset from an off-chain pusher signed by the house, which is also the trader's counterparty.
`mark/` moves the average on chain: the feed reads the pool's vaults itself and its PDA holds the asset's oracle role, so
the house can no longer push a price (`perc push` now fails with Percolator's `Unauthorized`), and an update sooner than
the feed's interval is refused. The feed was opened on the run pool with a 30-minute window, a 10 s interval and a 1%
step, and the pool was then pumped 240% with 1,500 USDC:

| | before | 70 s after the buy |
|---|---|---|
| pool | 3.0917 | 10.5194 |
| price the feed holds | 3.0917 | 3.3814 (eight 1% steps) |
| mark | 3.0917 | 3.0993 (+0.25%) |

10. **The step limit is what stops a one-transaction pump, and it slows real moves too.** Without it, a transaction
    that buys, samples and sells back would hold the pumped price for one interval: at 10 s in a 30-minute window,
    0.56% of the pump. With it, a sample moves 1% at most. When a coin really rises 240%, the price the feed holds takes
    about twenty minutes (123 steps) to reach it, and the 30-minute average trails that; a coin's step and window are
    set together with its open interest cap when it is listed.

Mainnet pools are quoted in SOL, so a feed on a SOL pool prices each sample in dollars with Pyth's SOL/USD (the
sponsored `PriceUpdateV2` account, full verification, the feed id pinned, at most 120 s old and 1% wide, or the update
is refused). On a second test pool, 20M TMEME against 0.8 SOL, the feed read 0.04 SOL per 1M coins at $70.96 as
2.8385. Pumped 164% with 0.5 SOL, the price the feed held rose 1% a step and the mark 0.15% over 50 s; the pool's
dollar price also moved with SOL between samples, which the average takes in the same way.

Coins from Raydium's launchpad (LaunchLab, under letsbonk.fun) graduate to Raydium CPMM pools: on mainnet the
USELESS/SOL pool is owned by the CPMM program, while older Pump coins such as pippin and VINE sit in Raydium AMM v4
pools. A CPMM pool keeps the protocol, fund and creator fees it owes inside its vaults, so the feed reads its reserves
as each vault less those fees, as Raydium's `vault_amount_without_fee` does; read that way the USELESS pool gave
0.0010516 SOL a coin against GeckoTerminal's 0.0010493 from the last trade. On a devnet CPMM pool, 20M TMEME against
0.8 SOL, a 161% pump moved the price the feed held 1% a step and the mark 0.18% over 50 s, as on PumpSwap.

Raydium AMM v4 pools, where older Pump coins went, need an OpenBook market to be opened, so that reader was checked on
mainnet instead: `perc probe` reads any cluster's pool with the feed program's own layout code. Read on 2026-11-26,
each against GeckoTerminal's price of the last trade:

| pool | kind | feed's reading (SOL a coin) | GeckoTerminal |
|---|---|---|---|
| pippin/SOL `8WwcNqdZ…` | AMM v4, coin on the pc side | 0.0002559958 | 0.0002566261 |
| VINE/SOL `58fzJMbX…` | AMM v4 | 0.0001302146 | 0.0001300168 |
| USELESS/SOL `Q2sPHPdU…` | CPMM | 0.0010427929 | 0.0010403896 |
| FIONA/SOL `DjbLtX1H…` | PumpSwap | 0.0000270648 | 0.0000272591 |

The readings are the pools' reserves; a last trade includes its fee and price impact, so they differ by a fraction of
a percent.

An update costs about 16,100 compute units, Percolator's `PushAuthMark` included. The keeper (`perc feed run`) asks for
25,000 at 1,000 micro-lamports a unit: 5,025 lamports an update, 0.0434 SOL a day for one market at one update every
10 s. A minute without an update is reported; the average then holds the last price it read.

## The house's rules on chain

Up to here the house co-signed every trade (`TradeNoCpi`), so its quoting rules ran in the client and a trader could
trade, and close, only while the house was online and willing. `matcher/` moves the rules into a matcher program for
Percolator's `TradeCpi`, which the trader signs alone. Reading Percolator for it turned up three constraints:

11. **Percolator tells a matcher only the request.** The matcher is called with the size, the mark and some ids, and no
    account Percolator owns may be passed to it, so it cannot see the trader's position or the market's open interest.
    The trader's transaction therefore runs the matcher's `Prepare` as the instruction right before `TradeCpi`:
    `Prepare` reads them and checks through the instructions sysvar that the next instruction is that trade, and the
    matcher, when called, checks that the instruction running is that trade at the top level and the one before it
    its `Prepare`. Nothing runs in between, so the snapshot is the state the trade meets.
12. **The gap cannot be charged through `TradeCpi`.** An unsigned house is charged only the market's base fee, and on
    an `AuthMark` asset the fee is sized at the mark whatever price the matcher returns. The matcher refuses an open on
    the side that gains as the mark catches up once the pool is past the gap limit; closing is never refused. An entry
    wider than the limit needs the house's signature, with the gap as its fee (`TradeNoCpi`, as before).
13. **Trades against one house portfolio go one at a time.** Every trade advances the house portfolio's position
    epoch, and a trade names that epoch exactly, so of two trades built against the same epoch the second fails as
    stale and is sent again. This holds for the co-signed path too. More house portfolios would let trades run side
    by side.

The matcher was enabled on the house portfolio of the devnet market with an open interest cap of 5% of the pool and a
1% gap limit. The pool was the CPMM test pool (20M TMEME against SOL), 1.75% above the mark at the time, so longs were
on the gaining side. Each trade below was signed by the trader alone:

| request | position before | filled | why | compute units |
|---|---|---|---|---|
| long 1 | 0 | refused | pool 2.9146 against a mark of 2.8646: past the gap on the gaining side | |
| short 2 | 0 | short 1.0011, [tx](https://explorer.solana.com/tx/54Lz7mmyrm8VNChnfwfrSXDAtCb4ww7ZxxPaAwKqTzt6jgj4uDMbXbrjryHCZpxEMQXizoUk5AqxmgAzaicS8HVc?cluster=devnet) | the cap: 5% of a pool of 20.02 contracts | 325,986 |
| long 2 | short 1.0011 | long 1.0011, [tx](https://explorer.solana.com/tx/r5R7ArTb9LNpgP6RnMbRZgjdMitwzCSiZK19B39HWhj2zc2ZApXKFZoAAN6doiSL7qdTk2EphuYQBv6aDA6LDDu?cluster=devnet) | the close in full; the rest would open a long past the gap | 334,573 |
| short 0.5, with a feed update | 0 | short 0.5, [tx](https://explorer.solana.com/tx/UH3y4PZGB77zMWbhPYM3M6Qmwx29sowkPZFGbmE8VF6nFnV2y6s6VxbNGYR7TjnD1so6epyAXCgT8r8gxUuogXo?cluster=devnet) | | 342,077 |
| long 0.5, with a feed update | short 0.5 | long 0.5, [tx](https://explorer.solana.com/tx/TDLDKzVBQVEWopAh9gnMjfc6nsrv3p1NKwozHwkWn7QbUKxYLAyD6ipw9MJbrwaDNVNr1MVvfUnVYtc15TwM8L4?cluster=devnet) | a close, with the pool 2.4% above the mark | 430,870 |

With a feed update in the same transaction the trader needs no keeper either: the update pushes a new mark and the
cranks after it bring both portfolios to it (the other way round, Percolator refuses the trade with
`EngineLockActive`). A transaction holds the update, two cranks, `Prepare` and `TradeCpi` within 431,000 compute
units, a third of the limit.

`perc matcher attack` simulates the ways around the snapshot. Each was refused by the matcher:

| attempt | refused with |
|---|---|
| `TradeCpi` without `Prepare` | `NotPrepared` |
| another instruction between `Prepare` and `TradeCpi` | `NotATrade` (in `Prepare`) |
| two trades after one `Prepare`, the second with the epochs Percolator expects | `NotPrepared` (the second) |
| `Prepare` on another portfolio than the one that trades | `NotATrade` |

A program that calls `TradeCpi` itself is refused by the same check (the running top-level instruction must be
Percolator's), but was not tried on chain.

An open the matcher refuses for the gap can still be bought from the house: `perc order` quotes a paid entry, a
`TradeNoCpi` the house co-signs with the gap as its fee (plus the market's base fee), so the trader enters as if at
the pool's price, and the trader caps the fee it takes. With the matcher's gap limit set to the base fee (0 on
devnet) and the pool 0.53% above the mark:

| order | went | fee |
|---|---|---|
| long 1, accepting no fee | matcher refused for the gap; the house quoted 69 bps; not sent | |
| long 1, accepting 100 bps | paid entry, [tx](https://explorer.solana.com/tx/3Nn28rp6jeo3gtXhUnhb2qFyARvpguuKC5BRfSQjzobbog9XUgCR851yxYoVdNRdWn46iS6CeK7RhyQeS3HLCoaf?cluster=devnet) | 53 bps, 0.0151 USDC |
| short 1 (the close) | the matcher, the trader alone, [tx](https://explorer.solana.com/tx/2sJyE7CTdzyrf9MYEygUNtSp5zKdH8ghDhdqqaALwXbBTyCBRKiG763SfrSb6SSJpVKdijGXEb5UFWPzWM4hbQMK?cluster=devnet) | none |

Percolator charges the fee to both accounts and puts both into the asset's insurance, which the house funds: the
trader's share is what the house earns, its own share moves from its capital to its insurance.

14. **A trade the house signs revokes its matcher.** Percolator counts a `TradeNoCpi` on the house portfolio as a
    change of position outside the matcher and disables the matcher on it until the owner enables it again; the
    first paid entry left the trader's long impossible to close through the matcher. A trade the house signs now
    carries `SetMatcherConfig` right after it, in the same transaction. It also means the key that signs paid
    entries is the house portfolio's owner, the key that can withdraw its capital: on mainnet the paid entries
    belong on a house portfolio of their own, with an owner key of its own and only the capital they need.
15. **The fee ceiling is fixed when the market is opened.** Percolator refuses a trade whose fee is above the market's
    `max_trading_fee_bps`, and no instruction changes it afterwards. On the devnet market it is 1%, so a paid entry
    can cover a gap of 1% at most there; a mainnet market's ceiling is chosen at its opening with this in mind.

## Before mainnet

- Done on devnet: open interest per coin capped by the pool's depth, a gap fee and limit, the house as backing provider,
  a 30-minute average, the attack again until it lost money (run 4), and an attacker who held the pump through the
  whole average (run 5).
- The house's quoting rules are on chain on devnet (`matcher/`), and so is the average (`mark/`), SOL pools priced in
  dollars with Pyth, Raydium CPMM and AMM v4 pools read, and a keeper calls it; on mainnet the keeper runs on a host
  of its own, with a second one that anyone can run, since an update is permissionless, and a trader can update in
  its own transaction, and an open past the gap limit is a paid entry the house co-signs. Left: more than one house
  portfolio, so that trades do not wait for each other (finding 13) and paid entries are signed by a key that holds
  only their capital (finding 14), and each market's fee ceiling (finding 15).
- A listing floor: only coins whose pools meet a pump with sellers. Run 4's half-life of 120 s and run 5's 150M TMEME
  of holder supply were assumptions. The first mainnet reading (findings 8 and 9) rules out coins hours old; for the
  rest, each coin's pump and fade is measured from its own candles before it is listed, and its open interest cap
  kept below what absorbing that selling would cost. How much is sold, not only how fast, still needs the pool's trades.
- Backing set a year out from the first top-up, what happens to the positions on a side whose backing lapses anyway
  (domain 1 on devnet, at slot 527,453,036), and the house's exposure to the backing it provides.
