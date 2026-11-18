//! Just enough JSON-RPC to deploy and drive the mixer on a public CKB node.
//!
//! The node is the only source of truth: cells come from its indexer, and a
//! transaction is dry-run with `test_tx_pool_accept` before it is sent.

use std::time::{Duration, Instant};

use ckb_jsonrpc_types as json_types;
use ckb_types::{bytes::Bytes, core::TransactionView, packed, prelude::*};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};

/// Cells per `get_cells` page.
const PAGE: usize = 100;

pub struct Rpc {
    url: String,
    agent: ureq::Agent,
}

#[derive(Debug, Clone)]
pub struct LiveCell {
    pub out_point: packed::OutPoint,
    pub output: packed::CellOutput,
    pub data: Bytes,
}

impl LiveCell {
    pub fn capacity(&self) -> u64 {
        self.output.capacity().unpack()
    }
}

/// Which script of a cell an indexer query matches.
#[derive(Debug, Clone, Copy)]
pub enum Role {
    Lock,
    Type,
}

/// A transaction the node knows, in the pool or on chain.
#[derive(Debug, Clone)]
pub struct ChainTx {
    pub tx: TransactionView,
    pub status: String,
    pub block_number: Option<u64>,
    /// What verifying it cost, as the node recorded it.
    pub cycles: Option<u64>,
    /// What it pays. The node only says so while the transaction is in its pool.
    pub fee: Option<u64>,
}

/// What the node reports about a dry run.
#[derive(Debug, Clone, Copy)]
pub struct Accepted {
    pub cycles: u64,
    pub fee: u64,
}

#[derive(Debug, Clone)]
pub struct Committed {
    pub block_number: u64,
    /// Cycles the node recorded for the transaction, when it still has them.
    pub cycles: Option<u64>,
    /// Seconds between the first poll and the block that committed it.
    pub seconds: u64,
}

fn hex_u64(value: &Value) -> Result<u64, String> {
    let text = value.as_str().ok_or_else(|| format!("expected a hex number, got {value}"))?;
    u64::from_str_radix(text.trim_start_matches("0x"), 16).map_err(|e| format!("{text}: {e}"))
}

fn parse<T: DeserializeOwned>(method: &str, value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("{method}: unexpected response: {error}"))
}

pub fn hash_hex(hash: &packed::Byte32) -> String {
    format!("{hash:#x}")
}

fn transaction_json(tx: &TransactionView) -> Value {
    serde_json::to_value(json_types::Transaction::from(tx.data())).expect("a transaction is JSON")
}

