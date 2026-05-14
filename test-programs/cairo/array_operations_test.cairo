// M10 round 2: array operations pin.
//
// Pre-fix the array compound-binding heuristic only recognised the
// `let mut a: Array<...> = ArrayTrait::new();` + repeated
// `<name>.append(<lit>);` shape — the more idiomatic
// `let mut a = array![1, 2, 3, 4];` macro form was dropped.  Post-fix
// `parse_compound_bindings` also recognises `array![...]` literal-only
// initialisers, plus the two most common consumers:
//
//   * `<name>.pop_front()` — removes and returns the head element.
//     The recorder re-emits the array's contents after the mutation
//     so the trace surfaces the post-pop state.
//
//   * `*<name>.at(<idx>)` — read-only element-at-index.  No
//     re-emission needed; the underlying compound binding is left
//     untouched.

fn use_array() -> u32 {
    let mut a = array![1_u32, 2_u32, 3_u32, 4_u32];
    let _popped = a.pop_front();
    let head: u32 = *a.at(0);
    head
}

fn main() -> u32 {
    use_array()
}
