// Keypairs under keys/, created on first use (the same directory the client uses).
const fs = require("fs");
const path = require("path");
const { Keypair } = require("@solana/web3.js");
const { ROOT } = require("./common.cjs");

function loadOrNew(name) {
  const file = path.join(ROOT, "keys", name);
  if (fs.existsSync(file)) return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))));
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const k = Keypair.generate();
  fs.writeFileSync(file, JSON.stringify(Array.from(k.secretKey)));
  return k;
}

module.exports = { loadOrNew };
