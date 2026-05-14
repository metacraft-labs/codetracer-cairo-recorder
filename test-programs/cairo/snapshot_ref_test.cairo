// M10 round 2: snapshot (`@T`) and mutable-reference (`ref T`)
// parameter pin.
//
// Cairo passes structs by value by default; `@Point` is the snapshot
// (read-only borrow) form and `ref Point` is the mutable-reference
// form.  This fixture exercises both shapes so the recorder can pin a
// `ValueRecord::Reference { mutable: false, ... }` for the snapshot
// parameter and a `ValueRecord::Reference { mutable: true, ... }` for
// the mutable-reference parameter, with the dereferenced `Point`
// struct walked under each.

#[derive(Drop, Copy)]
struct Point {
    x: felt252,
    y: felt252,
}

fn read_only(p: @Point) -> felt252 {
    *p.x + *p.y
}

fn scale(ref p: Point, k: felt252) {
    p.x = p.x * k;
    p.y = p.y * k;
}

fn compute() -> felt252 {
    let origin: Point = Point { x: 3, y: 4 };
    let snap_total: felt252 = read_only(@origin);
    let mut shift: Point = Point { x: 10, y: 20 };
    scale(ref shift, 2);
    let scaled_total: felt252 = shift.x + shift.y;
    let total: felt252 = snap_total + scaled_total;
    total
}

fn main() -> felt252 {
    compute()
}
