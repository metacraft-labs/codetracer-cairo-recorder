// M10 round 5: closure expressions / lambda forms (Cairo 2.7+).
//
// Cairo's closure syntax supports inline anonymous functions
// `|args| body` that may capture surrounding bindings by snapshot.
// This fixture exercises both shapes:
//   * `no_capture()` defines a closure with no captures and invokes
//     it directly (`|x| x + 32`).
//   * `with_capture()` defines a closure that captures an enclosing
//     `bias` binding and invokes it (`|x| x + bias`).
// Higher-order generic dispatch (passing a closure across a function
// boundary as an `impl Fn` trait object) requires the
// `unstable-feature("type-constraints")` flag in the crate config —
// the recorder does not enable that flag, so this fixture exercises
// only the inline-closure shape that the stable compiler supports
// out of the box.
//
// Strict pin (current recorder behaviour): both driver functions
// surface in the function table alongside `main`; the closure bodies
// and the `core::ops::FnOnce::call` corelib dispatch are inlined by
// the Sierra optimiser at the call sites and do not surface as
// their own frames in the recorder's function table — the
// closure-as-distinct-frame surfacing is a downstream M11 extension
// (mirrors the byte_array / felt252_dict pattern: the trace today
// contains the surrounding driver function frames; the synthetic
// closure frame is the next step).  What this fixture does pin
// strictly: the function table, the call sequence (`main` →
// `no_capture` → `with_capture`), and the per-driver
// `call_exit.return_value` decoded as a typed `ValueRecord::Int`.
// The `bias` capture surfaces as a let-binding step variable in
// `with_capture` so the recorder also pins the captured value.
//
// Computed values:
//   * `no_capture()` → `(|x| x + 32)(10) = 42`.
//   * `with_capture()` → `bias = 32; (|x| x + bias)(10) = 42`.
//   * `main` returns `42 + 42 = 84`.

fn no_capture() -> felt252 {
    let c = |x: felt252| x + 32;
    c(10)
}

fn with_capture() -> felt252 {
    let bias: felt252 = 32;
    let c = |x: felt252| x + bias;
    c(10)
}

fn main() -> felt252 {
    let a: felt252 = no_capture();
    let b: felt252 = with_capture();
    a + b
}
