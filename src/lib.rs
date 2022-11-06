#![feature(
    allocator_api,
    let_chains,
    array_windows,
    array_chunks,
    iter_partition_in_place,
    slice_flatten,
    slice_as_chunks
)]

#[macro_use]
mod macros;
mod air;
pub mod challenges;
mod channel;
mod composer;
pub mod constraint;
pub mod fri;
mod matrix;
pub mod merkle;
mod prover;
mod random;
mod trace;
pub mod utils;
mod verifier;

pub use air::Air;
use ark_serialize::CanonicalDeserialize;
use ark_serialize::CanonicalSerialize;
pub use constraint::Column;
pub use constraint::Constraint;
use fri::FriOptions;
use fri::FriProof;
pub use matrix::Matrix;
pub use prover::Prover;
use trace::Queries;
pub use trace::Trace;
pub use trace::TraceInfo;

// TODO: include ability to specify:
// - base field
// - extension field
// - hashing function
#[derive(Debug, Clone, Copy, CanonicalSerialize, CanonicalDeserialize)]
pub struct ProofOptions {
    pub num_queries: u8,
    pub lde_blowup_factor: u8,
    pub grinding_factor: u8,
    pub fri_folding_factor: u8,
    pub fri_max_remainder_size: u8,
}

impl ProofOptions {
    pub const MIN_NUM_QUERIES: u8 = 1;
    pub const MAX_NUM_QUERIES: u8 = 128;
    pub const MIN_BLOWUP_FACTOR: u8 = 2;
    pub const MAX_BLOWUP_FACTOR: u8 = 64;
    pub const MAX_GRINDING_FACTOR: u8 = 32;

    pub fn new(
        num_queries: u8,
        lde_blowup_factor: u8,
        grinding_factor: u8,
        fri_folding_factor: u8,
        fri_max_remainder_size: u8,
    ) -> Self {
        assert!(num_queries >= Self::MIN_NUM_QUERIES);
        assert!(num_queries <= Self::MAX_NUM_QUERIES);
        assert!(lde_blowup_factor.is_power_of_two());
        assert!(lde_blowup_factor >= Self::MIN_BLOWUP_FACTOR);
        assert!(lde_blowup_factor <= Self::MAX_BLOWUP_FACTOR);
        assert!(grinding_factor <= Self::MAX_GRINDING_FACTOR);
        ProofOptions {
            num_queries,
            lde_blowup_factor,
            grinding_factor,
            fri_folding_factor,
            fri_max_remainder_size,
        }
    }

    pub fn into_fri_options(self) -> FriOptions {
        // TODO: move fri params into struct
        FriOptions::new(
            self.lde_blowup_factor.into(),
            self.fri_folding_factor.into(),
            self.fri_max_remainder_size.into(),
        )
    }
}

/// A proof generated by a mini-stark prover
#[derive(CanonicalSerialize, CanonicalDeserialize, Clone)]
pub struct Proof<A: Air> {
    pub options: ProofOptions,
    pub trace_info: TraceInfo,
    pub base_trace_commitment: Vec<u8>,
    pub extension_trace_commitment: Option<Vec<u8>>,
    pub composition_trace_commitment: Vec<u8>,
    pub fri_proof: FriProof<A::Fp>,
    pub pow_nonce: u64,
    pub trace_queries: Queries<A::Fp>,
    pub public_inputs: A::PublicInputs,
    pub ood_trace_states: (Vec<A::Fp>, Vec<A::Fp>),
    pub ood_constraint_evaluations: Vec<A::Fp>,
}
