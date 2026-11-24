// A recorded run as one JSON file for charts: node scripts/export-run.cjs <tag> <out.json>
// Ticks: wall-clock ms, the pool's price and the effective mark (USDC per 1M coins). Events: open, pump, close, dump.
// The pool column comes from the transactions that moved the pool (the buy and the sell, and the holders' trades when
// they ran), at the times they confirmed: the recorder reads the pool at the start of a tick and stamps the tick at
// its end, a few seconds later.
// Profit: the trader's equity (capital and PnL) after the close against its equity before the open, so a close that
// converted its profit counts the same as one that left it as PnL, and a fee paid at the open counts against it. Cost: the house's collateral round trip through the pool, read from the buy
// and sell transactions' token balances, plus what the attacker spent keeping the pump (state/defend-<tag>.jsonl).
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
  const equity = (e) => {
    const m = /capital (-?[\d.]+) pnl (-?[\d.]+)/.exec(lines(e).find((l) => l.trim().startsWith("trader")));
    return Number(m[1]) + Number(m[2]);
  };
  const openFeeBps = Number(/fee (\d+) bps/.exec(lines(ev.open).find((l) => l.startsWith("house quote")) ?? "fee 0 bps")[1]);
  // a run recorded before the full output was kept has only the quote line for its open: its mark is the entry
  const entry = tradeLine(ev.open) ? Number(/at ([\d.]+)/.exec(tradeLine(ev.open))[1]) : Number(/mark ([\d.]+)/.exec(lines(ev.open)[0])[1]);
  const closePrice = Number(/at ([\d.]+)/.exec(tradeLine(ev.close))[1]);
  const contracts = Math.abs(Number(/trade (-?[\d.]+) contracts/.exec(tradeLine(ev.close))[1]));
  const openFee = (contracts * entry * openFeeBps) / 10_000;
  const profit = equity(ev.close) - (equity(ev.open) + openFee);

  // Every move of the pool in a run, at its confirmation time: the attacker's buy and sell, and the holders' trades
  // when they ran (state/holders-<tag>.jsonl).
  const holdersFile = statePath(`holders-${tag}.jsonl`);
  const holders = fs.existsSync(holdersFile)
    ? fs.readFileSync(holdersFile, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l))
    : [];
  // and the attacker's buys that kept the pump through the hold (state/defend-<tag>.jsonl), when it was defended
  const defendFile = statePath(`defend-${tag}.jsonl`);
  const defended = fs.existsSync(defendFile)
    ? fs.readFileSync(defendFile, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l))
    : [];
  const moves = [
    { ms: ev.pump.ms, price: Number(/-> ([\d.]+)/.exec(ev.pump.out[1])[1]), at: "buy confirmed" },
    { ms: ev.dump.ms, price: Number(/-> ([\d.]+)/.exec(ev.dump.out[1])[1]), at: "sell confirmed" },
    ...holders.map((h) => ({ ms: h.ms, price: h.after, at: `holders ${h.side}` })),
    ...defended.map((d) => ({ ms: d.ms, price: d.after, at: "attacker buys back" })),
  ].sort((a, b) => a.ms - b.ms);
  const pool0 = ticks[0].spot;
  const poolAt = (ms) => moves.filter((m) => m.ms <= ms).at(-1)?.price ?? pool0;
  if (!holders.length) {
    const seen = [...new Set(ticks.map((k) => k.spot.toFixed(4)))];
    if (!seen.every((v) => [pool0, ...moves.map((m) => m.price)].some((p) => Math.abs(p - Number(v)) < 1e-4))) throw new Error(`the pool moved outside the run: ${seen}`);
  }
  const before = (ms) => ticks.filter((k) => k.ms <= ms).at(-1).effective;
  const series = [...ticks.map((k) => ({ ms: k.ms, effective: k.effective })), ...moves.map((m) => ({ ms: m.ms, effective: before(m.ms), at: m.at }))]
    .sort((a, b) => a.ms - b.ms)
    .map((k) => ({ ...k, spot: poolAt(k.ms) }));
  const pool1 = moves[0].price;
  const holdersSold = holders.filter((h) => h.side === "sell").length;
  const holdersBought = holders.filter((h) => h.side === "buy").length;
  const defendSpent = -defended.reduce((s, d) => s + d.usdc, 0);

  const data = {
    run: tag,
    ticks: series,
    events: { open: ev.open.ms, pump: ev.pump.ms, close: ev.close.ms, dump: ev.dump.ms },
    entry,
    closePrice,
    poolMove: pool1 / entry - 1,
    perpMove: closePrice / entry - 1,
    profit,
    openFee,
    cost: -(buy + sell) + defendSpent,
    holders: holders.length ? { trades: holders.length, sold: holdersSold, bought: holdersBought } : null,
    defended: defended.length ? { buys: defended.length, usdc: defendSpent } : null,
    sigs: { open: tradeLine(ev.open) ? /: (\w+)$/.exec(tradeLine(ev.open))[1] : null, pump: sig(ev.pump.out), close: /: (\w+)$/.exec(tradeLine(ev.close))[1], dump: sig(ev.dump.out) },
  };
  fs.writeFileSync(out, JSON.stringify(data, null, 1));
  console.log({ ticks: series.length, entry, closePrice, poolMove: data.poolMove, perpMove: data.perpMove, profit, cost: data.cost, net: profit - data.cost });
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
