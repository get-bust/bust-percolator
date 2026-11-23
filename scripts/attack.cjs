// A recorded manipulation run: the trader holds a long while the house pumps the pool it is marked to.
//   node scripts/attack.cjs <tag> [pump_usdc=1000] [hold_secs=120] [contracts=200]
// Starts `perc record`, waits until the mark sits on the pool, then: opens the long, pumps the pool, holds it while the
// recorder cranks every tick, closes the long and sells the bought coins back, and records 20 s more.
// Ticks go to state/run-<tag>.jsonl, events to state/run-<tag>-events.json. Start from a fresh trader (perc newtrader).
const { spawn, execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const { ROOT, statePath } = require("./common.cjs");

const [tag, pump = "1000", hold = "120", contracts = "200"] = process.argv.slice(2);
if (!tag) throw new Error("usage: attack.cjs <tag> [pump_usdc] [hold_secs] [contracts]");
const PERC = path.join(ROOT, "client/target/release/perc");
const POOL = path.join(__dirname, "pool.cjs");
const TICKS = statePath(`run-${tag}.jsonl`);
const EVENTS = statePath(`run-${tag}-events.json`);

const events = [];
const mark = (name, extra = {}) => {
  const e = { ms: Date.now(), name, ...extra };
  events.push(e);
  fs.writeFileSync(EVENTS, JSON.stringify(events, null, 2));
  console.log(new Date(e.ms).toISOString(), name, JSON.stringify(extra));
};
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const run = (cmd, args) => execFileSync(cmd, args, { encoding: "utf8", cwd: ROOT });
const last = () => {
  const lines = fs.readFileSync(TICKS, "utf8").trim().split("\n").filter(Boolean);
  return lines.length ? JSON.parse(lines.at(-1)) : null;
};
const poolBase = () => Number(/base: (\d+)/.exec(run("node", [POOL, "price"]))[1]);

(async () => {
  fs.mkdirSync(path.dirname(TICKS), { recursive: true });
  fs.writeFileSync(TICKS, "");
  const recorder = spawn(PERC, ["record", "60", "900"], { cwd: ROOT, stdio: ["ignore", fs.openSync(TICKS, "a"), "inherit"] });
  mark("recorder", { pump: Number(pump), hold: Number(hold), contracts: Number(contracts) });

  // the target and the effective mark within 0.3% of the pool for 10 s
  let since = 0;
  for (;;) {
    await sleep(1000);
    const t = last();
    if (!t) continue;
    const near = Math.abs(t.effective / t.spot - 1) < 0.003 && Math.abs(t.target / t.spot - 1) < 0.003;
    since = near ? since || Date.now() : 0;
    if (since && Date.now() - since > 10_000) break;
  }
  mark("converged", { effective: last().effective, spot: last().spot });
  await sleep(10_000);

  const open = run(PERC, ["trade", contracts]);
  mark("open", { line: open.split("\n")[0] });
  await sleep(15_000);

  const before = poolBase();
  const buy = run("node", [POOL, "buy", pump]);
  const bought = (before - poolBase()) / 1e6;
  mark("pump", { out: buy.trim().split("\n"), bought });
  await sleep(Number(hold) * 1000);

  const close = run(PERC, ["trade", `-${contracts}`]);
  mark("close", { line: close.split("\n")[0], after: close.split("\n").slice(1, 4) });
  const sell = run("node", [POOL, "sell", String(bought)]);
  mark("dump", { out: sell.trim().split("\n") });

  await sleep(20_000);
  recorder.kill();
  mark("done");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
