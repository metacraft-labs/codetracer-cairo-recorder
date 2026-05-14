// M10 round 2: destructuring let-binding pin.
//
// `let (x, y) = pair;` should surface as three step variables:
//   * `pair` — the source tuple binding (Tuple [Int 10, Int 20]).
//   * `x` — the first destructured component (Int 10).
//   * `y` — the second destructured component (Int 20).
//
// Pre-fix only the source `pair` binding surfaced; the destructured names
// were dropped because `parse_let_binding_names` skipped `let (...)`
// patterns and `parse_compound_bindings` did the same.

fn use_pair() -> felt252 {
    let pair: (felt252, felt252) = (10, 20);
    let (x, y) = pair;
    let total: felt252 = x + y;
    total
}

fn main() -> felt252 {
    use_pair()
}
