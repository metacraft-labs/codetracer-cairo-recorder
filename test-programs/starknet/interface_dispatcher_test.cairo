// M10 round 5: `#[starknet::interface]` Dispatcher pattern.
//
// `IToken<TContractState>` is a typical StarkNet interface trait.
// The `#[starknet::interface]` macro generates the matching
// `IToken::Dispatcher` and `IToken::DispatcherTrait` types: a
// caller holding a `ContractAddress` can construct a
// `IToken::Dispatcher { contract_address }` value and invoke
// `balance_of(...)` / `transfer(...)` on it; the generated
// dispatcher routes the call through the StarkNet syscall
// `call_contract_syscall` to the target contract's matching
// external entry point.
//
// The recorder does not compile this source through the Sierra/CASM
// pipeline (it has no `fn main` and the `#[starknet::contract]` /
// `#[starknet::interface]` macros require dispatcher plumbing at
// link time).  Instead the matching test pipes the canonical
// `interface_dispatcher_test_trace.json` snforge fixture through
// `starknet::write_starknet_trace`, which surfaces each dispatched
// call with the trait identity (`IToken`) baked into both the
// function-table name (`IToken::<callee>::<selector>`) and a
// dedicated `dispatcher_trait` arg on the call entry.  The Cairo
// source lives alongside the JSON to document the contract that
// produced the trace.

#[starknet::interface]
trait IToken<TContractState> {
    fn balance_of(self: @TContractState, address: starknet::ContractAddress) -> u256;
    fn transfer(
        ref self: TContractState,
        recipient: starknet::ContractAddress,
        amount: u256,
    ) -> bool;
}

#[starknet::contract]
mod Caller {
    use super::{ITokenDispatcher, ITokenDispatcherTrait};
    use starknet::ContractAddress;

    #[storage]
    struct Storage {
        token_address: ContractAddress,
    }

    #[abi(embed_v0)]
    impl CallerImpl of ICaller<ContractState> {
        fn check_balance(self: @ContractState, who: ContractAddress) -> u256 {
            let token = ITokenDispatcher { contract_address: self.token_address.read() };
            token.balance_of(who)
        }

        fn forward_transfer(
            ref self: ContractState,
            recipient: ContractAddress,
            amount: u256,
        ) -> bool {
            let token = ITokenDispatcher { contract_address: self.token_address.read() };
            token.transfer(recipient, amount)
        }
    }
}

#[starknet::interface]
trait ICaller<TContractState> {
    fn check_balance(
        self: @TContractState,
        who: starknet::ContractAddress,
    ) -> u256;
    fn forward_transfer(
        ref self: TContractState,
        recipient: starknet::ContractAddress,
        amount: u256,
    ) -> bool;
}
