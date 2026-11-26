// A test coin and its PumpSwap pool against the collateral, so a run can move the price it is marked to.
//   node scripts/pool.cjs setup          mint 1B TMEME, open a TMEME/collateral pool at 4 USDC per 1M TMEME
//   node scripts/pool.cjs price          pool reserves and price
//   node scripts/pool.cjs buy <amount>   spend the quote token (collateral, or SOL) on TMEME (the price goes up)
//   node scripts/pool.cjs sell <tmeme>   sell TMEME for collateral (the price goes down)
//   node scripts/pool.cjs setup-sol <tmeme_millions> <sol>
//                                        open a second pool of the same coin against wrapped SOL, as mainnet pools are
// The house pays and holds the coin; state/pool.json keeps the pool and its two vaults, state/pool-sol.json the SOL
// pool's. POOL=pool-sol.json points price, buy and sell at the SOL pool.
const { Keypair, PublicKey } = require("@solana/web3.js");
const spl = require("@solana/spl-token");
const BN = require("bn.js");
const amm = require("@pump-fun/pump-swap-sdk");
const { connection: c, house, mintAuthority, collateral, readState, writeState, send: sendAs } = require("./common.cjs");

const send = (ixs) => sendAs(ixs, [house]);
const POOL = process.env.POOL || "pool.json";

async function reserves(st) {
  const [b, q] = await Promise.all([c.getTokenAccountBalance(new PublicKey(st.baseVault)), c.getTokenAccountBalance(new PublicKey(st.quoteVault))]);
  const base = Number(b.value.amount);
  const quote = Number(q.value.amount);
  // per 1M coins, in the quote token (USDC, or SOL for the SOL pool)
  const decimals = st.quoteMint === spl.NATIVE_MINT.toBase58() ? 9 : 6;
  return { base, quote, perMillion: (quote / base) * 1e6 * 10 ** (6 - decimals) };
}

async function setup() {
  const mint = await spl.createMint(c, house, house.publicKey, null, 6, Keypair.generate());
  const coin = await spl.getOrCreateAssociatedTokenAccount(c, house, mint, house.publicKey);
  await spl.mintTo(c, house, mint, coin.address, house, 1_000_000_000n * 1_000_000n);
  const usdc = await spl.getOrCreateAssociatedTokenAccount(c, house, collateral, house.publicKey);
  await spl.mintTo(c, house, collateral, usdc.address, mintAuthority, 100_000n * 1_000_000n);
  console.log("TMEME", mint.toBase58());

  const index = 0;
  const state = await new amm.OnlinePumpAmmSdk(c).createPoolSolanaState(index, house.publicKey, mint, collateral, coin.address, usdc.address);
  // 500M TMEME against 2,000 USDC: 4 USDC per 1M TMEME.
  const sig = await send(await amm.PUMP_AMM_SDK.createPoolInstructions(state, new BN("500000000000000"), new BN("2000000000")));
  const pda = amm.poolPda(index, house.publicKey, mint, collateral);
  const pool = Array.isArray(pda) ? pda[0] : pda;
  const data = (await c.getAccountInfo(pool)).data;
  const at = (o) => new PublicKey(data.subarray(o, o + 32)).toBase58();
  // Pool layout: discriminator, bump, index, creator, then base mint, quote mint, LP mint, base vault, quote vault.
  const st = { mint: at(43), quoteMint: at(75), pool: pool.toBase58(), baseVault: at(139), quoteVault: at(171), createSig: sig };
  writeState("pool.json", st);
  console.log("pool", st.pool, await reserves(st));
}

async function setupSol(millions, sol) {
  const coinState = readState("pool.json");
  const mint = new PublicKey(coinState.mint);
  const coin = await spl.getOrCreateAssociatedTokenAccount(c, house, mint, house.publicKey);
  const wsol = await spl.getOrCreateAssociatedTokenAccount(c, house, spl.NATIVE_MINT, house.publicKey);
  const lamports = BigInt(Math.round(Number(sol) * 1e9));
  const { SystemProgram } = require("@solana/web3.js");
  await send([SystemProgram.transfer({ fromPubkey: house.publicKey, toPubkey: wsol.address, lamports }), spl.createSyncNativeInstruction(wsol.address)]);
  const index = 0;
  const state = await new amm.OnlinePumpAmmSdk(c).createPoolSolanaState(index, house.publicKey, mint, spl.NATIVE_MINT, coin.address, wsol.address);
  const base = new BN((BigInt(Math.round(Number(millions) * 1e6)) * 1_000_000n).toString());
  const sig = await send(await amm.PUMP_AMM_SDK.createPoolInstructions(state, base, new BN(lamports.toString())));
  const pda = amm.poolPda(index, house.publicKey, mint, spl.NATIVE_MINT);
  const pool = Array.isArray(pda) ? pda[0] : pda;
  const data = (await c.getAccountInfo(pool)).data;
  const at = (o) => new PublicKey(data.subarray(o, o + 32)).toBase58();
  const st = { mint: at(43), quoteMint: at(75), pool: pool.toBase58(), baseVault: at(139), quoteVault: at(171), createSig: sig };
  writeState("pool-sol.json", st);
  console.log("SOL pool", st.pool, await reserves(st));
}

async function swap(side, amount) {
  const st = readState(POOL);
  const state = await new amm.OnlinePumpAmmSdk(c).swapSolanaState(new PublicKey(st.pool), house.publicKey);
  const before = await reserves(st);
  // a buy spends the quote token (6 decimals, or 9 for SOL), a sell spends TMEME (6 decimals)
  const quoteDecimals = st.quoteMint === spl.NATIVE_MINT.toBase58() ? 9 : 6;
  const raw = new BN(Math.round(Number(amount) * 10 ** (side === "buy" ? quoteDecimals : 6)).toString());
  const ixs = side === "buy" ? await amm.PUMP_AMM_SDK.buyQuoteInput(state, raw, 50) : await amm.PUMP_AMM_SDK.sellBaseInput(state, raw, 50);
  const sig = await send(ixs);
  const after = await reserves(st);
  console.log(side, amount, "sig", sig);
  console.log("price per 1M TMEME", before.perMillion.toFixed(6), "->", after.perMillion.toFixed(6), `(${((after.perMillion / before.perMillion - 1) * 100).toFixed(1)}%)`);
}

(async () => {
  const [cmd, arg] = process.argv.slice(2);
  if (cmd === "setup") await setup();
  else if (cmd === "setup-sol") await setupSol(arg, process.argv[4]);
  else if (cmd === "price") console.log(await reserves(readState(POOL)));
  else if (cmd === "buy" || cmd === "sell") await swap(cmd, arg);
  else console.log("usage: setup | price | buy <usdc> | sell <tmeme>");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
