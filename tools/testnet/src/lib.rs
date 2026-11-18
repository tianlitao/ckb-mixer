//! Deploying the mixer PoC to CKB testnet and driving it there, with plain
//! JSON-RPC against a public node: no node, indexer or SDK of our own.

pub mod pool;
pub mod proof;
pub mod rpc;
pub mod wallet;

pub const TESTNET_RPC: &str = "https://testnet.ckb.dev/rpc";
pub const EXPLORER_TX: &str = "https://testnet.explorer.nervos.org/transaction/";
