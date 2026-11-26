// A Raydium CPMM pool of the test coin against wrapped SOL, the pool kind coins from Raydium's launchpad go to.
//   node scripts/cpmm.cjs setup <tmeme_millions> <sol>   open the pool (state/pool-cpmm.json)
//   node scripts/cpmm.cjs price
//   node scripts/cpmm.cjs buy <sol>                     spend SOL on TMEME (the price goes up)
//   node scripts/cpmm.cjs sell <tmeme>                  sell TMEME for SOL (the price goes down)
// The pool file names the coin's vault as baseVault and SOL's as quoteVault, whichever order the pool keeps them in.
const { PublicKey } = require("@solana/web3.js");
const spl = require("@solana/spl-token");
const BN = require("bn.js");
const { Raydium, TxVersion, DEVNET_PROGRAM_ID, getCpmmPdaAmmConfigId, CurveCalculator } = require("@raydium-io/raydium-sdk-v2");
const { connection: c, house, readState, writeState, send: sendAs } = require("./common.cjs");

const FILE = "pool-cpmm.json";
const PROGRAM = DEVNET_PROGRAM_ID.CREATE_CPMM_POOL_PROGRAM;

async function load() {
  return Raydium.load({ owner: house, connection: c, cluster: "devnet", disableFeatureCheck: true, disableLoadToken: true, blockhashCommitment: "confirmed" });
}

// Sends what the SDK built through our own sender, which confirms over HTTP.
async function run(built) {
  return sendAs(built.transaction.instructions, [house, ...(built.signers || [])], 600_000);
}

async function reserves(st) {
  const d = (await c.getAccountInfo(new PublicKey(st.pool))).data;
  const u = (o) => d.readBigUInt64LE(o);
  const [v0, v1] = await Promise.all([st.vault0, st.vault1].map((v) => c.getTokenAccountBalance(new PublicKey(v))));
  const r0 = BigInt(v0.value.amount) - (u(341) + u(357) + u(397));
  const r1 = BigInt(v1.value.amount) - (u(349) + u(365) + u(405));
  const [base, quote] = st.baseSide === 0 ? [r0, r1] : [r1, r0];
  return { base: Number(base), quote: Number(quote), solPerMillion: (Number(quote) / 1e9) / (Number(base) / 1e6) * 1e6 };
}

async function setup(millions, sol) {
  const coin = readState("pool.json").mint;
  const raydium = await load();
  const configs = await raydium.api.getCpmmConfigs();
  configs.forEach((cfg) => (cfg.id = getCpmmPdaAmmConfigId(PROGRAM, cfg.index).publicKey.toBase58()));
  const mintA = { address: coin, programId: spl.TOKEN_PROGRAM_ID.toBase58(), decimals: 6 };
  const mintB = { address: spl.NATIVE_MINT.toBase58(), programId: spl.TOKEN_PROGRAM_ID.toBase58(), decimals: 9 };
  const built = await raydium.cpmm.createPool({
    programId: PROGRAM,
    poolFeeAccount: DEVNET_PROGRAM_ID.CREATE_CPMM_POOL_FEE_ACC,
    mintA,
    mintB,
    mintAAmount: new BN((BigInt(Math.round(Number(millions) * 1e6)) * 1_000_000n).toString()),
    mintBAmount: new BN(Math.round(Number(sol) * 1e9).toString()),
    startTime: new BN(0),
    feeConfig: configs[0],
    associatedOnly: false,
    ownerInfo: { useSOLBalance: true },
    txVersion: TxVersion.LEGACY,
  });
  const sig = await run(built);
  const pool = built.extInfo.address.poolId.toBase58();
  const d = (await c.getAccountInfo(new PublicKey(pool))).data;
  const at = (o) => new PublicKey(d.subarray(o, o + 32)).toBase58();
  const baseSide = at(168) === coin ? 0 : 1;
  const [vault0, vault1] = [at(72), at(104)];
  const st = {
    mint: coin,
    quoteMint: spl.NATIVE_MINT.toBase58(),
    pool,
    vault0,
    vault1,
    baseSide,
    baseVault: baseSide === 0 ? vault0 : vault1,
    quoteVault: baseSide === 0 ? vault1 : vault0,
    createSig: sig,
  };
  writeState(FILE, st);
  console.log("CPMM pool", pool, await reserves(st));
}

async function swap(side, amount) {
  const st = readState(FILE);
  const raydium = await load();
  const { poolInfo, poolKeys, rpcData } = await raydium.cpmm.getPoolInfoFromRpc(st.pool);
  const coinIsA = poolInfo.mintA.address === st.mint;
  // buy: SOL in; sell: the coin in
  const baseIn = side === "buy" ? !coinIsA : coinIsA;
  const input = new BN(Math.round(Number(amount) * (side === "buy" ? 1e9 : 1e6)).toString());
  const cfg = rpcData.configInfo;
  const zero = new BN(0);
  // creator fee mode 0 takes it from the input token; 1 and 2 only from token 0 or token 1
  const mode = Number(rpcData.creatorFeeOn ?? 0);
  const creatorOnInput = mode === 0 || (mode === 1) === baseIn;
  const result = CurveCalculator.swapBaseInput(
    input,
    baseIn ? rpcData.baseReserve : rpcData.quoteReserve,
    baseIn ? rpcData.quoteReserve : rpcData.baseReserve,
    cfg.tradeFeeRate,
    cfg.creatorFeeRate || zero,
    cfg.protocolFeeRate || zero,
    cfg.fundFeeRate || zero,
    creatorOnInput,
  );
  const before = await reserves(st);
  const built = await raydium.cpmm.swap({ poolInfo, poolKeys, inputAmount: input, swapResult: result, slippage: 0.5, baseIn, txVersion: TxVersion.LEGACY });
  const sig = await run(built);
  const after = await reserves(st);
  console.log(side, amount, "sig", sig);
  console.log("SOL per 1M TMEME", before.solPerMillion.toFixed(6), "->", after.solPerMillion.toFixed(6), `(${((after.solPerMillion / before.solPerMillion - 1) * 100).toFixed(1)}%)`);
}

(async () => {
  const [cmd, a, b] = process.argv.slice(2);
  if (cmd === "setup") await setup(a, b);
  else if (cmd === "price") console.log(await reserves(readState(FILE)));
  else if (cmd === "buy" || cmd === "sell") await swap(cmd, a);
  else console.log("usage: setup <tmeme_millions> <sol> | price | buy <sol> | sell <tmeme>");
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