impl Rpc {
    pub fn new(url: &str) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(90)).build();
        Self { url: url.to_string(), agent }
    }

    /// One JSON-RPC call. Transport errors are retried, the public endpoint
    /// drops a request now and then; an error the node returns is final.
    fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "id": 1, "jsonrpc": "2.0", "method": method, "params": params });
        let mut last = String::new();
        for attempt in 0..5u64 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2 * attempt));
            }
            match self.agent.post(&self.url).send_json(&body) {
                Ok(response) => {
                    let value: Value = response.into_json().map_err(|e| format!("{method}: {e}"))?;
                    return match value.get("error").filter(|error| !error.is_null()) {
                        Some(error) => Err(format!(
                            "{method}: {}",
                            error["message"].as_str().map(str::to_string).unwrap_or_else(|| error.to_string())
                        )),
                        None => Ok(value["result"].clone()),
                    };
                }
                Err(error) => last = format!("{method}: {error}"),
            }
        }
        Err(last)
    }

    pub fn chain(&self) -> Result<String, String> {
        let info = self.call("get_blockchain_info", json!([]))?;
        info["chain"].as_str().map(str::to_string).ok_or_else(|| "no chain name".to_string())
    }

    fn indexer_tip(&self) -> Result<u64, String> {
        hex_u64(&self.call("get_indexer_tip", json!([]))?["block_number"])
    }

    /// Wait until the node's indexer has seen `block_number`, so that the cells
    /// it returns include that block's.
    pub fn wait_indexed(&self, block_number: u64) -> Result<(), String> {
        let start = Instant::now();
        while self.indexer_tip()? < block_number {
            if start.elapsed() > Duration::from_secs(120) {
                return Err(format!("the indexer has not reached block {block_number}"));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        Ok(())
    }

    /// Every live cell whose lock (or type) script is exactly `script`.
    pub fn live_cells(&self, script: &packed::Script, role: Role) -> Result<Vec<LiveCell>, String> {
        self.cells(json!({
            "script": json_types::Script::from(script.clone()),
            "script_type": match role { Role::Lock => "lock", Role::Type => "type" },
            "script_search_mode": "exact",
        }))
    }

    /// The live cells of `lock` that hold nothing but capacity: no type script
    /// and no data. The indexer filters, so the data of other cells under the
    /// same lock (deployed code, say) is not downloaded.
    pub fn plain_cells(&self, lock: &packed::Script) -> Result<Vec<LiveCell>, String> {
        let cells = self.cells(json!({
            "script": json_types::Script::from(lock.clone()),
            "script_type": "lock",
            "script_search_mode": "exact",
            "filter": { "script_len_range": ["0x0", "0x1"], "output_data_len_range": ["0x0", "0x1"] },
        }))?;
        Ok(cells.into_iter().filter(|cell| cell.output.type_().is_none() && cell.data.is_empty()).collect())
    }

    /// Capacity of all live cells of `lock`, whatever they hold.
    pub fn total_capacity(&self, lock: &packed::Script) -> Result<u64, String> {
        let key = json!({ "script": json_types::Script::from(lock.clone()), "script_type": "lock" });
        hex_u64(&self.call("get_cells_capacity", json!([key]))?["capacity"])
    }

    /// Timestamp of block `number`, in milliseconds since the epoch.
    pub fn block_timestamp(&self, number: u64) -> Result<u64, String> {
        hex_u64(&self.call("get_header_by_number", json!([format!("{number:#x}")]))?["timestamp"])
    }

    /// The live cells an indexer search key matches, all pages of them.
    fn cells(&self, key: Value) -> Result<Vec<LiveCell>, String> {
        #[derive(Deserialize)]
        struct Page {
            objects: Vec<Object>,
            last_cursor: String,
        }
        #[derive(Deserialize)]
        struct Object {
            out_point: json_types::OutPoint,
            output: json_types::CellOutput,
            output_data: Option<json_types::JsonBytes>,
        }

        let mut cells = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = vec![key.clone(), json!("asc"), json!(format!("{PAGE:#x}"))];
            if let Some(cursor) = &cursor {
                params.push(json!(cursor));
            }
            let page: Page = parse("get_cells", self.call("get_cells", Value::Array(params))?)?;
            let count = page.objects.len();
            cells.extend(page.objects.into_iter().map(|object| LiveCell {
                out_point: object.out_point.into(),
                output: object.output.into(),
                data: object.output_data.map(json_types::JsonBytes::into_bytes).unwrap_or_default(),
            }));
            if count < PAGE {
                return Ok(cells);
            }
            cursor = Some(page.last_cursor);
        }
    }

    /// The cell at `out_point` with its data, if it is live.
    pub fn live_cell(&self, out_point: &packed::OutPoint) -> Result<Option<LiveCell>, String> {
        let params = json!([json_types::OutPoint::from(out_point.clone()), true]);
        let value = self.call("get_live_cell", params)?;
        if value["status"].as_str() != Some("live") {
            return Ok(None);
        }
        let output: json_types::CellOutput = parse("get_live_cell", value["cell"]["output"].clone())?;
        let data: json_types::JsonBytes = parse("get_live_cell", value["cell"]["data"]["content"].clone())?;
        Ok(Some(LiveCell { out_point: out_point.clone(), output: output.into(), data: data.into_bytes() }))
    }

    pub fn transaction(&self, hash: &packed::Byte32) -> Result<Option<ChainTx>, String> {
        let value = self.call("get_transaction", json!([hash_hex(hash)]))?;
        if value["transaction"].is_null() {
            return Ok(None);
        }
        let view: json_types::TransactionView = parse("get_transaction", value["transaction"].clone())?;
        let tx: packed::Transaction = view.inner.into();
        let status = &value["tx_status"];
        Ok(Some(ChainTx {
            tx: tx.into_view(),
            status: status["status"].as_str().unwrap_or("unknown").to_string(),
            block_number: hex_u64(&status["block_number"]).ok(),
            cycles: hex_u64(&value["cycles"]).ok(),
            fee: hex_u64(&value["fee"]).ok(),
        }))
    }

    /// Ask the node whether its pool would take `tx`, without submitting it.
    /// Runs every script; the error of a rejected transaction names the script
    /// and its exit code.
    pub fn dry_run(&self, tx: &TransactionView) -> Result<Accepted, String> {
        let value = self.call("test_tx_pool_accept", json!([transaction_json(tx), "passthrough"]))?;
        Ok(Accepted { cycles: hex_u64(&value["cycles"])?, fee: hex_u64(&value["fee"])? })
    }

    pub fn send(&self, tx: &TransactionView) -> Result<packed::Byte32, String> {
        match self.call("send_transaction", json!([transaction_json(tx), "passthrough"])) {
            Ok(_) => Ok(tx.hash()),
            // A retried request finds its first attempt already in the pool.
            Err(error) if error.contains("PoolRejectedDuplicatedTransaction") => Ok(tx.hash()),
            Err(error) => Err(error),
        }
    }

    /// Poll until `hash` is committed, printing each status change.
    pub fn wait_committed(&self, hash: &packed::Byte32, timeout: Duration) -> Result<Committed, String> {
        let start = Instant::now();
        let mut seen = String::new();
        loop {
            let value = self.call("get_transaction", json!([hash_hex(hash)]))?;
            let status = value["tx_status"]["status"].as_str().unwrap_or("unknown").to_string();
            if status != seen {
                println!("    {:>4}s  {status}", start.elapsed().as_secs());
                seen = status.clone();
            }
            match status.as_str() {
                "committed" => {
                    return Ok(Committed {
                        block_number: hex_u64(&value["tx_status"]["block_number"])?,
                        cycles: hex_u64(&value["cycles"]).ok(),
                        seconds: start.elapsed().as_secs(),
                    });
                }
                "rejected" => {
                    return Err(format!("{} was rejected: {}", hash_hex(hash), value["tx_status"]["reason"]));
                }
                _ => {}
            }
            if start.elapsed() > timeout {
                return Err(format!(
                    "{} is still {status} after {}s; it may yet be committed",
                    hash_hex(hash),
                    timeout.as_secs()
                ));
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    }
}
