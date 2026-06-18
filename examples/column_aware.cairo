// Column-aware navigation fixture (FU-Column-Aware-Nav-Cairo).
//
// `main` packs three `let` declarations onto a single source line so
// each statement starts at a distinct column. Under column-aware
// navigation the recorder surfaces a step for each statement with
// strictly distinct column values; without column awareness all three
// collapse onto the same `(line, column=1)` pair and only the first
// surfaces as a step.
//
// Record this example with `ct record examples/column_aware.cairo`
// and step-over inside the CodeTracer GUI: the cursor advances column
// by column on line 14 before moving on to line 15.
fn main() -> felt252 {
    let a: felt252 = 1; let b: felt252 = 2; let c: felt252 = 3;
    a + b + c
}
