fn array_total() -> felt252 {
    let mut arr: Array<felt252> = ArrayTrait::new();
    arr.append(1);
    arr.append(2);
    arr.append(3);
    arr.append(4);
    let len: felt252 = arr.len().into();
    len
}

fn pair_sum() -> felt252 {
    let pair: (felt252, felt252) = (10, 20);
    let (x, y) = pair;
    x + y
}

fn compute() -> (felt252, felt252, felt252) {
    let arr_total: felt252 = array_total();
    let pair_total: felt252 = pair_sum();
    let final_sum: felt252 = arr_total + pair_total;
    (arr_total, pair_total, final_sum)
}

fn main() -> (felt252, felt252, felt252) {
    compute()
}
