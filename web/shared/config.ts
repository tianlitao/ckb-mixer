// The pool this front end and the aggregator Worker are built for: pool v2 on
// the CKB testnet (README §五). Everything else about the pool is read from
// the args of its type script, on chain.

export interface OutPointJson {
  tx_hash: string;
  index: string;
}

export interface ScriptJson {
  code_hash: string;
  hash_type: string;
  args: string;
}

export const RPC_URL = "https://testnet.ckb.dev/rpc";
export const CHAIN = "ckb_testnet";
export const EXPLORER = "https://testnet.explorer.nervos.org";

/** The pool's type script. Its args carry the Type ID, so one cell has it. */
export const POOL_TYPE: ScriptJson = {
  code_hash: "0x9e98a45f112e4f9d5721a726a156122159011e23e8bf393b1876a9ac3c89ff6b",
  hash_type: "data2",
  args:
    "0x22dcc42854379ee5996852e60fc7337d37257ffb4fd2c4acb48adca69373efa500d6117e03000000" +
    "fc49cb947eb743b15bddec9020c692ae0e40e0b0605f084e8c74dd62615220b4" +
    "f24dbd685ea7c69568180977c6fbf9d1289931f4ebfe76e9be9dd1ddc58a763a04" +
    "b3753821112dae7a749a01954662056368fd9cf873294376e82cda98ad75178304" +
    "2000",
};

/** The transaction that created the pool cell; the replay starts there. */
export const POOL_CREATION_TX = "0x79fa82a11aa151830e28f8ddebc474c8fb1afdde98c522e47ef55720f4dc0173";

const CODE_1 = "0xae25728119746b82cdc78d173548dbb2209957d2994bcc140fbbef534dc6dfeb";
const CODE_2 = "0x25d31a87791fbb34b5111d537a38f3e116fcaaabf219839fd5f3939e3b3744b1";

/** The cells holding the verifying key and the three scripts. */
export const DEPS = {
  vk: { tx_hash: CODE_1, index: "0x0" },
  poolLock: { tx_hash: CODE_1, index: "0x1" },
  poolType: { tx_hash: CODE_2, index: "0x0" },
  intentLock: { tx_hash: CODE_2, index: "0x1" },
} satisfies Record<string, OutPointJson>;

export const SHANNONS = 100_000_000n;

/** What a withdrawal offers whoever sends it, by default: 0.01 CKB. */
export const DEFAULT_WITHDRAW_FEE = 1_000_000n;

/** Fee rate this code pays, in shannons per 1000 bytes of weight. */
export const FEE_RATE = 5000n;

/** A relative `since` counts blocks in 56 bits; a longer wait never ends. */
export const LONGEST_WAIT = (1n << 56n) - 1n;

/** Bytes a plain cell occupies beyond its lock's args: capacity + code hash + hash type. */
export const PLAIN_CELL_BASE = 8n + 32n + 1n;
