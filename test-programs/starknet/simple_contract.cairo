#[starknet::interface]
trait ISimpleContract<TContractState> {
    fn increase_balance(ref self: TContractState, amount: felt252);
    fn get_balance(self: @TContractState) -> felt252;
}

#[starknet::contract]
mod SimpleContract {
    use starknet::ContractAddress;

    #[storage]
    struct Storage {
        balance: felt252,
        owner: ContractAddress,
    }

    #[event]
    #[derive(Drop, starknet::Event)]
    enum Event {
        BalanceIncreased: BalanceIncreased,
    }

    #[derive(Drop, starknet::Event)]
    struct BalanceIncreased {
        #[key]
        caller: ContractAddress,
        amount: felt252,
        new_balance: felt252,
    }

    #[abi(embed_v0)]
    impl SimpleContractImpl of super::ISimpleContract<ContractState> {
        fn increase_balance(ref self: ContractState, amount: felt252) {
            let current = self.balance.read();
            let new_balance = current + amount;
            self.balance.write(new_balance);
            self.emit(BalanceIncreased {
                caller: starknet::get_caller_address(),
                amount: amount,
                new_balance: new_balance,
            });
        }

        fn get_balance(self: @ContractState) -> felt252 {
            self.balance.read()
        }
    }
}
