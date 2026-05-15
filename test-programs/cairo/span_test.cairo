// M10 round 3: Span<T> pin.
//
// `Span<T>` is the immutable slice view over an `Array<T>`.  The driver
// builds `array![10, 20, 30]`, takes a `.span()` view, and passes it
// into `sum_span(items: Span<u32>) -> u32` which sums the three
// elements.
//
// Strict pin (current recorder behaviour): the `xs` source binding
// surfaces as a `ValueRecord::Sequence { is_slice: false, ... }`
// (owned Array literal) and the `view` binding surfaces as a
// `ValueRecord::Sequence { is_slice: true, ... }` carrying the same
// elements — the recorder recognises `<name>.span()` as the
// owned→view conversion and re-emits the contents with the slice
// discriminator flipped on.
//
// Note: the recorder does NOT yet propagate the Span carrier into the
// callee's `items` parameter binding (no Reference value at the
// callee's entry step).  That FFI gap stays for round 4 — the pin
// here is intentionally on the source-side `view` binding only.

#[inline(never)]
fn sum_span(items: Span<u32>) -> u32 {
    let mut total: u32 = 0;
    let mut i: u32 = 0;
    while i < 3 {
        total = total + *items.at(i);
        i = i + 1;
    };
    total
}

fn main() -> felt252 {
    let xs = array![10_u32, 20_u32, 30_u32];
    let view = xs.span();
    let r: u32 = sum_span(view);
    r.into()
}
