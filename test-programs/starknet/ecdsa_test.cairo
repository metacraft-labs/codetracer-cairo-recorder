// M10 round 5: `check_ecdsa_signature` syscall pin.
//
// `core::ecdsa::check_ecdsa_signature(message_hash, public_key,
// signature_r, signature_s) -> bool` is StarkNet's STARK-friendly
// signature-verification primitive.  The host VM exposes it as a
// dedicated syscall whose return is a pure boolean (true =
// signature valid against the public key).
//
// The recorder does not compile this source through the
// Sierra/CASM pipeline (the syscall is a host-provided primitive
// only available to a deployed contract).  Instead the matching
// test pipes the canonical `ecdsa_test_trace.json` snforge fixture
// through `starknet::write_starknet_trace`, which surfaces the
// syscall as a `<contract>::check_ecdsa_signature` Call/Return
// pair with the boolean result decoded as `ValueRecord::Bool`.
// The Cairo source lives alongside the JSON to document the
// contract that produced the trace.

use core::ecdsa::check_ecdsa_signature;

#[starknet::interface]
trait IVerifier<TContractState> {
    fn verify(
        self: @TContractState,
        message_hash: felt252,
        public_key: felt252,
        signature_r: felt252,
        signature_s: felt252,
    ) -> bool;
}

#[starknet::contract]
mod Verifier {
    use super::check_ecdsa_signature;

    #[storage]
    struct Storage {}

    #[abi(embed_v0)]
    impl VerifierImpl of super::IVerifier<ContractState> {
        fn verify(
            self: @ContractState,
            message_hash: felt252,
            public_key: felt252,
            signature_r: felt252,
            signature_s: felt252,
        ) -> bool {
            check_ecdsa_signature(message_hash, public_key, signature_r, signature_s)
        }
    }
}
