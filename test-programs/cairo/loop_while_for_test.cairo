fn loop_three() -> felt252 {
    let mut acc: felt252 = 0;
    let mut i: u32 = 0;
    while i < 3 {
        acc = acc + 1;
        i = i + 1;
    };
    acc
}

fn loop_double() -> felt252 {
    let mut acc: felt252 = 0;
    let mut i: u32 = 0;
    while i < 2 {
        acc = acc + 10;
        i = i + 1;
    };
    acc
}

fn compute() -> felt252 {
    let a: felt252 = loop_three();
    let b: felt252 = loop_double();
    let total: felt252 = a + b;
    total
}

fn main() -> felt252 {
    compute()
}
