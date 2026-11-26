// How real graduated Pump coins' pools answer a pump, read from mainnet: the measurement behind a listing floor.
//   node scripts/measure-pools.cjs [--pages 3] [--jump 10] [--min-age-days 0] [<pool> ...]
// With no pools given it takes PumpSwap's busiest pools from GeckoTerminal (--pages of 20, by 24 h volume), keeping
// those opened at least --min-age-days ago: the busiest pools are mostly coins launched hours before. For each
// pool it reads the last 1,000 one-minute candles and finds every pump: a minute whose high is at least --jump percent
// above the lowest low of that minute and the one before it. For each pump it measures how many minutes the pool takes
// to give back half the rise (the holders' half-life that runs 4 and 5 had to assume), and whether it gives it back at
// all inside the candles read. Minute candles cannot see what happens inside a minute, so a half-life under a minute
// reads as one minute.
// Source: GeckoTerminal's public API (no key, about 30 requests a minute), so the script waits between requests.
const args = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = args.indexOf(`--${name}`);
  return i >= 0 ? Number(args.splice(i, 2)[1]) : fallback;
};
const PAGES = opt("pages", 3);
const JUMP = opt("jump", 10) / 100;
const MIN_AGE_DAYS = opt("min-age-days", 0);
const API = "https://api.geckoterminal.com/api/v2/networks/solana";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const median = (xs) => (xs.length ? [...xs].sort((a, b) => a - b)[Math.floor(xs.length / 2)] : null);

async function get(path) {
  for (let attempt = 0; attempt < 5; attempt++) {
    await sleep(2200);
    const res = await fetch(`${API}${path}`, { headers: { accept: "application/json" } });
    if (res.status === 429) {
      await sleep(15000);
      continue;
    }
    if (res.ok) return res.json();
    throw new Error(`${path}: ${res.status}`);
  }
  throw new Error(`${path}: rate limited`);
}

async function busiestPools() {
  const out = [];
  for (let page = 1; page <= PAGES; page++) {
    let data;
    try {
      ({ data } = await get(`/dexes/pumpswap/pools?page=${page}&sort=h24_volume_usd_desc`));
    } catch (e) {
      console.error(`page ${page}: ${String(e).slice(0, 80)}`);
      break;
    }
    for (const p of data) {
      const ageDays = (Date.now() - Date.parse(p.attributes.pool_created_at)) / 86_400_000;
      if (ageDays >= MIN_AGE_DAYS) out.push({ pool: p.attributes.address, name: p.attributes.name, volume24h: Number(p.attributes.volume_usd.h24), ageDays: +ageDays.toFixed(1) });
    }
  }
  return out;
}

function pumps(candles) {
  // candles: [t, open, high, low, close, volume], oldest first
  const found = [];
  for (let i = 1; i < candles.length; i++) {
    const low = Math.min(candles[i - 1][3], candles[i][3]);
    const high = candles[i][2];
    if (high < low * (1 + JUMP)) continue;
    const half = high - (high - low) / 2;
    const back = candles.slice(i + 1).findIndex((c) => c[3] <= half);
    found.push({ rise: high / low - 1, halfBackMin: back >= 0 ? back + 1 : null });
    i += 5; // one pump is counted once
  }
  return found;
}

(async () => {
  const pools = args.length ? args.map((pool) => ({ pool, name: "", volume24h: null })) : await busiestPools();
  for (const { pool, name, volume24h, ageDays } of pools) {
    try {
      const ohlcv = (await get(`/pools/${pool}/ohlcv/minute?aggregate=1&limit=1000`)).data.attributes.ohlcv_list;
      const candles = [...ohlcv].sort((a, b) => a[0] - b[0]);
      const ps = pumps(candles);
      const halves = ps.map((x) => x.halfBackMin).filter((x) => x !== null);
      const traded = candles.filter((c) => c[5] > 0).length;
      console.log(
        JSON.stringify({
          pool,
          name,
          volume24h: volume24h && Math.round(volume24h),
          ageDays,
          minutes: candles.length,
          minutesWithTrades: traded,
          pumps: ps.length,
          medianRisePct: ps.length ? +(median(ps.map((x) => x.rise)) * 100).toFixed(1) : null,
          medianHalfBackMin: median(halves),
          halfBackWithin2Min: halves.filter((x) => x <= 2).length,
          neverHalfBack: ps.length - halves.length,
        }),
      );
    } catch (e) {
      console.log(JSON.stringify({ pool, name, error: String(e).slice(0, 120) }));
    }
  }
})();
