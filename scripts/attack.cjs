// A recorded manipulation run: the trader holds a long while the house pumps the pool it is marked to.
//   node scripts/attack.cjs <tag> [--pump 1000] [--hold 120] [--contracts 200] [--window 60] [--holders <half_life_s>]
//                                 [--defend <band_bps>]
// Starts `perc record` (pushing the pool's average over --window seconds), and the holders when --holders is given,
// waits until the mark sits on the pool, then: opens the long, pumps the pool with --pump USDC, holds it --hold
// seconds while the recorder cranks every tick, closes the long and sells the bought coins back, and records 20 s more.
// With --defend the attacker keeps the pump through the hold (scripts/defend.cjs): the pool is bought back to the
// pumped price whenever it slips more than band_bps below it, and those coins are sold back with the pump's.
// Ticks go to state/run-<tag>.jsonl, events to state/run-<tag>-events.json. Start from a fresh trader (perc newtrader).
const { spawn, execFileSync } = require("child_process");
const fs = require("fs");
const path = require("path");
const { ROOT, statePath } = require("./common.cjs");

const [tag, ...flags] = process.argv.slice(2);
if (!tag || tag.startsWith("--")) throw new Error("usage: attack.cjs <tag> [--pump usdc] [--hold s] [--contracts n] [--window s] [--holders half_life_s] [--defend band_bps]");
const opt = (name, fallback) => {
  const i = flags.indexOf(`--${name}`);
  return i >= 0 ? flags[i + 1] : fallback;
};
const pump = opt("pump", "1000");
const hold = opt("hold", "120");
const contracts = opt("contracts", "200");
const window = opt("window", "60");
const holdersHalfLife = opt("holders", null);
const defendBand = opt("defend", null);
const PERC = path.join(ROOT, "client/target/release/perc");
const POOL = path.join(__dirname, "pool.cjs");
const DEFENDED = statePath(`defend-${tag}.jsonl`);
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
// A step still running after three minutes stops the run: a hold that stretches on is a different run.
const run = (cmd, args) => execFileSync(cmd, args, { encoding: "utf8", cwd: ROOT, timeout: 180_000 });
const last = () => {
  const lines = fs.readFileSync(TICKS, "utf8").trim().split("\n").filter(Boolean);
  return lines.length ? JSON.parse(lines.at(-1)) : null;
};
const poolBase = () => Number(/base: (\d+)/.exec(run("node", [POOL, "price"]))[1]);

(async () => {
  fs.mkdirSync(path.dirname(TICKS), { recursive: true });
  fs.writeFileSync(TICKS, "");
  const recorder = spawn(PERC, ["record", window, "3600"], { cwd: ROOT, stdio: ["ignore", fs.openSync(TICKS, "a"), "inherit"] });
  const holders = holdersHalfLife
    ? spawn("node", [path.join(__dirname, "holders.cjs"), tag, holdersHalfLife, "200", window], { cwd: ROOT, stdio: ["ignore", fs.openSync(statePath(`holders-${tag}.log`), "a"), "inherit"] })
    : null;
  mark("recorder", { pump: Number(pump), hold: Number(hold), contracts: Number(contracts), window: Number(window), holders: holdersHalfLife && Number(holdersHalfLife), defend: defendBand && Number(defendBand) });

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
  mark("open", { out: open.trim().split("\n") });
  await sleep(15_000);

  const before = poolBase();
  const buy = run("node", [POOL, "buy", pump]);
  const bought = (before - poolBase()) / 1e6;
  mark("pump", { out: buy.trim().split("\n"), bought });
  const defender = defendBand
    ? spawn("node", [path.join(__dirname, "defend.cjs"), tag, /-> ([\d.]+)/.exec(buy)[1], defendBand], { cwd: ROOT, stdio: ["ignore", fs.openSync(statePath(`defend-${tag}.log`), "a"), "inherit"] })
    : null;
  await sleep(Number(hold) * 1000);

  let defended = 0;
  if (defender) {
    // a buy in flight lands and is logged before the defender exits
    await new Promise((r) => (defender.on("exit", r), defender.kill("SIGTERM")));
    const buys = fs.readFileSync(DEFENDED, "utf8").trim().split("\n").filter(Boolean).map((l) => JSON.parse(l));
    defended = buys.reduce((s, b) => s + b.coins, 0);
    mark("defended", { buys: buys.length, usdc: buys.reduce((s, b) => s + b.usdc, 0), coins: defended });
  }

  const close = run(PERC, ["trade", `-${contracts}`]);
  mark("close", { out: close.trim().split("\n") });
  const sell = run("node", [POOL, "sell", String(bought + defended)]);
  mark("dump", { out: sell.trim().split("\n") });

  await sleep(20_000);
  recorder.kill();
  holders?.kill();
  mark("done");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
