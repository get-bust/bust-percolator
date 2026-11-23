// A recorded run as one JSON file for charts: node scripts/export-run.cjs <tag> <out.json>
// Ticks: wall-clock ms, the pool's price and the effective mark (USDC per 1M coins). Events: open, pump, close, dump.
// The pool column comes from the two transactions that moved the pool (the buy and the sell), at the times they
// confirmed: the recorder reads the pool at the start of a tick and stamps the tick at its end, a few seconds later.
// Profit: the trader's PnL after the close. Cost: the house's collateral round trip through the pool, read from the buy
// and sell transactions' token balances.
const fs = require("fs");
const { connection: c, house, collateral, statePath } = require("./common.cjs");

const [tag, out] = process.argv.slice(2);
if (!tag || !out) throw new Error("usage: export-run.cjs <tag> <out.json>");

(async () => {
  const ticks = fs.readFileSync(statePath(`run-${tag}.jsonl`), "utf8").trim().split("\n").map((l) => JSON.parse(l));
  const ev = Object.fromEntries(JSON.parse(fs.readFileSync(statePath(`run-${tag}-events.json`), "utf8")).map((e) => [e.name, e]));
  const sig = (lines) => /sig (\w+)/.exec(lines.join(" "))[1];
  const delta = async (s) => {
    const t = await c.getTransaction(s, { maxSupportedTransactionVersion: 0, commitment: "confirmed" });
    const mine = (bs) => bs.filter((b) => b.mint === collateral.toBase58() && b.owner === house.publicKey.toBase58()).reduce((x, b) => x + Number(b.uiTokenAmount.amount), 0);
    return (mine(t.meta.postTokenBalances) - mine(t.meta.preTokenBalances)) / 1e6;
  };
  const buy = await delta(sig(ev.pump.out));
  const sell = await delta(sig(ev.dump.out));
  // the client's output for a trade: an optional quote line, the trade line, then the market and both portfolios
  const lines = (e) => e.out ?? [e.line, ...(e.after ?? [])];
  const tradeLine = (e) => lines(e).find((l) => l.startsWith("trade "));
  const profit = Number(/pnl \$?(-?[\d.]+)/.exec(lines(ev.close).find((l) => l.trim().startsWith("trader")))[1]);
  // a run recorded before the full output was kept has only the quote line for its open: its mark is the entry
  const entry = tradeLine(ev.open) ? Number(/at ([\d.]+)/.exec(tradeLine(ev.open))[1]) : Number(/mark ([\d.]+)/.exec(lines(ev.open)[0])[1]);
  const closePrice = Number(/at ([\d.]+)/.exec(tradeLine(ev.close))[1]);

  const pool0 = ticks[0].spot;
  const pool1 = Number(/-> ([\d.]+)/.exec(ev.pump.out[1])[1]);
  const pool2 = Number(/-> ([\d.]+)/.exec(ev.dump.out[1])[1]);
  const seen = [...new Set(ticks.map((k) => k.spot.toFixed(4)))];
  if (!seen.every((v) => [pool0, pool1, pool2].some((p) => Math.abs(p - Number(v)) < 1e-4))) throw new Error(`the pool moved outside the run: ${seen}`);
  const before = (ms) => ticks.filter((k) => k.ms <= ms).at(-1).effective;
  const series = [
    ...ticks.map((k) => ({ ms: k.ms, effective: k.effective })),
    { ms: ev.pump.ms, effective: before(ev.pump.ms), at: "buy confirmed" },
    { ms: ev.dump.ms, effective: before(ev.dump.ms), at: "sell confirmed" },
  ]
    .sort((a, b) => a.ms - b.ms)
    .map((k) => ({ ...k, spot: k.ms < ev.pump.ms ? pool0 : k.ms < ev.dump.ms ? pool1 : pool2 }));

  const data = {
    run: tag,
    ticks: series,
    events: { open: ev.open.ms, pump: ev.pump.ms, close: ev.close.ms, dump: ev.dump.ms },
    entry,
    closePrice,
    poolMove: pool1 / entry - 1,
    perpMove: closePrice / entry - 1,
    profit,
    cost: -(buy + sell),
    sigs: { open: tradeLine(ev.open) ? /: (\w+)$/.exec(tradeLine(ev.open))[1] : null, pump: sig(ev.pump.out), close: /: (\w+)$/.exec(tradeLine(ev.close))[1], dump: sig(ev.dump.out) },
  };
  fs.writeFileSync(out, JSON.stringify(data, null, 1));
  console.log({ ticks: series.length, entry, closePrice, poolMove: data.poolMove, perpMove: data.perpMove, profit, cost: data.cost, net: profit - data.cost });
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
