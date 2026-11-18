# Host-side workspace (mixer-core + tests). The contracts are built separately,
# for riscv64, by scripts/build-contracts.sh.
SHELL := /bin/bash
CIRCOM_VERSION := v2.2.3

.PHONY: all tools deps setup contracts bench test clean

all: setup contracts test

## Install circom into ./.tools (project-local, not global).
tools:
	cargo install --git https://github.com/iden3/circom.git --tag $(CIRCOM_VERSION) \
		--root .tools --locked circom

## Node dependencies of the circuit toolchain.
deps:
	cd circuits && npm ci

## Compile the circuit and run the DEV-ONLY Groth16 setup (~2 minutes).
setup: deps
	cd circuits && npm run setup

## Regenerate the Poseidon constants from circomlibjs (only after changing params).
gen-constants: deps
	cd circuits && npm run gen-constants

## Build the on-chain scripts: make contracts [VARIANT=default|size|bext|profile|all]
VARIANT ?= default
contracts:
	scripts/build-contracts.sh $(VARIANT)

## Unit, end-to-end and adversarial tests against CKB-VM.
test: contracts
	cargo test --workspace

## Cycles, sizes and stage breakdowns for every build variant.
bench:
	scripts/build-contracts.sh all
	cargo test -p mixer-tests --test cycles -- --nocapture

clean:
	cargo clean
	rm -rf contracts/target contracts/target-size contracts/target-bext contracts/target-profile
	rm -rf circuits/build
