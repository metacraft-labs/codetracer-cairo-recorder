// M10 round 4: visibility-decorator pin (`#[external(v0)]` vs
// `#[view]` vs internal `fn`).
//
// StarkNet's ABI distinguishes three visibility classes for contract
// methods:
//
//   * `#[external(v0)]` — state-mutating; takes `ref self:
//     ContractState`.
//   * `#[view]` — read-only; takes `@self: @ContractState` (snapshot).
//   * undecorated `fn` — internal helper; not exposed via the ABI,
//     only callable from other methods of the same contract.
//
// The recorder doesn't compile this source through the Sierra/CASM
// pipeline — `#[starknet::contract]` requires the dispatcher runtime
// that the in-process Sierra runner does not provide.  The matching
// test pipes the canonical `visibility_decorators_test_trace.json`
// snforge fixture through `starknet::write_starknet_trace`, which
// recognises the new `visibility` / `self_kind` fields on each
// `contract_call` entry and:
//
//   * writes the function name as
//     `<visibility>::<contract>::<selector>` so the function table
//     groups external / view / internal visibility classes.
//   * emits a typed `ValueRecord::Reference` `self_kind` arg whose
//     `mutable` flag is `true` for `ref self` (external) and `false`
//     for `@self` (view).  Internal callers omit the `self_kind`
//     field so the arg is dropped — pinning that the recorder keeps
//     the @-vs-ref distinction visible at the trace level.

#[starknet::interface]
trait IBank<TContractState> {
    fn deposit(ref self: TContractState, amount: u128);
    fn balance_of(self: @TContractState) -> u128;
}

#[starknet::contract]
mod Bank {
    use starknet::ContractAddress;

    #[storage]
    struct Storage {
        balance: u128,
    }

    #[abi(embed_v0)]
    impl BankImpl of super::IBank<ContractState> {
        // External — state-mutating; `ref self`.
        fn deposit(ref self: ContractState, amount: u128) {
            let validated = self.validate(amount);
            let cur: u128 = self.balance.read();
            self.balance.write(cur + validated);
        }

        // View — read-only; `@self` (snapshot).
        fn balance_of(self: @ContractState) -> u128 {
            self.balance.read()
        }
    }

    // Internal — no ABI decorator; not exposed externally.
    #[generate_trait]
    impl Internal of InternalTrait {
        fn validate(self: @ContractState, amount: u128) -> u128 {
            amount
        }
    }
}
