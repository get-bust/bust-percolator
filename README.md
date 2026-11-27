<h1 align="center">Bust × Percolator</h1>

Leveraged markets on Pump coins, built on [Percolator](https://github.com/aeyakovenko/percolator), the perpetual-futures
risk engine by Anatoly Yakovenko. Each coin gets its own market inside one Percolator market group, with its own
insurance and backing, so a loss in one coin cannot be paid from another.

This repository is a devnet trial. It is not deployed on mainnet and holds no real funds.

## Deployments

| Network | Program id |
|---|---|
| Devnet | [`7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo`](https://explorer.solana.com/address/7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo?cluster=devnet) |
| Devnet, mark feed (`mark/`) | [`DGCkC4YboRrAxFKY5piEgCE9yjT1tCxwzKeMqZ7NCBfo`](https://explorer.solana.com/address/DGCkC4YboRrAxFKY5piEgCE9yjT1tCxwzKeMqZ7NCBfo?cluster=devnet) |
| Devnet, matcher (`matcher/`) | [`7XE8mxQ9UxZ397pR5xrVUjKhv4EeSL3VK9idbfae2yCV`](https://explorer.solana.com/address/7XE8mxQ9UxZ397pR5xrVUjKhv4EeSL3VK9idbfae2yCV?cluster=devnet) |

The devnet program is [percolator-prog](https://github.com/aeyakovenko/percolator-prog) at commit
[`aac89b9`](https://github.com/aeyakovenko/percolator-prog/commit/aac89b9e078696131df2100368d4fd53baec5fac), built
with its `declare_id!` set to the id above and nothing else changed. The build below reproduces the deployed binary
byte for byte (sha256 `760be21accd628704f5c3594b393492c086c63b845b14183e97d8768c38d23b1`).

## Build the program

Percolator is a git submodule pinned to the commit above. With the Solana CLI and platform-tools v1.52 or newer:

```sh
git submodule update --init
scripts/build-program.sh
solana program dump 7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo deployed.so -u devnet
shasum -a 256 deployed.so target/deploy/percolator_prog.so
```

The script builds a clean copy of the submodule's commit under `target/program`, so the submodule itself stays
unchanged.

## Run the client

`client/` is a command-line client for one market with one asset, a perp on a Pump coin. The asset is priced by an
authority-pushed mark (Percolator's AuthMark mode) and collateralised in a devnet test USDC; one contract is
1,000,000 coins and marks are collateral atoms per contract. The house is the market authority, the oracle authority
and the trader's counterparty.

```sh
export HOUSE_KEYPAIR=~/.config/solana/id.json        # pays, and signs as the house
export MINT_AUTHORITY_KEYPAIR=...                     # mint authority of the collateral in devnet.json
cd client && cargo build --release && cd ..
client/target/release/perc setup 4000000              # first mark: 4 USDC per contract
client/target/release/perc trade 200                  # the trader goes long 200 contracts
client/target/release/perc push 4200000               # a new mark
client/target/release/perc crank trader               # move the effective price toward it
client/target/release/perc show
```

The list of commands is at the top of [`client/src/main.rs`](client/src/main.rs). The client writes the keypairs it
creates to `keys/` and the addresses of the market to `state/perc.json`; neither is committed.

## The mark feed

`mark/` is a small program of our own that marks an asset to its pool: PumpSwap, Raydium CPMM or Raydium AMM v4. Its feed
account holds the pool's two vaults, read from the pool account when the feed is made, and a ring of observations. Anyone may call `Update` once
the interval has passed: the program reads both vaults, adds an observation and pushes the pool's time-weighted average
to Percolator as the asset's authority mark, signed by the feed's oracle PDA. The caller picks only the moment, never
the price. A pool quoted in SOL is priced in dollars at each sample with Pyth's SOL/USD. A sample may move the price the feed holds by at most `max_step_bps` from the one before, so pushing the
pool and sampling it in one transaction moves the average one step.

```sh
cd mark && cargo test && cargo-build-sbf && cd ..
client/target/release/perc feed init 1800 10 100   # 30-minute average, an update at most every 10 s, 1% per step
                                                   # (add pool-sol.json for the SOL pool of scripts/pool.cjs setup-sol)
client/target/release/perc feed take               # the feed's PDA takes the oracle role from the house
client/target/release/perc feed run 10             # call it every 10 s (perc feed release gives the role back)
client/target/release/perc probe <rpc> <pool> <quote_mint>   # read any cluster's pool with the feed's layouts
```

## The matcher

`matcher/` holds the house's quoting rules on chain, for Percolator's `TradeCpi`: the trader signs alone, and the house
takes no part in a trade once it has enabled the matcher on its portfolio. Percolator calls a matcher with only the
request and keeps its own accounts away from it, so the trader's transaction runs the matcher's `Prepare` right before
`TradeCpi`. `Prepare` reads the market's open interest, the trader's position (as the engine counts it after ADL) and
the pool, checks through the instructions sysvar that the next instruction is a `TradeCpi` for that market, trader and
house portfolio, and leaves what it read in the matcher's context. When Percolator calls the matcher, it checks that
the running instruction is that `TradeCpi`, at the top level, right after its `Prepare`, and fills by the rules:

- the part of a request that closes the trader's position is always filled;
- the part that opens is filled only while the pool can be read, while the pool and the mark agree within the gap
  limit on the side that would gain as the mark catches up, and up to the open interest cap (a share of the pool's
  coin reserve, valued at the lower of the mark and the pool price). What fits under the cap is a partial fill.

`matcher/src/layout.rs` reads Percolator's accounts at fixed offsets; its tests hold them to Percolator's types and to
its own readers on devnet accounts.

```sh
cd matcher && cargo test && cargo-build-sbf && cd ..
client/target/release/perc matcher init 500 100     # the house: open interest up to 5% of the pool, a 1% gap limit
client/target/release/perc matcher trade -2         # the trader alone; add `update` for a feed update in the same transaction
client/target/release/perc matcher attack           # simulate the ways around the snapshot; each is refused
client/target/release/perc order 1 100              # the app's path: the matcher, then a paid entry of at most 100 bps
```

`perc order` places an order the way the app does. If the matcher refuses an open because the pool has run past the
mark, the house quotes a paid entry (`rules::paid_entry`): it co-signs a `TradeNoCpi` that carries the gap as its fee,
so the trader enters as if at the pool's price, and the trader caps the fee it accepts. Percolator revokes the
matcher on a house portfolio whenever that portfolio trades outside it, so a trade the house signs enables the matcher
again in the same transaction.

## Scripts

`scripts/` holds the devnet tooling around the client (Node 20+, `pnpm install`):

| Script | Does |
|---|---|
| `pool.cjs` | Mints a test coin and opens its PumpSwap pool against the collateral; `buy` and `sell` move its price |
| `attack.cjs` | A recorded manipulation run: opens a long, pumps the pool, holds it, closes and sells back |
| `holders.cjs` | Holders who sell into a pump and buy back a dip, pulling the pool toward its average |
| `defend.cjs` | The attacker's buy-backs that hold a pump against the holders (`attack.cjs --defend`) |
| `export-run.cjs` | A recorded run as one JSON file for charts |
| `cpmm.cjs` | A Raydium CPMM pool of the test coin against SOL, for the mark feed; `buy` and `sell` move its price |

## Devnet results

Five manipulation runs against a 2,000 USDC pool, each pumping it 124% with 1,000 USDC. With only the engine's price
cap (runs 1 and 2, a trader long 200 contracts) the attacker came out 162.66 and 294.60 USDC ahead. With the house's
defenses (backing, an open interest cap at 5% of the pool's depth, a gap fee, a 30-minute average) the trader could
open 24 contracts: with nobody trading against the pump the attacker still came out 114.44 USDC ahead (run 3), with
holders selling into it the attacker lost 237.16 USDC (run 4), and buying back everything the holders sold to hold the
pump through the whole average lost 737.45 USDC (run 5). The holders' pace and supply are assumptions. Details,
transactions and what is left before mainnet: [docs/devnet-runs.md](docs/devnet-runs.md).

## License

The code in this repository is licensed under the [Business Source License 1.1](LICENSE). Percolator is licensed
under Apache-2.0 by its authors.
