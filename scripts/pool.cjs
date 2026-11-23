// A test coin and its PumpSwap pool against the collateral, so a run can move the price it is marked to.
//   node scripts/pool.cjs setup          mint 1B TMEME, open a TMEME/collateral pool at 4 USDC per 1M TMEME
//   node scripts/pool.cjs price          pool reserves and price
//   node scripts/pool.cjs buy <usdc>     spend collateral on TMEME (the price goes up)
//   node scripts/pool.cjs sell <tmeme>   sell TMEME for collateral (the price goes down)
// The house pays and holds the coin; state/pool.json keeps the pool and its two vaults.
const { ComputeBudgetProgram, Keypair, PublicKey, Transaction, sendAndConfirmTransaction } = require("@solana/web3.js");
const spl = require("@solana/spl-token");
const BN = require("bn.js");
const amm = require("@pump-fun/pump-swap-sdk");
const { connection: c, house, mintAuthority, collateral, readState, writeState } = require("./common.cjs");

const send = (ixs, signers = [house]) =>
  sendAndConfirmTransaction(
    c,
    new Transaction().add(ComputeBudgetProgram.setComputeUnitLimit({ units: 400_000 }), ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 20_000 }), ...ixs),
    signers,
    { commitment: "confirmed" },
  );

async function reserves(st) {
  const [b, q] = await Promise.all([c.getTokenAccountBalance(new PublicKey(st.baseVault)), c.getTokenAccountBalance(new PublicKey(st.quoteVault))]);
  const base = Number(b.value.amount);
  const quote = Number(q.value.amount);
  return { base, quote, usdPerMillion: (quote / base) * 1e6 };
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

async function swap(side, amount) {
  const st = readState("pool.json");
  const state = await new amm.OnlinePumpAmmSdk(c).swapSolanaState(new PublicKey(st.pool), house.publicKey);
  const before = await reserves(st);
  const raw = new BN(Math.round(Number(amount) * 1e6).toString());
  const ixs = side === "buy" ? await amm.PUMP_AMM_SDK.buyQuoteInput(state, raw, 50) : await amm.PUMP_AMM_SDK.sellBaseInput(state, raw, 50);
  const sig = await send(ixs);
  const after = await reserves(st);
  console.log(side, amount, "sig", sig);
  console.log("price $/1M TMEME", before.usdPerMillion.toFixed(4), "->", after.usdPerMillion.toFixed(4), `(${((after.usdPerMillion / before.usdPerMillion - 1) * 100).toFixed(1)}%)`);
}

(async () => {
  const [cmd, arg] = process.argv.slice(2);
  if (cmd === "setup") await setup();
  else if (cmd === "price") console.log(await reserves(readState("pool.json")));
  else if (cmd === "buy" || cmd === "sell") await swap(cmd, arg);
  else console.log("usage: setup | price | buy <usdc> | sell <tmeme>");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
