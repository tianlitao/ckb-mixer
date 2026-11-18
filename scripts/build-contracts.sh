#!/usr/bin/env bash
# Build the on-chain scripts for CKB-VM.
#
# Usage: scripts/build-contracts.sh [variant]
#
#   default   opt-level 3, base rv64imc                  -> contracts/target
#   size      opt-level "z" (what groth16-ckb ships)     -> contracts/target-size
#   bext      opt-level 3 + zba/zbb/zbc/zbs extensions   -> contracts/target-bext
#   profile   default + cycle checkpoints in pool-type   -> contracts/target-profile
#   all       every variant above
#
# The variants exist to compare cycle counts; the tests pick one through the
# MIXER_CONTRACTS_DIR environment variable (see `make bench`).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="riscv64imac-unknown-none-elf"
VARIANT="${1:-default}"

if [ "${VARIANT}" = "all" ]; then
    for variant in default size bext profile; do
        "${BASH_SOURCE[0]}" "${variant}"
    done
    exit 0
fi

# No atomic instructions in any variant, see contracts/.cargo/config.toml.
NO_ATOMICS="-a,-zaamo,-zalrsc"
CARGO_FEATURES=()
case "${VARIANT}" in
    default)
        TARGET_DIR="${ROOT}/contracts/target"
        OPT_LEVEL=3
        FEATURES="${NO_ATOMICS}"
        ;;
    size)
        TARGET_DIR="${ROOT}/contracts/target-size"
        OPT_LEVEL=z
        FEATURES="${NO_ATOMICS}"
        ;;
    bext)
        TARGET_DIR="${ROOT}/contracts/target-bext"
        OPT_LEVEL=3
        FEATURES="${NO_ATOMICS},+zba,+zbb,+zbc,+zbs"
        ;;
    profile)
        TARGET_DIR="${ROOT}/contracts/target-profile"
        OPT_LEVEL=3
        FEATURES="${NO_ATOMICS}"
        CARGO_FEATURES=(--features pool-type/profile)
        ;;
    *)
        echo "unknown variant: ${VARIANT} (default | size | bext | profile | all)" >&2
        exit 2
        ;;
esac

if ! rustup target list --installed | grep -q "^${TARGET}$"; then
    echo "rust target ${TARGET} is not installed: rustup target add ${TARGET}" >&2
    exit 1
fi

cd "${ROOT}/contracts"
RUSTFLAGS="-C target-feature=${FEATURES}" \
CARGO_PROFILE_RELEASE_OPT_LEVEL="${OPT_LEVEL}" \
    cargo build --release --target "${TARGET}" --target-dir "${TARGET_DIR}" \
    -p pool-type -p pool-lock ${CARGO_FEATURES[@]+"${CARGO_FEATURES[@]}"}

OUT="${TARGET_DIR}/${TARGET}/release"
echo "built (${VARIANT}):"
ls -l "${OUT}/pool-type" "${OUT}/pool-lock" | awk '{ printf "  %8d bytes  %s\n", $5, $9 }'
