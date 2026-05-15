// M10 round 4: StarkNet component pin.
//
// `#[starknet::component]` (Cairo 2.5+) is the reusable-behaviour
// module form: a component declares its own `#[storage]` struct and
// functions, then a host contract embeds it via `component!(path:
// my_component, storage: my_state, event: MyEvent);` and exposes
// the component's external functions via `#[abi(embed_v0)] impl
// MyImpl = my_component::MyImpl<ContractState>;`.
//
// At trace time the component's functions surface under their
// module-qualified path (e.g. `ownable_component::transfer_ownership`)
// rather than under the host contract's flat selector namespace, and
// the component's storage reads / writes carry the component path in
// the storage key so consumers can distinguish a host-level slot from
// a component-embedded slot at the same physical storage offset.
//
// The recorder doesn't compile this source through the Sierra/CASM
// pipeline — `#[starknet::contract]` / `component!()` requires the
// dispatcher runtime that the in-process Sierra runner does not
// provide.  The matching test pipes the canonical
// `component_test_trace.json` snforge fixture through
// `starknet::write_starknet_trace`, which already emits Call/Return
// frames named `<contract>::<selector>` and StorageRead /
// StorageWrite tagged io_events — the strict pin asserts that the
// component-prefixed selector / key strings flow through unchanged.

#[starknet::interface]
trait IOwnable<TContractState> {
    fn owner(self: @TContractState) -> starknet::ContractAddress;
    fn transfer_ownership(ref self: TContractState, new_owner: starknet::ContractAddress);
}

#[starknet::component]
mod ownable_component {
    use starknet::ContractAddress;

    #[storage]
    struct Storage {
        owner: ContractAddress,
    }

    #[embeddable_as(OwnableImpl)]
    impl Ownable<
        TContractState, +HasComponent<TContractState>
    > of super::IOwnable<ComponentState<TContractState>> {
        fn owner(self: @ComponentState<TContractState>) -> ContractAddress {
            self.owner.read()
        }

        fn transfer_ownership(
            ref self: ComponentState<TContractState>, new_owner: ContractAddress
        ) {
            self.owner.write(new_owner);
        }
    }
}

#[starknet::contract]
mod Host {
    use super::ownable_component;

    component!(path: ownable_component, storage: ownable, event: OwnableEvent);

    #[abi(embed_v0)]
    impl OwnableImpl = ownable_component::OwnableImpl<ContractState>;

    #[storage]
    struct Storage {
        #[substorage(v0)]
        ownable: ownable_component::Storage,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    enum Event {
        OwnableEvent: ownable_component::Event,
    }
}
