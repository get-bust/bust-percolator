//! Configuration, keys, run state and transaction sending shared by every command.

use anyhow::{anyhow, Context, Result};
use percolator_prog::ix::Instruction as Ix;
use serde_json::Value;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_error::ProgramError,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};
use std::{fs, path::PathBuf, str::FromStr};

pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn expand(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

fn read_keypair(path: &PathBuf) -> Result<Keypair> {
    read_keypair_file(path).map_err(|e| anyhow!("read {}: {e}", path.display()))
}

/// A keypair under keys/, created on first use.
/// The market being worked on: `PERC_MARKET` names a second market beside the first, with a state file and keys of
/// its own (`state/perc-<name>.json`, `keys/<name>/`). Wallet keys (the trader's, the racers') are shared.
pub fn profile() -> Option<String> {
    std::env::var("PERC_MARKET").ok().filter(|p| !p.is_empty())
}

/// A key that belongs to the market (its account, its portfolios), under the market's own directory.
pub fn market_key(name: &str) -> Result<Keypair> {
    match profile() {
        Some(p) => load_or_new(&format!("{p}/{name}")),
        None => load_or_new(name),
    }
}

pub fn load_or_new(name: &str) -> Result<Keypair> {
    let path = root().join("keys").join(name);
    if path.exists() {
        return read_keypair(&path);
    }
    fs::create_dir_all(path.parent().unwrap_or(&root().join("keys")))?;
    let k = Keypair::new();
    fs::write(&path, serde_json::to_string(&k.to_bytes().to_vec())?)?;
    Ok(k)
}

pub fn pk(v: &Value, key: &str) -> Result<Pubkey> {
    Pubkey::from_str(v[key].as_str().ok_or_else(|| anyhow!("state has no {key}"))?).map_err(Into::into)
}

/// Program errors carry no message of their own; their debug form names the variant.
pub fn eng<T>(r: Result<T, ProgramError>) -> Result<T> {
    r.map_err(|e| anyhow!("{e:?}"))
}

pub fn state_path() -> PathBuf {
    match profile() {
        Some(p) => root().join(format!("state/perc-{p}.json")),
        None => root().join("state/perc.json"),
    }
}

pub fn read_state() -> Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(state_path()).context("no state file for this market: run setup first")?)?)
}

pub fn write_state(st: &Value) -> Result<()> {
    fs::create_dir_all(root().join("state"))?;
    fs::write(state_path(), serde_json::to_string_pretty(st)?)?;
    Ok(())
}

pub fn vault_accounts(program: &Pubkey, market: &Pubkey, mint: &Pubkey) -> (Pubkey, Pubkey) {
    let authority = Pubkey::find_program_address(&[b"vault", market.as_ref()], program).0;
    let vault = spl_associated_token_account::get_associated_token_address(&authority, mint);
    (authority, vault)
}

pub struct Ctx {
    pub rpc: RpcClient,
    pub program: Pubkey,
    /// Collateral mint (a devnet test USDC with 6 decimals).
    pub collateral: Pubkey,
    /// Market authority, oracle authority, LP counterparty and fee payer.
    pub house: Keypair,
    pub trader: Keypair,
    /// The keeper: updates the feed, settles the market and samples the pool for the matcher. A hot key that holds
    /// only what its fees need and can move nothing else: the feed update and cranks are open to anyone, and the
    /// matcher's samples take this key as its observer.
    pub keeper: Keypair,
    /// Mint authority of the collateral mint, to fund the house and the trader.
    pub mint_authority: Keypair,
}

impl Ctx {
    pub fn new() -> Result<Self> {
        let cfg: Value = serde_json::from_str(&fs::read_to_string(root().join("devnet.json"))?)?;
        let url = std::env::var("RPC").unwrap_or_else(|_| cfg["rpc"].as_str().unwrap_or_default().to_string());
        let house = expand(&std::env::var("HOUSE_KEYPAIR").unwrap_or_else(|_| "~/.config/solana/id.json".into()));
        let minter = std::env::var("MINT_AUTHORITY_KEYPAIR").map(|p| expand(&p)).unwrap_or_else(|_| house.clone());
        Ok(Self {
            rpc: RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()),
            program: pk(&cfg, "program")?,
            collateral: pk(&cfg, "collateralMint")?,
            house: read_keypair(&house)?,
            trader: load_or_new("trader.json")?,
            keeper: match std::env::var("KEEPER_KEYPAIR") {
                Ok(path) => read_keypair(&expand(&path))?,
                Err(_) => load_or_new("keeper.json")?,
            },
            mint_authority: read_keypair(&minter)?,
        })
    }

    /// Sends and confirms one transaction; the first signer pays. A signer passed twice signs once.
    pub fn send(&self, ixs: Vec<Instruction>, signers: &[&Keypair]) -> Result<String> {
        self.send_with_budget(ixs, signers, 1_400_000, 20_000)
    }

    /// `send` with a compute unit limit and price of its own: the fee is 5,000 lamports a signature plus the limit
    /// times the price in micro-lamports.
    pub fn send_with_budget(&self, ixs: Vec<Instruction>, signers: &[&Keypair], units: u32, micro_lamports: u64) -> Result<String> {
        let mut all = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(units),
            ComputeBudgetInstruction::set_compute_unit_price(micro_lamports),
        ];
        all.extend(ixs);
        let mut unique: Vec<&Keypair> = Vec::new();
        for s in signers {
            if !unique.iter().any(|u| u.pubkey() == s.pubkey()) {
                unique.push(s);
            }
        }
        let blockhash = self.rpc.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(&all, Some(&unique[0].pubkey()), &unique, blockhash);
        let sig = self.rpc.send_and_confirm_transaction(&tx).map_err(|e| anyhow!("{e:#?}"))?;
        Ok(sig.to_string())
    }

    pub fn prog(&self, ix: Ix, accounts: Vec<AccountMeta>) -> Instruction {
        Instruction { program_id: self.program, accounts, data: ix.encode() }
    }

    pub fn data(&self, key: &Pubkey) -> Result<Vec<u8>> {
        Ok(self.rpc.get_account_data(key)?)
    }
}

/// An error in one line, from where Percolator or a program names the failing instruction, or "an RPC error" when it
/// names none: a connection error carries the RPC's address, key included, and must not be printed.
pub fn brief(e: &anyhow::Error) -> String {
    let flat = e.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.find("InstructionError") {
        Some(i) => flat[i..].chars().take(90).collect(),
        None => "an RPC error".into(),
    }
}
