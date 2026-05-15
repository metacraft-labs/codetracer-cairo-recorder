// M10 round 3: deep `match` pattern pin.
//
// `Result<Option<u32>, felt252>` is a two-level Variant.  `classify`
// dispatches over the three arms `Ok(Some(n))`, `Ok(None)`, and
// `Err(_)` and returns a witness u32 per arm.  The driver
// (`run_all`) calls `classify` three times — once per arm — so the
// trace surfaces one Step event per source line and a deterministic
// classify return value per call.
//
// Strict pin (current recorder behaviour): the recorder's
// `parse_variant_literal_decl` recogniser only matches single-level
// Option/Result literals with integer payloads — nested constructors
// like `Result::Ok(Option::Some(7))` fall through and surface as
// scalar Int let-bindings (no Variant ValueRecord at the let site).
// The pin therefore asserts on:
//   * the function table (DFS order from main).
//   * the per-line step sequence pinned by `assert_step_indices_monotonic`.
//   * `classify`'s return value across each of the three calls (7,
//     100, 200) on the call_exit events.
//   * the synthetic `return_value` step variable for main.
//
// True per-arm pattern-binding extraction (the matched `n` becoming a
// typed Int local) stays for round 4 — the static recorder cannot
// yet trace pattern-binding side-effects from a `match` expression
// without a Sierra-side variable map.

#[inline(never)]
fn classify(input: Result<Option<u32>, felt252>) -> u32 {
    match input {
        Result::Ok(opt) => match opt {
            Option::Some(n) => n,
            Option::None => 100,
        },
        Result::Err(_) => 200,
    }
}

fn run_all() -> u32 {
    let some_val: Result<Option<u32>, felt252> = Result::Ok(Option::Some(7));
    let none_val: Result<Option<u32>, felt252> = Result::Ok(Option::None);
    let err_val: Result<Option<u32>, felt252> = Result::Err(99);
    let a: u32 = classify(some_val);
    let b: u32 = classify(none_val);
    let c: u32 = classify(err_val);
    a + b + c
}

fn main() -> felt252 {
    let total: u32 = run_all();
    total.into()
}
