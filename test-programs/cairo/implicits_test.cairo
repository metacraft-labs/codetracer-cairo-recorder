// M10 round 5: implicit-arg passing pin (gas / Pedersen / range
// check / segment arena).
//
// Cairo functions that exercise corelib primitives implicitly take
// extra "implicit" arguments that the Sierra runner threads through
// the call ABI: the `Pedersen` builtin pointer for `pedersen()`,
// the `RangeCheck` builtin for u32 / u128 arithmetic, the
// `GasBuiltin` for `withdraw_gas()` calls, and the
// `SegmentArenaBuiltin` for `Felt252Dict` allocations.  These args
// don't appear in the user-visible function signature — they're
// inserted by the Sierra ABI and propagated transparently through
// every callee.
//
// This fixture exercises a function that touches several implicit
// classes at once:
//   * `pedersen(a, b)` — implicits(Pedersen)
//   * `u32` arithmetic (`* 2`) — implicits(RangeCheck)
//   * `Felt252Dict` allocate / insert — implicits(SegmentArena, GasBuiltin)
//
// Strict pin (current recorder behaviour): the recorder does NOT
// surface implicit args separately on the call_entry — they are
// inlined into the Sierra call ABI and the source-level recorder
// doesn't reconstruct the implicit-vs-explicit distinction.  What
// the trace today contains is the user-visible function frames
// (`main`, `compute`) and the explicit `a` / `b` args derived from
// the source's let-binding shape.  Surfacing each implicit arg as
// its own `ValueRecord` (e.g. a synthetic `_gas` / `_range_check`
// step variable) is a downstream M11 extension; this fixture pins
// the strict shape recorded today so a future implicit-aware
// extension lands as a *new* test variable rather than silently
// overwriting the current contract.

use core::pedersen::pedersen;

fn compute(a: felt252, b: felt252) -> felt252 {
    let h: felt252 = pedersen(a, b);
    let mut d: Felt252Dict<u32> = Default::default();
    let widened: u32 = 7_u32 * 2_u32;
    d.insert('seed', widened);
    let _v: u32 = d.get('seed');
    h
}

fn main() -> felt252 {
    compute(1, 2)
}
