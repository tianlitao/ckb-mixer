/* @ts-self-types="./mixer_wasm.d.ts" */

/**
 * What a pool's transactions so far add up to: the state its cell holds, and
 * the commitment tree and nullifier set behind the roots in that state.
 */
export class Ledger {
    static __wrap(ptr) {
        const obj = Object.create(Ledger.prototype);
        obj.__wbg_ptr = ptr;
        LedgerFinalization.register(obj, obj.__wbg_ptr, obj);
        return obj;
    }
    __destroy_into_raw() {
        const ptr = this.__wbg_ptr;
        this.__wbg_ptr = 0;
        LedgerFinalization.unregister(this);
        return ptr;
    }
    free() {
        const ptr = this.__destroy_into_raw();
        wasm.__wbg_ledger_free(ptr, 0);
    }
    /**
     * Apply one pool transaction the way the type script did: the withdrawals
     * of `action` (the `input_type` of the pool input's witness) first, then
     * one deposit per intent input, whose `commitments` (n × 32 bytes) the
     * caller read from the locks of the transaction's inputs, in input order.
     *
     * The nullifier-set proofs are the transaction's own, checked against the
     * root kept here exactly as the script checked them. Leaves the ledger
     * untouched if anything is refused.
     * @param {Uint8Array | number[]} action
     * @param {Uint8Array | number[]} commitments
     */
    apply(action, commitments) {
        let ptr0, len0, ptr1, len1;
        try {
            ptr0 = passArray8ToWasm0(action, wasm.__wbindgen_malloc);
            len0 = WASM_VECTOR_LEN;
            ptr1 = passArray8ToWasm0(commitments, wasm.__wbindgen_malloc);
            len1 = WASM_VECTOR_LEN;
        } catch (e) {
            if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
            if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
            throw e;
        }
        const ret = wasm.ledger_apply(this.__wbg_ptr, ptr0, len0, ptr1, len1);
        if (ret[1]) {
            throw takeFromExternrefTable0(ret[0]);
        }
    }
    /**
     * The batch that pays `requests` and deposits `commitments`, in these
     * orders, on top of this ledger.
     *
     * `requests` is w × 200 bytes: fee (8, LE) | root | nullifier hash |
     * Groth16 proof. Returns JSON with the `action` for the pool input's
     * witness, and the `state` and `capacity` the pool output has to have.
     * Whether the proofs verify is not checked here.
     * @param {Uint8Array | number[]} requests
     * @param {Uint8Array | number[]} commitments
     * @returns {string}
     */
    batch(requests, commitments) {
        let deferred4_0;
        let deferred4_1;
        try {
            let ptr0, len0, ptr1, len1;
            try {
                ptr0 = passArray8ToWasm0(requests, wasm.__wbindgen_malloc);
                len0 = WASM_VECTOR_LEN;
                ptr1 = passArray8ToWasm0(commitments, wasm.__wbindgen_malloc);
                len1 = WASM_VECTOR_LEN;
            } catch (e) {
                if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
                if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
                throw e;
            }
            const ret = wasm.ledger_batch(this.__wbg_ptr, ptr0, len0, ptr1, len1);
            var ptr3 = ret[0];
            var len3 = ret[1];
            if (ret[3]) {
                ptr3 = 0; len3 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred4_0 = ptr3;
            deferred4_1 = len3;
            return getStringFromWasm0(ptr3, len3);
        } finally {
            wasm.__wbindgen_free(deferred4_0, deferred4_1, 1);
        }
    }
    /**
     * The pool cell's capacity this ledger stands for, in shannons.
     * @returns {bigint}
     */
    get capacity() {
        const ret = wasm.ledger_capacity(this.__wbg_ptr);
        return BigInt.asUintN(64, ret);
    }
    /**
     * @returns {Uint8Array}
     */
    current_root() {
        const ret = wasm.ledger_current_root(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * @returns {bigint}
     */
    get denomination() {
        const ret = wasm.ledger_denomination(this.__wbg_ptr);
        return BigInt.asUintN(64, ret);
    }
    /**
     * @param {Uint8Array | number[]} root
     * @returns {boolean}
     */
    is_known_root(root) {
        const ptr0 = passArray8ToWasm0(root, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.ledger_is_known_root(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * @param {Uint8Array | number[]} nullifier_hash
     * @returns {boolean}
     */
    is_spent(nullifier_hash) {
        const ptr0 = passArray8ToWasm0(nullifier_hash, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.ledger_is_spent(this.__wbg_ptr, ptr0, len0);
        return ret !== 0;
    }
    /**
     * @returns {number}
     */
    get leaf_count() {
        const ret = wasm.ledger_leaf_count(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * Where `commitment` sits in the tree, if it is there.
     * @param {Uint8Array | number[]} commitment
     * @returns {number | undefined}
     */
    leaf_index(commitment) {
        const ptr0 = passArray8ToWasm0(commitment, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.ledger_leaf_index(this.__wbg_ptr, ptr0, len0);
        return ret === Number.MAX_SAFE_INTEGER ? undefined : ret;
    }
    /**
     * A pool as its creation leaves it: the empty state, in a cell of
     * `capacity` shannons. `pool_args` are the args of its type script.
     * @param {Uint8Array | number[]} pool_args
     * @param {bigint} capacity
     */
    constructor(pool_args, capacity) {
        const val2 = BigInt.asIntN(64, capacity);
        const ptr0 = passArray8ToWasm0(pool_args, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.ledger_new(ptr0, len0, val2);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        this.__wbg_ptr = ret[0];
        LedgerFinalization.register(this, this.__wbg_ptr, this);
        return this;
    }
    /**
     * @returns {number}
     */
    get nullifier_count() {
        const ret = wasm.ledger_nullifier_count(this.__wbg_ptr);
        return ret >>> 0;
    }
    /**
     * @returns {Uint8Array}
     */
    nullifier_root() {
        const ret = wasm.ledger_nullifier_root(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * A ledger saved with [`Ledger::snapshot`]. The caller still has to hold
     * the result against a pool cell on chain: a snapshot is a cache.
     * @param {Uint8Array | number[]} pool_args
     * @param {string} snapshot
     * @returns {Ledger}
     */
    static restore(pool_args, snapshot) {
        let ptr0, len0, ptr1, len1;
        try {
            ptr0 = passArray8ToWasm0(pool_args, wasm.__wbindgen_malloc);
            len0 = WASM_VECTOR_LEN;
            ptr1 = passStringToWasm0(snapshot, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
            len1 = WASM_VECTOR_LEN;
        } catch (e) {
            if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
            if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
            throw e;
        }
        const ret = wasm.ledger_restore(ptr0, len0, ptr1, len1);
        if (ret[2]) {
            throw takeFromExternrefTable0(ret[1]);
        }
        return Ledger.__wrap(ret[0]);
    }
    /**
     * @returns {string}
     */
    snapshot() {
        let deferred1_0;
        let deferred1_1;
        try {
            const ret = wasm.ledger_snapshot(this.__wbg_ptr);
            deferred1_0 = ret[0];
            deferred1_1 = ret[1];
            return getStringFromWasm0(ret[0], ret[1]);
        } finally {
            wasm.__wbindgen_free(deferred1_0, deferred1_1, 1);
        }
    }
    /**
     * The pool cell's data this ledger stands for.
     * @returns {Uint8Array}
     */
    state_bytes() {
        const ret = wasm.ledger_state_bytes(this.__wbg_ptr);
        var v1 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
        return v1;
    }
    /**
     * Everything a withdrawal of the note at `leaf_index` needs before it is
     * proved: the input of the withdraw circuit (`input`, for snarkjs), and
     * the `root` and `nullifier_hash` the proof will be bound to, with the
     * public signals snarkjs has to report (`public`).
     *
     * Refuses a note whose secrets do not give that leaf, and one that is
     * withdrawn already.
     * @param {number} leaf_index
     * @param {Uint8Array | number[]} nullifier
     * @param {Uint8Array | number[]} secret
     * @param {Uint8Array | number[]} recipient_lock_hash
     * @param {bigint} fee
     * @returns {string}
     */
    withdraw_input(leaf_index, nullifier, secret, recipient_lock_hash, fee) {
        let deferred7_0;
        let deferred7_1;
        try {
            const val4 = +leaf_index;
            const val5 = BigInt.asIntN(64, fee);
            let ptr0, len0, ptr1, len1, ptr2, len2;
            try {
                ptr0 = passArray8ToWasm0(nullifier, wasm.__wbindgen_malloc);
                len0 = WASM_VECTOR_LEN;
                ptr1 = passArray8ToWasm0(secret, wasm.__wbindgen_malloc);
                len1 = WASM_VECTOR_LEN;
                ptr2 = passArray8ToWasm0(recipient_lock_hash, wasm.__wbindgen_malloc);
                len2 = WASM_VECTOR_LEN;
            } catch (e) {
                if (ptr2) { wasm.__wbindgen_free(ptr2, len2 * 1, 1); }
                if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
                if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
                throw e;
            }
            const ret = wasm.ledger_withdraw_input(this.__wbg_ptr, val4, ptr0, len0, ptr1, len1, ptr2, len2, val5);
            var ptr6 = ret[0];
            var len6 = ret[1];
            if (ret[3]) {
                ptr6 = 0; len6 = 0;
                throw takeFromExternrefTable0(ret[2]);
            }
            deferred7_0 = ptr6;
            deferred7_1 = len6;
            return getStringFromWasm0(ptr6, len6);
        } finally {
            wasm.__wbindgen_free(deferred7_0, deferred7_1, 1);
        }
    }
}
if (Symbol.dispose) Ledger.prototype[Symbol.dispose] = Ledger.prototype.free;

/**
 * CKB's default hash: blake2b-256 personalised with `ckb-default-hash`.
 * @param {Uint8Array | number[]} data
 * @returns {Uint8Array}
 */
export function ckb_hash(data) {
    const ptr0 = passArray8ToWasm0(data, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.ckb_hash(ptr0, len0);
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
}

/**
 * `Poseidon(nullifier, secret)`: the leaf a note puts into the tree.
 * @param {Uint8Array | number[]} nullifier
 * @param {Uint8Array | number[]} secret
 * @returns {Uint8Array}
 */
export function commitment(nullifier, secret) {
    let ptr0, len0, ptr1, len1;
    try {
        ptr0 = passArray8ToWasm0(nullifier, wasm.__wbindgen_malloc);
        len0 = WASM_VECTOR_LEN;
        ptr1 = passArray8ToWasm0(secret, wasm.__wbindgen_malloc);
        len1 = WASM_VECTOR_LEN;
    } catch (e) {
        if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
        if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
        throw e;
    }
    const ret = wasm.commitment(ptr0, len0, ptr1, len1);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v3 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v3;
}

/**
 * The withdrawals of a batch action, as a JSON array; `undefined` if the
 * bytes are not one well-formed batch.
 * @param {Uint8Array | number[]} action
 * @returns {string | undefined}
 */
export function decode_batch(action) {
    const ptr0 = passArray8ToWasm0(action, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.decode_batch(ptr0, len0);
    let v2;
    if (ret[0] !== 0) {
        v2 = getStringFromWasm0(ret[0], ret[1]);
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    }
    return v2;
}

/**
 * The 104 bytes of an intent lock's args.
 *
 * `reclaim_after` is a number of blocks, not an encoded `since`: a value of
 * 2^56 or more is refused here, because the cell it would make could never be
 * taken back and no script would say so when it is created.
 * @param {Uint8Array | number[]} pool_type_hash
 * @param {Uint8Array | number[]} commitment
 * @param {Uint8Array | number[]} owner_lock_hash
 * @param {bigint} reclaim_after
 * @returns {Uint8Array}
 */
export function intent_args(pool_type_hash, commitment, owner_lock_hash, reclaim_after) {
    const val4 = BigInt.asIntN(64, reclaim_after);
    let ptr0, len0, ptr1, len1, ptr2, len2;
    try {
        ptr0 = passArray8ToWasm0(pool_type_hash, wasm.__wbindgen_malloc);
        len0 = WASM_VECTOR_LEN;
        ptr1 = passArray8ToWasm0(commitment, wasm.__wbindgen_malloc);
        len1 = WASM_VECTOR_LEN;
        ptr2 = passArray8ToWasm0(owner_lock_hash, wasm.__wbindgen_malloc);
        len2 = WASM_VECTOR_LEN;
    } catch (e) {
        if (ptr2) { wasm.__wbindgen_free(ptr2, len2 * 1, 1); }
        if (ptr1) { wasm.__wbindgen_free(ptr1, len1 * 1, 1); }
        if (ptr0) { wasm.__wbindgen_free(ptr0, len0 * 1, 1); }
        throw e;
    }
    const ret = wasm.intent_args(ptr0, len0, ptr1, len1, ptr2, len2, val4);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v5 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v5;
}

/**
 * Whether 32 bytes are a canonical field element; a commitment has to be one.
 * @param {Uint8Array | number[]} bytes
 * @returns {boolean}
 */
export function is_field_element(bytes) {
    const ptr0 = passArray8ToWasm0(bytes, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.is_field_element(ptr0, len0);
    return ret !== 0;
}

/**
 * `Poseidon(nullifier, leaf_index)`: what a withdrawal reveals.
 * @param {Uint8Array | number[]} nullifier
 * @param {number} leaf_index
 * @returns {Uint8Array}
 */
export function nullifier_hash(nullifier, leaf_index) {
    const val2 = +leaf_index;
    const ptr0 = passArray8ToWasm0(nullifier, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.nullifier_hash(ptr0, len0, val2);
    if (ret[3]) {
        throw takeFromExternrefTable0(ret[2]);
    }
    var v3 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v3;
}

/**
 * The fields of an intent lock's args as JSON, or `undefined` if they are not
 * 104 bytes. `reclaimable` says whether any `since` can express the wait.
 * @param {Uint8Array | number[]} args
 * @returns {string | undefined}
 */
export function parse_intent_args(args) {
    const ptr0 = passArray8ToWasm0(args, wasm.__wbindgen_malloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.parse_intent_args(ptr0, len0);
    let v2;
    if (ret[0] !== 0) {
        v2 = getStringFromWasm0(ret[0], ret[1]);
        wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    }
    return v2;
}

/**
 * The fields of a v2 pool type script's args, as JSON. `hash_type` is the
 * byte the script carries (4 is `data2`).
 * @param {Uint8Array | number[]} args
 * @returns {string}
 */
export function pool_config(args) {
    let deferred3_0;
    let deferred3_1;
    try {
        const ptr0 = passArray8ToWasm0(args, wasm.__wbindgen_malloc);
        const len0 = WASM_VECTOR_LEN;
        const ret = wasm.pool_config(ptr0, len0);
        var ptr2 = ret[0];
        var len2 = ret[1];
        if (ret[3]) {
            ptr2 = 0; len2 = 0;
            throw takeFromExternrefTable0(ret[2]);
        }
        deferred3_0 = ptr2;
        deferred3_1 = len2;
        return getStringFromWasm0(ptr2, len2);
    } finally {
        wasm.__wbindgen_free(deferred3_0, deferred3_1, 1);
    }
}

/**
 * snarkjs' `proof.json` as the 128 bytes the pool's verifier reads.
 * @param {string} json
 * @returns {Uint8Array}
 */
export function proof_from_snarkjs(json) {
    const ptr0 = passStringToWasm0(json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.proof_from_snarkjs(ptr0, len0);
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
}

/**
 * The `since` the input of a reclaim needs after a wait of `blocks` blocks.
 * @param {bigint} blocks
 * @returns {bigint}
 */
export function reclaim_since(blocks) {
    const ret = wasm.reclaim_since(blocks);
    if (ret[2]) {
        throw takeFromExternrefTable0(ret[1]);
    }
    return BigInt.asUintN(64, ret[0]);
}

/**
 * snarkjs' `verification_key.json` as the bytes of the verifying-key cell.
 * Their [`ckb_hash`] has to be the `vk_hash` of the pool's args.
 * @param {string} json
 * @returns {Uint8Array}
 */
export function vk_from_snarkjs(json) {
    const ptr0 = passStringToWasm0(json, wasm.__wbindgen_malloc, wasm.__wbindgen_realloc);
    const len0 = WASM_VECTOR_LEN;
    const ret = wasm.vk_from_snarkjs(ptr0, len0);
    var v2 = getArrayU8FromWasm0(ret[0], ret[1]).slice();
    wasm.__wbindgen_free(ret[0], ret[1] * 1, 1);
    return v2;
}
function __wbg_get_imports() {
    const import0 = {
        __proto__: null,
        __wbg_Error_208446807d0d1e30: function(arg0, arg1) {
            const ret = Error(getStringFromWasm0(arg0, arg1));
            return ret;
        },
        __wbg___wbindgen_throw_4c88d8bf264fd984: function(arg0, arg1) {
            throw new Error(getStringFromWasm0(arg0, arg1));
        },
        __wbindgen_init_externref_table: function() {
            const table = wasm.__wbindgen_externrefs;
            table.set(0, undefined);
            table.set(1024, undefined);
            table.set(1025, null);
            table.set(1026, true);
            table.set(1027, false);
        },
    };
    return {
        __proto__: null,
        "./mixer_wasm_bg.js": import0,
    };
}

const LedgerFinalization = (typeof FinalizationRegistry === 'undefined')
    ? { register: () => {}, unregister: () => {} }
    : new FinalizationRegistry(ptr => wasm.__wbg_ledger_free(ptr, 1));

function getArrayU8FromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return getUint8ArrayMemory0().subarray(ptr / 1, ptr / 1 + len);
}

function getStringFromWasm0(ptr, len) {
    ptr = ptr >>> 0;
    return decodeText(getUint8ArrayMemory0().subarray(ptr, ptr + len));
}

let cachedUint8ArrayMemory0 = null;
function getUint8ArrayMemory0() {
    if (cachedUint8ArrayMemory0 === null || cachedUint8ArrayMemory0.byteLength === 0) {
        cachedUint8ArrayMemory0 = new Uint8Array(wasm.memory.buffer);
    }
    return cachedUint8ArrayMemory0;
}

function passArray8ToWasm0(arg, malloc) {
    const ptr = malloc(arg.length * 1, 1) >>> 0;
    getUint8ArrayMemory0().set(arg, ptr / 1);
    WASM_VECTOR_LEN = arg.length;
    return ptr;
}

function passStringToWasm0(arg, malloc, realloc) {
    if (realloc === undefined) {
        const buf = cachedTextEncoder.encode(arg);
        const ptr = malloc(buf.length, 1) >>> 0;
        getUint8ArrayMemory0().subarray(ptr, ptr + buf.length).set(buf);
        WASM_VECTOR_LEN = buf.length;
        return ptr;
    }

    let len = arg.length;
    let ptr = malloc(len, 1) >>> 0;

    const mem = getUint8ArrayMemory0();

    let offset = 0;

    for (; offset < len; offset++) {
        const code = arg.charCodeAt(offset);
        if (code > 0x7F) break;
        mem[ptr + offset] = code;
    }
    if (offset !== len) {
        if (offset !== 0) {
            arg = arg.slice(offset);
        }
        ptr = realloc(ptr, len, len = offset + arg.length * 3, 1) >>> 0;
        const view = getUint8ArrayMemory0().subarray(ptr + offset, ptr + len);
        const ret = cachedTextEncoder.encodeInto(arg, view);

        offset += ret.written;
        ptr = realloc(ptr, len, offset, 1) >>> 0;
    }

    WASM_VECTOR_LEN = offset;
    return ptr;
}

function takeFromExternrefTable0(idx) {
    const value = wasm.__wbindgen_externrefs.get(idx);
    wasm.__externref_table_dealloc(idx);
    return value;
}

let cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true });
cachedTextDecoder.decode();
const MAX_SAFARI_DECODE_BYTES = 2146435072;
let numBytesDecoded = 0;
function decodeText(bytes) {
    numBytesDecoded += bytes.length;
    if (numBytesDecoded >= MAX_SAFARI_DECODE_BYTES) {
        cachedTextDecoder = new TextDecoder('utf-8', { ignoreBOM: true });
        cachedTextDecoder.decode();
        numBytesDecoded = bytes.length;
    }
    return cachedTextDecoder.decode(bytes);
}

const cachedTextEncoder = new TextEncoder();

if (!('encodeInto' in cachedTextEncoder)) {
    cachedTextEncoder.encodeInto = function (arg, view) {
        const buf = cachedTextEncoder.encode(arg);
        view.set(buf);
        return {
            read: arg.length,
            written: buf.length
        };
    };
}

let WASM_VECTOR_LEN = 0;

let wasmModule, wasmInstance, wasm;
function __wbg_finalize_init(instance, module) {
    wasmInstance = instance;
    wasm = instance.exports;
    wasmModule = module;
    cachedUint8ArrayMemory0 = null;
    wasm.__wbindgen_start();
    return wasm;
}

async function __wbg_load(module, imports) {
    if (typeof Response === 'function' && module instanceof Response) {
        if (!module.ok) {
            throw new Error(`failed to fetch Wasm: ${module.status} ${module.statusText} fetching '${module.url}'`);
        }

        if (typeof WebAssembly.instantiateStreaming === 'function') {
            try {
                return await WebAssembly.instantiateStreaming(module, imports);
            } catch (e) {
                const validResponse = expectedResponseType(module.type);

                if (validResponse && module.headers.get('Content-Type') !== 'application/wasm') {
                    console.warn("`WebAssembly.instantiateStreaming` failed because your server does not serve Wasm with `application/wasm` MIME type. Falling back to `WebAssembly.instantiate` which is slower. Original error:\n", e);

                } else { throw e; }
            }
        }

        const bytes = await module.arrayBuffer();
        return await WebAssembly.instantiate(bytes, imports);
    } else {
        const instance = await WebAssembly.instantiate(module, imports);

        if (instance instanceof WebAssembly.Instance) {
            return { instance, module };
        } else {
            return instance;
        }
    }

    function expectedResponseType(type) {
        switch (type) {
            case 'basic': case 'cors': case 'default': return true;
        }
        return false;
    }
}

function initSync(module) {
    if (wasm !== undefined) return wasm;


    if (module !== undefined) {
        if (Object.getPrototypeOf(module) === Object.prototype) {
            ({module} = module)
        } else {
            console.warn('using deprecated parameters for `initSync()`; pass a single object instead')
        }
    }

    const imports = __wbg_get_imports();
    if (!(module instanceof WebAssembly.Module)) {
        module = new WebAssembly.Module(module);
    }
    const instance = new WebAssembly.Instance(module, imports);
    return __wbg_finalize_init(instance, module);
}

let __wbg_init_promise;
async function __wbg_init(module_or_path) {
    if (wasm !== undefined) return wasm;
    if (__wbg_init_promise === undefined) {
        __wbg_init_promise = __wbg_init_once(module_or_path);
        __wbg_init_promise.catch(() => { __wbg_init_promise = undefined; });
    }
    return __wbg_init_promise;
}

async function __wbg_init_once(module_or_path) {

    if (module_or_path !== undefined) {
        if (Object.getPrototypeOf(module_or_path) === Object.prototype) {
            ({module_or_path} = module_or_path)
        } else {
            console.warn('using deprecated parameters for the initialization function; pass a single object instead')
        }
    }

    if (module_or_path === undefined) {
        module_or_path = new URL('mixer_wasm_bg.wasm', import.meta.url);
    }
    const imports = __wbg_get_imports();

    if (typeof module_or_path === 'string' || (typeof Request === 'function' && module_or_path instanceof Request) || (typeof URL === 'function' && module_or_path instanceof URL)) {
        module_or_path = fetch(module_or_path);
    }

    const { instance, module } = await __wbg_load(await module_or_path, imports);

    return __wbg_finalize_init(instance, module);
}

export { initSync, __wbg_init as default };
