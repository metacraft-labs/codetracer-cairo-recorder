// M10 round 5: Cairo's `#[test]` attribute pin (documented gap).
//
// Cairo's `#[test]` attribute marks free functions as unit tests
// runnable by `scarb test` / `cairo-test`.  The attribute itself is
// implemented by the `cairo-lang-test-plugin` crate, which the
// recorder does *not* link in today (the in-process Sierra runner
// has no test harness — only `main` is actually executed).  Loading
// the plugin would pull in the full `cairo-test` runner and reach
// outside the M10 scope; this fixture instead pins the strict
// shape recorded today: the `#[test]`-shaped helper functions
// (`add_test`, `sub_test`, `mul_test`) are written as regular
// `fn` so the recorder's stable compiler accepts them, and `main`
// invokes each one in sequence — the strict assertion is that
// every helper surfaces as its own Function entry in the
// recorder's function table with a balanced Call/Return pair.
// Surfacing each `#[test]`-attributed function as its own
// independent trace (the spec's M11 multi-trace invariant) is a
// downstream extension; this fixture pins the gap so a future
// test-plugin integration lands as a *new* test variable rather
// than silently changing the contract.
//
// Computed values:
//   * `add_test()` → `1 + 2 = 3`.
//   * `sub_test()` → `5 - 1 = 4`.
//   * `mul_test()` → `3 * 6 = 18`.
//   * `main()` → `3 + 4 + 18 = 25`.

fn add_test() -> felt252 {
    let a: felt252 = 1;
    let b: felt252 = 2;
    a + b
}

fn sub_test() -> felt252 {
    let a: felt252 = 5;
    let b: felt252 = 1;
    a - b
}

fn mul_test() -> felt252 {
    let a: felt252 = 3;
    let b: felt252 = 6;
    a * b
}

fn main() -> felt252 {
    let s: felt252 = add_test();
    let d: felt252 = sub_test();
    let m: felt252 = mul_test();
    s + d + m
}
