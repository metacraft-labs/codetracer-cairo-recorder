// StarkNet contract pin for the `#[event]` / `self.emit(...)` shape
// exercised by the M10 round-2 event_test fixture.
//
// The recorder does not compile this source through the Sierra/CASM
// pipeline (it has no `fn main` and the `#[starknet::contract]`
// attribute requires the dispatcher plumbing).  Instead the matching
// test pipes the canonical `event_test_trace.json` snforge fixture
// through `starknet::write_starknet_trace`, which converts the
// `event` entry into the same `register_special_event(EvmEvent, ...)`
// shape that real snforge runs produce.  The Cairo source lives
// alongside the JSON to document the contract that produced the trace.
use starknet::ContractAddress;

#[starknet::interface]
trait IToken<TContractState> {
    fn transfer(ref self: TContractState, from: ContractAddress, to: ContractAddress, amount: u128);
}

#[starknet::contract]
mod Token {
    use starknet::ContractAddress;

    #[storage]
    struct Storage {}

    #[event]
    #[derive(Drop, starknet::Event)]
    enum Event {
        Transfer: TransferEvent,
    }

    #[derive(Drop, starknet::Event)]
    struct TransferEvent {
        #[key]
        from: ContractAddress,
        to: ContractAddress,
        amount: u128,
    }

    #[abi(embed_v0)]
    impl TokenImpl of super::IToken<ContractState> {
        fn transfer(
            ref self: ContractState,
            from: ContractAddress,
            to: ContractAddress,
            amount: u128,
        ) {
            self.emit(TransferEvent { from, to, amount });
        }
    }
}
