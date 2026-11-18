/* tslint:disable */
/* eslint-disable */

/**
 * What a pool's transactions so far add up to: the state its cell holds, and
 * the commitment tree and nullifier set behind the roots in that state.
 */
export class Ledger {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Apply one pool transaction the way the type script did: the withdrawals
     * of `action` (the `input_type` of the pool input's witness) first, then
     * one deposit per intent input, whose `commitments` (n × 32 bytes) the
     * caller read from the locks of the transaction's inputs, in input order.
     *
     * The nullifier-set proofs are the transaction's own, checked against the
     * root kept here exactly as the script checked them. Leaves the ledger
     * untouched if anything is refused.
     */
    apply(action: Uint8Array | number[], commitments: Uint8Array | number[]): void;
    /**
     * The batch that pays `requests` and deposits `commitments`, in these
     * orders, on top of this ledger.
     *
     * `requests` is w × 200 bytes: fee (8, LE) | root | nullifier hash |
     * Groth16 proof. Returns JSON with the `action` for the pool input's
     * witness, and the `state` and `capacity` the pool output has to have.
     * Whether the proofs verify is not checked here.
     */
    batch(requests: Uint8Array | number[], commitments: Uint8Array | number[]): string;
    current_root(): Uint8Array;
    is_known_root(root: Uint8Array | number[]): boolean;
    is_spent(nullifier_hash: Uint8Array | number[]): boolean;
    /**
     * Where `commitment` sits in the tree, if it is there.
     */
    leaf_index(commitment: Uint8Array | number[]): number | undefined;
    /**
     * A pool as its creation leaves it: the empty state, in a cell of
     * `capacity` shannons. `pool_args` are the args of its type script.
     */
    constructor(pool_args: Uint8Array | number[], capacity: bigint);
    nullifier_root(): Uint8Array;
    /**
     * A ledger saved with [`Ledger::snapshot`]. The caller still has to hold
     * the result against a pool cell on chain: a snapshot is a cache.
     */
    static restore(pool_args: Uint8Array | number[], snapshot: string): Ledger;
    snapshot(): string;
    /**
     * The pool cell's data this ledger stands for.
     */
    state_bytes(): Uint8Array;
    /**
     * Everything a withdrawal of the note at `leaf_index` needs before it is
     * proved: the input of the withdraw circuit (`input`, for snarkjs), and
     * the `root` and `nullifier_hash` the proof will be bound to, with the
     * public signals snarkjs has to report (`public`).
     *
     * Refuses a note whose secrets do not give that leaf, and one that is
     * withdrawn already.
     */
    withdraw_input(leaf_index: number, nullifier: Uint8Array | number[], secret: Uint8Array | number[], recipient_lock_hash: Uint8Array | number[], fee: bigint): string;
    /**
     * The pool cell's capacity this ledger stands for, in shannons.
     */
    readonly capacity: bigint;
    readonly denomination: bigint;
    readonly leaf_count: number;
    readonly nullifier_count: number;
}

/**
 * CKB's default hash: blake2b-256 personalised with `ckb-default-hash`.
 */
export function ckb_hash(data: Uint8Array | number[]): Uint8Array;

/**
 * `Poseidon(nullifier, secret)`: the leaf a note puts into the tree.
 */
export function commitment(nullifier: Uint8Array | number[], secret: Uint8Array | number[]): Uint8Array;

/**
 * The withdrawals of a batch action, as a JSON array; `undefined` if the
 * bytes are not one well-formed batch.
 */
export function decode_batch(action: Uint8Array | number[]): string | undefined;

/**
 * The 104 bytes of an intent lock's args.
 *
 * `reclaim_after` is a number of blocks, not an encoded `since`: a value of
 * 2^56 or more is refused here, because the cell it would make could never be
 * taken back and no script would say so when it is created.
 */
export function intent_args(pool_type_hash: Uint8Array | number[], commitment: Uint8Array | number[], owner_lock_hash: Uint8Array | number[], reclaim_after: bigint): Uint8Array;

/**
 * Whether 32 bytes are a canonical field element; a commitment has to be one.
 */
export function is_field_element(bytes: Uint8Array | number[]): boolean;

/**
 * `Poseidon(nullifier, leaf_index)`: what a withdrawal reveals.
 */
export function nullifier_hash(nullifier: Uint8Array | number[], leaf_index: number): Uint8Array;

/**
 * The fields of an intent lock's args as JSON, or `undefined` if they are not
 * 104 bytes. `reclaimable` says whether any `since` can express the wait.
 */
export function parse_intent_args(args: Uint8Array | number[]): string | undefined;

/**
 * The fields of a v2 pool type script's args, as JSON. `hash_type` is the
 * byte the script carries (4 is `data2`).
 */
export function pool_config(args: Uint8Array | number[]): string;

/**
 * snarkjs' `proof.json` as the 128 bytes the pool's verifier reads.
 */
export function proof_from_snarkjs(json: string): Uint8Array;

/**
 * The `since` the input of a reclaim needs after a wait of `blocks` blocks.
 */
export function reclaim_since(blocks: bigint): bigint;

/**
 * snarkjs' `verification_key.json` as the bytes of the verifying-key cell.
 * Their [`ckb_hash`] has to be the `vk_hash` of the pool's args.
 */
export function vk_from_snarkjs(json: string): Uint8Array;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_ledger_free: (a: number, b: number) => void;
    readonly ckb_hash: (a: number, b: number) => [number, number];
    readonly commitment: (a: number, b: number, c: number, d: number) => [number, number, number, number];
    readonly decode_batch: (a: number, b: number) => [number, number];
    readonly intent_args: (a: number, b: number, c: number, d: number, e: number, f: number, g: bigint) => [number, number, number, number];
    readonly is_field_element: (a: number, b: number) => number;
    readonly ledger_apply: (a: number, b: number, c: number, d: number, e: number) => [number, number];
    readonly ledger_batch: (a: number, b: number, c: number, d: number, e: number) => [number, number, number, number];
    readonly ledger_capacity: (a: number) => bigint;
    readonly ledger_current_root: (a: number) => [number, number];
    readonly ledger_denomination: (a: number) => bigint;
    readonly ledger_is_known_root: (a: number, b: number, c: number) => number;
    readonly ledger_is_spent: (a: number, b: number, c: number) => number;
    readonly ledger_leaf_count: (a: number) => number;
    readonly ledger_leaf_index: (a: number, b: number, c: number) => number;
    readonly ledger_new: (a: number, b: number, c: bigint) => [number, number, number];
    readonly ledger_nullifier_count: (a: number) => number;
    readonly ledger_nullifier_root: (a: number) => [number, number];
    readonly ledger_restore: (a: number, b: number, c: number, d: number) => [number, number, number];
    readonly ledger_snapshot: (a: number) => [number, number];
    readonly ledger_state_bytes: (a: number) => [number, number];
    readonly ledger_withdraw_input: (a: number, b: number, c: number, d: number, e: number, f: number, g: number, h: number, i: bigint) => [number, number, number, number];
    readonly nullifier_hash: (a: number, b: number, c: number) => [number, number, number, number];
    readonly parse_intent_args: (a: number, b: number) => [number, number];
    readonly pool_config: (a: number, b: number) => [number, number, number, number];
    readonly proof_from_snarkjs: (a: number, b: number) => [number, number];
    readonly reclaim_since: (a: bigint) => [bigint, number, number];
    readonly vk_from_snarkjs: (a: number, b: number) => [number, number];
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __externref_table_dealloc: (a: number) => void;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
