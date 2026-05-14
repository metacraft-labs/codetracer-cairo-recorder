fn require_positive(n: felt252) -> felt252 {
    assert!(n != 0, "value was zero");
    n
}

fn compute() -> felt252 {
    let a: felt252 = 7;
    let b: felt252 = require_positive(0);
    a + b
}

fn main() -> felt252 {
    compute()
}
