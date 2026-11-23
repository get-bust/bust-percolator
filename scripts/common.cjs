// Shared by the scripts: devnet.json, the keypairs from the same environment variables as the client, run state.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { ComputeBudgetProgram, Connection, Keypair, PublicKey, Transaction } = require("@solana/web3.js");

const ROOT = path.join(__dirname, "..");
const config = JSON.parse(fs.readFileSync(path.join(ROOT, "devnet.json"), "utf8"));
const expand = (p) => (p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p);
const keypair = (p) => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(expand(p), "utf8"))));
const housePath = process.env.HOUSE_KEYPAIR || "~/.config/solana/id.json";
const connection = new Connection(process.env.RPC || config.rpc, "confirmed");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Sends the instructions and polls the signature over HTTP until it is confirmed. A confirmation that waits on a
// websocket can hang after the transaction has landed: some providers serve websockets from another host.
async function send(ixs, signers, units = 400_000) {
  const tx = new Transaction().add(ComputeBudgetProgram.setComputeUnitLimit({ units }), ComputeBudgetProgram.setComputeUnitPrice({ microLamports: 20_000 }), ...ixs);
  const { blockhash, lastValidBlockHeight } = await connection.getLatestBlockhash("confirmed");
  tx.recentBlockhash = blockhash;
  tx.feePayer = signers[0].publicKey;
  tx.sign(...signers);
  const sig = await connection.sendRawTransaction(tx.serialize());
  for (;;) {
    await sleep(1000);
    const [status] = (await connection.getSignatureStatuses([sig])).value;
    if (status?.err) throw new Error(`${sig} failed: ${JSON.stringify(status.err)}`);
    if (status && status.confirmationStatus !== "processed") return sig;
    if ((await connection.getBlockHeight("confirmed")) > lastValidBlockHeight) throw new Error(`${sig} expired before it was confirmed`);
  }
}

module.exports = {
  ROOT,
  config,
  connection,
  send,
  house: keypair(housePath),
  mintAuthority: keypair(process.env.MINT_AUTHORITY_KEYPAIR || housePath),
  collateral: new PublicKey(config.collateralMint),
  statePath: (name) => path.join(ROOT, "state", name),
  readState: (name) => JSON.parse(fs.readFileSync(path.join(ROOT, "state", name), "utf8")),
  writeState: (name, value) => {
    fs.mkdirSync(path.join(ROOT, "state"), { recursive: true });
    fs.writeFileSync(path.join(ROOT, "state", name), JSON.stringify(value, null, 2));
  },
};
