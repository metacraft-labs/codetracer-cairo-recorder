fn inner(a: felt252, b: felt252) -> felt252 {
    a + b
}

fn middle(x: felt252) -> felt252 {
    inner(x, 10)
}

fn outer(p: felt252) -> felt252 {
    middle(p) + 100
}

fn compute() -> (felt252, felt252, felt252, felt252) {
    let a: felt252 = 1;
    let b: felt252 = inner(a, 2);
    let c: felt252 = middle(a);
    let d: felt252 = outer(a);
    (a, b, c, d)
}

fn main() -> (felt252, felt252, felt252, felt252) {
    compute()
}
