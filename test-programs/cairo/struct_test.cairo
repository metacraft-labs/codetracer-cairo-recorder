#[derive(Drop, Copy)]
struct Point {
    x: felt252,
    y: felt252,
}

fn compute() -> felt252 {
    let origin: Point = Point { x: 3, y: 4 };
    let shift: Point = Point { x: 10, y: 20 };
    let total: felt252 = origin.x + origin.y + shift.x + shift.y;
    total
}

fn main() -> felt252 {
    compute()
}
