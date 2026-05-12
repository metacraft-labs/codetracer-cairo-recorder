fn divide(numerator: felt252, divisor: felt252) -> felt252 {
    assert!(divisor != 0, "division by zero");
    numerator
}

fn compute() -> (felt252, felt252) {
    let a: felt252 = 10;
    let b: felt252 = divide(a, 0);
    (a, b)
}

fn main() -> (felt252, felt252) {
    compute()
}
