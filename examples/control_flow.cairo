// Exercises branches, a `while` loop, and `match` so the recorded
// trace contains non-linear control flow worth stepping through in
// the GUI.
fn classify(raw: felt252) -> felt252 {
    if raw == 0 {
        return 0;
    }
    if raw == 1 {
        return 10;
    }
    20
}

fn loop_sum(n: u32) -> felt252 {
    let mut total: felt252 = 0;
    let mut i: u32 = 0;
    while i < n {
        total = total + 1;
        i = i + 1;
    };
    total
}

fn match_pick(tag: felt252) -> felt252 {
    match tag {
        0 => 100,
        _ => 200,
    }
}

fn compute() -> (felt252, felt252, felt252, felt252, felt252) {
    let raw: felt252 = 2;
    let sign: felt252 = classify(raw);
    let loop_total: felt252 = loop_sum(3);
    let picked: felt252 = match_pick(0);
    let combined: felt252 = sign + loop_total + picked;
    (raw, sign, loop_total, picked, combined)
}

fn main() -> (felt252, felt252, felt252, felt252, felt252) {
    compute()
}
