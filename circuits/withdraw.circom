pragma circom 2.1.6;

include "circomlib/circuits/poseidon.circom";

// Merkle path over Poseidon(left, right).
// pathIndices[i] == 1 means the running node is the RIGHT child at level i.
// Also outputs the leaf index encoded by the path bits (level 0 = least significant bit).
template MerklePath(levels) {
    signal input leaf;
    signal input pathElements[levels];
    signal input pathIndices[levels];
    signal output root;
    signal output index;

    signal cur[levels + 1];
    signal acc[levels + 1];
    signal left[levels];
    signal right[levels];
    component hashers[levels];

    cur[0] <== leaf;
    acc[0] <== 0;

    for (var i = 0; i < levels; i++) {
        pathIndices[i] * (1 - pathIndices[i]) === 0;

        // bit = 0: (left, right) = (cur, sibling); bit = 1: (sibling, cur)
        left[i] <== cur[i] + pathIndices[i] * (pathElements[i] - cur[i]);
        right[i] <== pathElements[i] + cur[i] - left[i];

        hashers[i] = Poseidon(2);
        hashers[i].inputs[0] <== left[i];
        hashers[i].inputs[1] <== right[i];
        cur[i + 1] <== hashers[i].out;

        acc[i + 1] <== acc[i] + pathIndices[i] * (1 << i);
    }

    root <== cur[levels];
    index <== acc[levels];
}

// Statement: "I know (nullifier, secret) such that
//   commitment    = Poseidon(nullifier, secret) is the leaf at position `index` under `root`
//   nullifierHash = Poseidon(nullifier, index)"
//
// The nullifier is bound to the leaf index, so the same commitment deposited
// twice yields two distinct nullifier hashes and both deposits stay withdrawable.
// CKB has no cheap on-chain "commitment already used" map, unlike Tornado's
// Solidity contract, so this is handled in the circuit instead.
//
// recipient and fee are bound to the proof so nobody can rebuild the (unsigned)
// withdrawal transaction with another payee or a larger fee.
template Withdraw(levels) {
    // public
    signal input root;
    signal input nullifierHash;
    signal input recipient;   // first 31 bytes of the recipient lock script hash, little-endian
    signal input fee;         // shannons deducted from the denomination

    // private
    signal input nullifier;
    signal input secret;
    signal input pathElements[levels];
    signal input pathIndices[levels];

    component commitment = Poseidon(2);
    commitment.inputs[0] <== nullifier;
    commitment.inputs[1] <== secret;

    component path = MerklePath(levels);
    path.leaf <== commitment.out;
    for (var i = 0; i < levels; i++) {
        path.pathElements[i] <== pathElements[i];
        path.pathIndices[i] <== pathIndices[i];
    }
    path.root === root;

    component nullifierHasher = Poseidon(2);
    nullifierHasher.inputs[0] <== nullifier;
    nullifierHasher.inputs[1] <== path.index;
    nullifierHasher.out === nullifierHash;

    // Put recipient and fee into real constraints. A public input that appears
    // in no constraint is not bound by a Groth16 proof.
    signal recipientSquare;
    signal feeSquare;
    recipientSquare <== recipient * recipient;
    feeSquare <== fee * fee;
}

component main {public [root, nullifierHash, recipient, fee]} = Withdraw(20);
