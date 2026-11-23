<h1 align="center">Bust × Percolator</h1>

Leveraged markets on Pump coins, built on [Percolator](https://github.com/aeyakovenko/percolator), the perpetual-futures
risk engine by Anatoly Yakovenko. Each coin gets its own market inside one Percolator market group, with its own
insurance and backing, so a loss in one coin cannot be paid from another.

This repository is a devnet trial. It is not deployed on mainnet and holds no real funds.

## Deployments

| Network | Program id |
|---|---|
| Devnet | [`7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo`](https://explorer.solana.com/address/7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo?cluster=devnet) |

The devnet program is [percolator-prog](https://github.com/aeyakovenko/percolator-prog) at commit
[`aac89b9`](https://github.com/aeyakovenko/percolator-prog/commit/aac89b9e078696131df2100368d4fd53baec5fac), built
with its `declare_id!` set to the id above and nothing else changed. The build below reproduces the deployed binary
byte for byte (sha256 `760be21accd628704f5c3594b393492c086c63b845b14183e97d8768c38d23b1`).

## Build the program

Percolator is a git submodule pinned to the commit above. With the Solana CLI and platform-tools v1.52 or newer:

```sh
git submodule update --init
scripts/build-program.sh
solana program dump 7R1ED7LLqoP468afxJfq9HJ5ikqT4z73C6AYoE7evdPo deployed.so -u devnet
shasum -a 256 deployed.so target/deploy/percolator_prog.so
```

The script builds a clean copy of the submodule's commit under `target/program`, so the submodule itself stays
unchanged.

## License

The code in this repository is licensed under the [Business Source License 1.1](LICENSE). Percolator is licensed
under Apache-2.0 by its authors.
