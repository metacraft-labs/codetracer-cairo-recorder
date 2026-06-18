// Column-aware navigation fixture (FU-Column-Aware-Nav-Cairo).
//
// `main` packs three `let` declarations onto a single source line so
// each statement starts at a distinct column.  Under column-aware
// navigation the recorder must surface a step for each statement with
// strictly distinct column values; without column awareness all three
// collapse onto the same `(line, column=1)` pair and only the first
// surfaces as a step.
//
// See the JS reference fixture at
// `codetracer-js-recorder/tests/integration/column-aware.test.ts`
// and the EVM fixture at
// `codetracer-evm-recorder/test-programs/column_aware/ColumnAware.sol`.
fn main() -> felt252 {
    let a: felt252 = 1; let b: felt252 = 2; let c: felt252 = 3;
    a + b + c
}
