// StarkNet contract pin for the storage_read / storage_write event
// shape exercised by the M4 SnforgeTrace path.
//
// The recorder does not compile this source through the Sierra/CASM
// pipeline (it has no `fn main` and the `#[starknet::contract]` attribute
// requires the dispatcher plumbing).  Instead the matching test pipes
// the canonical `storage_test_trace.json` snforge fixture through
// `starknet::write_starknet_trace`, which converts the storage
// read/write entries into the same event shape that real snforge runs
// produce.  The Cairo source lives alongside the JSON to document the
// contract that produced the trace.
#[starknet::interface]
trait ICounter<TContractState> {
    fn increment(ref self: TContractState, by: felt252);
    fn get(self: @TContractState) -> felt252;
}

#[starknet::contract]
mod Counter {
    #[storage]
    struct Storage {
        value: felt252,
    }

    #[abi(embed_v0)]
    impl CounterImpl of super::ICounter<ContractState> {
        fn increment(ref self: ContractState, by: felt252) {
            let current = self.value.read();
            self.value.write(current + by);
        }

        fn get(self: @ContractState) -> felt252 {
            self.value.read()
        }
    }
}
