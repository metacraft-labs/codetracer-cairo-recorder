// StarkNet contract pin for the syscall trace shape exercised by the
// M10 round-3 syscalls_test fixture.
//
// The recorder does not compile this source through the Sierra/CASM
// pipeline — it has no `fn main` and the
// `#[starknet::contract]`/`syscalls::*` plumbing requires the dispatcher
// runtime that the in-process Sierra runner does not provide.  Instead
// the matching test pipes the canonical `syscalls_test_trace.json`
// snforge fixture through `starknet::write_starknet_trace`, which now
// recognises a new `syscall` entry type and converts each one into a
// Call/Return frame named `<contract>::<syscall_name>` with the syscall
// return value typed appropriately (addresses surface as
// `ValueRecord::Raw` carrying the 32-byte big-endian felt; the
// `get_block_timestamp` return surfaces as `ValueRecord::Int { i, u64 }`).
//
// The Cairo source lives alongside the JSON to document the contract
// shape that produces the trace.
use starknet::ContractAddress;
use starknet::get_caller_address;
use starknet::get_block_timestamp;
use starknet::get_contract_address;

#[starknet::interface]
trait IInfo<TContractState> {
    fn snapshot(self: @TContractState) -> (ContractAddress, u64, ContractAddress);
}

#[starknet::contract]
mod Info {
    use starknet::ContractAddress;
    use starknet::get_caller_address;
    use starknet::get_block_timestamp;
    use starknet::get_contract_address;

    #[storage]
    struct Storage {}

    #[abi(embed_v0)]
    impl InfoImpl of super::IInfo<ContractState> {
        fn snapshot(self: @ContractState) -> (ContractAddress, u64, ContractAddress) {
            let caller = get_caller_address();
            let ts = get_block_timestamp();
            let me = get_contract_address();
            (caller, ts, me)
        }
    }
}
