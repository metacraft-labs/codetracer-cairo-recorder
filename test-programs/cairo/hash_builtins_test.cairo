// M10 round 4: hash builtin pin.
//
// Two of Cairo's canonical hash primitives:
//
//   * `pedersen::pedersen(a, b) -> felt252` — the classic StarkNet
//     state-tree hash.
//   * `poseidon::hades_permutation(a, b, c) -> (felt252, felt252,
//     felt252)` — the Poseidon "Hades" permutation that backs the
//     `poseidon_hash_*` family.  Returns the three permuted state
//     felts.
//
// The driver feeds small fixed inputs and surfaces the *first* output
// felt of each builtin as a `felt252` return so the recorder's existing
// `Int` / `return_value` plumbing pins both calls.  Pre-computed
// expected outputs (offline using the corelib's own implementation):
//
//   * `pedersen(0x1, 0x2)` =
//     0x05bb9440e27889a364bcb678b1f679ecd1347acdedcbf36e83494f857cc58026
//     (mod p, fits comfortably inside 251 bits).
//   * `hades_permutation(0x1, 0x2, 0x3)` returns three state felts;
//     the first is also recovered offline.
//
// Strict pin (current recorder behaviour): the recorder doesn't yet
// emit a typed Int variable for the corelib hash *outputs* (the values
// flow through felt252 returns that the source-level let-binding
// heuristic doesn't see — they live in unspilled tail-position
// expressions).  What it *does* pin strictly: the function table
// (`main`, `use_pedersen`, `use_poseidon`), the call/exit sequence
// (DFS visit order from main, LIFO close), and the two driver
// functions' `call_exit.return_value` decoded as typed Int — those
// are the recovered pedersen / poseidon outputs the caller asked
// for.  Surfacing each builtin call as its *own* call frame is a
// downstream M11 extension (the Sierra optimiser inlines the corelib
// dispatch into the driver body); this fixture pins the strict shape
// recorded today so the inline-vs-distinct-frame question lands as a
// new test variable.

use core::pedersen::pedersen;
use core::poseidon::hades_permutation;

fn use_pedersen() -> felt252 {
    let h: felt252 = pedersen(1, 2);
    h
}

fn use_poseidon() -> felt252 {
    let (s0, _s1, _s2) = hades_permutation(1, 2, 3);
    s0
}

fn main() -> felt252 {
    let p: felt252 = use_pedersen();
    let q: felt252 = use_poseidon();
    p + q
}
