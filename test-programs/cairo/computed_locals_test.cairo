// computed_locals_test — Cairo
// Every local is produced by a runtime computation or a copy of one;
// none of them is returned on its own, so their values can only come
// from the executed program.
fn main() -> felt252 {
    let a: felt252 = 6;
    let b: felt252 = a * 7;
    let c: felt252 = b + 1;
    let d: felt252 = c;
    a + d
}
