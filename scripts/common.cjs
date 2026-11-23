// Shared by the scripts: devnet.json, the keypairs from the same environment variables as the client, run state.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { Connection, Keypair, PublicKey } = require("@solana/web3.js");

const ROOT = path.join(__dirname, "..");
const config = JSON.parse(fs.readFileSync(path.join(ROOT, "devnet.json"), "utf8"));
const expand = (p) => (p.startsWith("~/") ? path.join(os.homedir(), p.slice(2)) : p);
const keypair = (p) => Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(expand(p), "utf8"))));
const housePath = process.env.HOUSE_KEYPAIR || "~/.config/solana/id.json";

module.exports = {
  ROOT,
  config,
  connection: new Connection(process.env.RPC || config.rpc, "confirmed"),
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
