// The attacker who holds the pump instead of leaving it: while the long is open, the pool is bought back to the pumped
// price whenever it slips below it, so every coin the holders sell into the pump is bought again by the attacker.
// The house pays, as it does for the pump.
//   node scripts/defend.cjs <tag> <target_usd_per_1M> [band_bps=50]
// Each buy is a JSON line in state/defend-<tag>.jsonl: the collateral it spent and the coins it got, read from the
// transaction's token balances, and the pool's price before and after. Runs until it is stopped.
const fs = require("fs");
const { PublicKey } = require("@solana/web3.js");
const BN = require("bn.js");
const amm = require("@pump-fun/pump-swap-sdk");
const { connection: c, house, collateral, readState, statePath, send } = require("./common.cjs");

const pool = readState("pool.json");

async function reserves() {
  const [b, q] = await Promise.all([c.getTokenAccountBalance(new PublicKey(pool.baseVault)), c.getTokenAccountBalance(new PublicKey(pool.quoteVault))]);
  return { x: Number(b.value.amount), y: Number(q.value.amount) };
}

// The house's change in a mint across a confirmed transaction, in whole tokens
async function houseDelta(sig, mint) {
  const tx = await c.getTransaction(sig, { maxSupportedTransactionVersion: 0, commitment: "confirmed" });
  const mine = (bs) => (bs || []).filter((b) => b.mint === mint && b.owner === house.publicKey.toBase58()).reduce((s, b) => s + Number(b.uiTokenAmount.amount), 0);
  return (mine(tx.meta.postTokenBalances) - mine(tx.meta.preTokenBalances)) / 1e6;
}

// A stop asked for mid-buy lets the buy land and be logged first: the attacker sells back every coin in the log
let stopping = false;
process.on("SIGTERM", () => (stopping = true));

async function run(tag, target, bandBps) {
  const log = statePath(`defend-${tag}.jsonl`);
  fs.writeFileSync(log, "");
  const goal = target / 1e6;
  while (!stopping) {
    await new Promise((r) => setTimeout(r, 3000));
    if (stopping) break;
    const { x, y } = await reserves();
    const p = y / x;
    if (p >= goal * (1 - bandBps / 10_000)) continue;
    // collateral in until the price is back at the target (constant product, before fees)
    const raw = Math.floor(Math.sqrt(x * y * goal) - y);
    if (raw <= 0) continue;
    try {
      const state = await new amm.OnlinePumpAmmSdk(c).swapSolanaState(new PublicKey(pool.pool), house.publicKey);
      const sig = await send(await amm.PUMP_AMM_SDK.buyQuoteInput(state, new BN(raw), 50), [house]);
      const [usdc, coins, after] = await Promise.all([houseDelta(sig, collateral.toBase58()), houseDelta(sig, pool.mint), reserves()]);
      const line = { ms: Date.now(), usdc, coins, price: p * 1e6, after: (after.y / after.x) * 1e6, sig };
      fs.appendFileSync(log, JSON.stringify(line) + "\n");
      console.log(new Date(line.ms).toISOString(), "bought back", coins.toFixed(0), "for", (-usdc).toFixed(2), "price", line.price.toFixed(4), "->", line.after.toFixed(4));
    } catch (e) {
      console.log("buy failed:", e.message);
    }
  }
}

const [tag, target, band] = process.argv.slice(2);
if (!tag || !target) throw new Error("usage: defend.cjs <tag> <target_usd_per_1M> [band_bps=50]");
run(tag, Number(target), Number(band || 50)).then(() => process.exit(0)).catch((e) => {
  console.error(e);
  process.exit(1);
});
