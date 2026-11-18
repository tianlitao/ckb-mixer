#!/usr/bin/env bash
# Compile the withdraw circuit and run a Groth16 trusted setup.
#
# DEV ONLY. This is a single-party setup run on one machine: whoever ran it
# could have kept the toxic waste and can then forge withdrawal proofs.
# The keys it produces must never guard real funds.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CIRCUITS="$(cd "${HERE}/.." && pwd)"
ROOT="$(cd "${CIRCUITS}/.." && pwd)"

CIRCOM="${CIRCOM:-${ROOT}/.tools/bin/circom}"
SNARKJS="${CIRCUITS}/node_modules/.bin/snarkjs"
BUILD="${CIRCUITS}/build"
# snarkjs needs 2 * (constraints + public inputs + 1) <= 2^POWER.
# The circuit has 5,322 constraints after --O2, so 2^14 is the smallest fit.
POWER=14

entropy() { head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n'; }

mkdir -p "${BUILD}"
cd "${CIRCUITS}"

echo "== compile"
"${CIRCOM}" withdraw.circom --r1cs --wasm --sym --O2 -l node_modules -o "${BUILD}"
"${SNARKJS}" r1cs info "${BUILD}/withdraw.r1cs"

echo "== phase 1 (powers of tau, dev only)"
"${SNARKJS}" powersoftau new bn128 "${POWER}" "${BUILD}/pot_0000.ptau"
"${SNARKJS}" powersoftau contribute "${BUILD}/pot_0000.ptau" "${BUILD}/pot_0001.ptau" \
    --name="dev-only" -e="$(entropy)"
"${SNARKJS}" powersoftau prepare phase2 "${BUILD}/pot_0001.ptau" "${BUILD}/pot_final.ptau"

echo "== phase 2 (circuit specific, dev only)"
"${SNARKJS}" groth16 setup "${BUILD}/withdraw.r1cs" "${BUILD}/pot_final.ptau" "${BUILD}/withdraw_0000.zkey"
# snarkjs logs "circuit too big" and still exits 0, so check the file itself.
[ -s "${BUILD}/withdraw_0000.zkey" ] || { echo "groth16 setup produced no zkey" >&2; exit 1; }
"${SNARKJS}" zkey contribute "${BUILD}/withdraw_0000.zkey" "${BUILD}/withdraw_final.zkey" \
    --name="dev-only" -e="$(entropy)"
"${SNARKJS}" zkey export verificationkey "${BUILD}/withdraw_final.zkey" "${BUILD}/verification_key.json"

rm -f "${BUILD}/pot_0000.ptau" "${BUILD}/pot_0001.ptau" "${BUILD}/withdraw_0000.zkey"
echo "== done: ${BUILD}/withdraw_final.zkey, ${BUILD}/verification_key.json"
