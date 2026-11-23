// A crowd of holders on the test coin's pool, standing in for what a pump meets on mainnet: when the pool trades above
// its own time-weighted average they sell into it, when it trades below they buy, pulling the price back toward the
// average with a given half-life. On devnet nobody else trades the pool, so without them a pump can be held for free.
// The half-life is an assumption, not a measurement; runs are reported with and without the holders.
//   node scripts/holders.cjs fund                         the house sends the holders 150M TMEME, the mint authority 3,000 USDC
//   node scripts/holders.cjs <tag> [half_life_s=120] [band_bps=200] [window_s=1800]
// Each trade is a JSON line in state/holders-<tag>.jsonl. Runs until it is stopped.
const fs = require("fs");
const { ComputeBudgetProgram, PublicKey, SystemProgram, Transaction, sendAndConfirmTransaction } = require("@solana/web3.js");
const spl = require("@solana/spl-token");
const BN = require("bn.js");
const amm = require("@pump-fun/pump-swap-sdk");
const { connection: c, house, mintAuthority, collateral, readState, statePath } = require("./common.cjs");
const { loadOrNew } = require("./keys.cjs");

const holders = loadOrNew("holders.json");
const pool = readState("pool.json");
const mint = new PublicKey(pool.mint);

async function fund() {
  const from = await spl.getOrCreateAssociatedTokenAccount(c, house, mint, house.publicKey);
  const coin = await spl.getOrCreateAssociatedTokenAccount(c, house, mint, holders.publicKey);
  await spl.transfer(c, house, from.address, coin.address, house, 150_000_000n * 1_000_000n);
  const usdc = await spl.getOrCreateAssociatedTokenAccount(c, house, collateral, holders.publicKey);
  await spl.mintTo(c, house, collateral, usdc.address, mintAuthority, 3_000n * 1_000_000n);
  const sig = await sendAndConfirmTransaction(c, new Transaction().add(SystemProgram.transfer({ fromPubkey: house.publicKey, toPubkey: holders.publicKey, lamports: 200_000_000 })), [house]);
  console.log("holders", holders.publicKey.toBase58(), "funded:", sig);
}

async function reserves() {
  const [b, q] = await Promise.all([c.getTokenAccountBalance(new PublicKey(pool.baseVault)), c.getTokenAccountBalance(new PublicKey(pool.quoteVault))]);
  return { x: Number(b.value.amount), y: Number(q.value.amount) };
}

async function swap(side, raw) {
  const state = await new amm.OnlinePumpAmmSdk(c).swapSolanaState(new PublicKey(pool.pool), holders.publicKey);
  const ixs = side === "sell" ? await amm.PUMP_AMM_SDK.sellBaseInput(state, new BN(raw), 50) : await amm.PUMP_AMM_SDK.buyQuoteInput(state, new BN(raw), 50);
  const tx = new Transaction().add(ComputeBudgetProgram.setComputeUnitLimit({ units: 400_000 }), ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 20_000 }), ...ixs);
  return sendAndConfirmTransaction(c, tx, [holders], { commitment: "confirmed" });
}

async function run(tag, halfLife, bandBps, window) {
  const log = statePath(`holders-${tag}.jsonl`);
  fs.writeFileSync(log, "");
  const r0 = await reserves();
  let seed = r0.y / r0.x;
  const samples = [];
  const twap = (now) => {
    const start = now - window * 1000;
    while (samples.length > 1 && samples[1].t <= start) seed = samples.shift().p;
    let at = start;
    let value = seed;
    let sum = 0;
    for (const s of samples) {
      if (s.t <= start) {
        value = s.p;
        continue;
      }
      sum += value * (s.t - at);
      at = s.t;
      value = s.p;
    }
    return (sum + value * (now - at)) / (window * 1000);
  };
  let last = Date.now();
  for (;;) {
    await new Promise((r) => setTimeout(r, 3000));
    const { x, y } = await reserves();
    const now = Date.now();
    const p = y / x;
    samples.push({ t: now, p });
    const ref = twap(now);
    const k = 1 - Math.pow(0.5, (now - last) / 1000 / halfLife);
    last = now;
    const band = bandBps / 10_000;
    let side = null;
    let raw = 0;
    if (p > ref * (1 + band)) {
      // sell coins until the price has moved k of the way back to the average
      const target = p - (p - ref) * k;
      raw = Math.floor(Math.sqrt((x * y) / target) - x);
      side = "sell";
    } else if (p < ref * (1 - band)) {
      const target = p + (ref - p) * k;
      raw = Math.floor(Math.sqrt(x * y * target) - y);
      side = "buy";
    }
    if (!side || raw <= 0) continue;
    try {
      const sig = await swap(side, raw);
      const after = await reserves();
      const line = { ms: now, side, raw, price: p * 1e6, ref: ref * 1e6, after: (after.y / after.x) * 1e6, sig };
      fs.appendFileSync(log, JSON.stringify(line) + "\n");
      console.log(new Date(now).toISOString(), side, raw, "price", (p * 1e6).toFixed(4), "->", line.after.toFixed(4), "ref", (ref * 1e6).toFixed(4));
    } catch (e) {
      console.log("swap failed:", e.message);
    }
  }
}

const [cmd, ...rest] = process.argv.slice(2);
(cmd === "fund" ? fund() : run(cmd, Number(rest[0] || 120), Number(rest[1] || 200), Number(rest[2] || 1800))).catch((e) => {
  console.error(e);
  process.exit(1);
});
