//! Reasons a pool transaction is rejected.
//!
//! The discriminant is the exit code of the pool type script, so a failing
//! transaction reports exactly which rule it broke.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum Error {
    // -- transaction shape
    /// A syscall failed unexpectedly.
    Syscall = 5,
    /// The type script args are not `denomination | vk_hash | lock_code_hash | lock_hash_type`.
    ScriptArgs = 6,
    /// The transaction does not create exactly one pool cell or update exactly one.
    GroupShape = 7,
    /// The output pool cell does not carry the pool lock bound to this type script.
    PoolLock = 8,
    /// The pool input has no `input_type` witness.
    Witness = 9,
    /// The witness is not a well-formed deposit or withdraw action.
    ActionEncoding = 10,
    /// The input pool cell data is not a well-formed state.
    StateEncoding = 11,
    /// A new pool cell does not start from the empty state.
    InitialState = 12,

    // -- deposit
    /// A commitment is not a canonical field element.
    NonCanonicalCommitment = 20,
    /// The commitment tree has no room for the deposit.
    TreeFull = 21,

    // -- withdraw
    /// The Merkle root is not a canonical field element.
    NonCanonicalRoot = 30,
    /// The nullifier hash is not a canonical field element.
    NonCanonicalNullifier = 31,
    /// The Merkle root is not in the pool's recent root history.
    UnknownRoot = 32,
    /// The nullifier-set proof is malformed.
    NullifierProof = 33,
    /// The nullifier is not provably unspent under the current nullifier-set root.
    NullifierSpent = 34,
    /// The fee exceeds the denomination.
    FeeTooLarge = 35,
    /// There is no output right after the pool cell to pay the recipient.
    RecipientMissing = 36,
    /// The recipient output has a type script or carries data.
    RecipientShape = 37,
    /// The recipient output does not hold exactly `denomination - fee`.
    RecipientCapacity = 38,
    /// No cell dep holds the verifying key named in the script args.
    VkNotFound = 39,
    /// The verifying key cell does not hold a valid key.
    InvalidVk = 40,
    /// The proof bytes are not a valid Groth16 proof encoding.
    InvalidProof = 41,
    /// The public inputs are rejected by the verifier.
    InvalidPublicInputs = 42,
    /// The verifying key was built for a different number of public inputs.
    PublicInputCountMismatch = 43,
    /// The pairing check failed: the proof does not match the public inputs.
    ProofRejected = 44,

    // -- state transition
    /// The output pool cell data is not the state this action produces.
    OutputState = 50,
    /// The output pool cell capacity is not the capacity this action produces.
    OutputCapacity = 51,
}

impl Error {
    pub const fn code(self) -> i8 {
        self as i8
    }
}
