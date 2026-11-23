#!/bin/bash
# Builds the pinned Percolator program under our program id: a clean copy of the submodule's commit with its
# declare_id! replaced by the id in devnet.json. Output: target/deploy/percolator_prog.so
set -euo pipefail
cd "$(dirname "$0")/.."
ID=$(node -p 'require("./devnet.json").program')
COMMIT=$(git -C percolator-prog rev-parse HEAD)
SRC=target/program
rm -rf "$SRC" && mkdir -p "$SRC" target/deploy
git -C percolator-prog archive "$COMMIT" | tar -x -C "$SRC"
FILE="$SRC/src/v16_program.rs"
UPSTREAM='declare_id!("Perco1ator111111111111111111111111111111111");'
grep -qF "$UPSTREAM" "$FILE" || { echo "upstream declare_id not found in $FILE" >&2; exit 1; }
sed -i.bak "s/declare_id!(\"Perco1ator111111111111111111111111111111111\")/declare_id!(\"$ID\")/" "$FILE" && rm "$FILE.bak"
grep -qF "declare_id!(\"$ID\");" "$FILE"
(cd "$SRC" && cargo-build-sbf)
cp "$SRC/target/deploy/percolator_prog.so" target/deploy/percolator_prog.so
echo "percolator-prog $COMMIT as $ID -> target/deploy/percolator_prog.so"
shasum -a 256 target/deploy/percolator_prog.so
