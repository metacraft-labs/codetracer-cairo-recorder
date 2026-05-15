// M10 round 3: generic-function pin.
//
// `min<T, +PartialOrd<T>, +Copy<T>, +Drop<T>>(a: T, b: T) -> T` is a
// generic function that monomorphises into one Sierra function per
// instantiation.  This fixture exercises two instantiations
// (`min<u32>` via `pick_u32` and `min<u64>` via `pick_u64`) so the
// function table surfaces two distinguishable Sierra-name entries
// for what is the same Cairo source function.  felt252 has no
// `PartialOrd` impl in the corelib so we use bounded widths instead.

fn min<T, +PartialOrd<T>, +Copy<T>, +Drop<T>>(a: T, b: T) -> T {
    if a < b {
        a
    } else {
        b
    }
}

fn pick_u32() -> u32 {
    let lo: u32 = 5;
    let hi: u32 = 7;
    let r: u32 = min::<u32>(lo, hi);
    r
}

fn pick_u64() -> u64 {
    let lo: u64 = 11;
    let hi: u64 = 13;
    let r: u64 = min::<u64>(lo, hi);
    r
}

fn main() -> felt252 {
    let f: u32 = pick_u32();
    let g: u64 = pick_u64();
    let total: felt252 = f.into() + g.into();
    total
}
