// M10 round 2: numeric-width pin.
//
// Each bounded-integer type (u8/u16/u32/u64/u128 + i8/i16/i32/i64/i128 +
// u256) must surface in the trace with the *declared* width carried in
// the per-variable `type_name`.  Pre-fix every integer let-binding
// shared the felt252 type id; post-fix `parse_typed_int_bindings`
// recovers the declared width from `let <name>: <T> = <lit>;` lines and
// emits `ValueRecord::Int { type_id }` against a per-width type id.
//
// `u256` is a special case in Cairo (`struct u256 { low: u128, high:
// u128 }`); the recorder surfaces it as a dedicated
// `ValueRecord::Struct` with the two u128 halves.

fn use_widths() -> felt252 {
    let a8: u8 = 254;
    let a16: u16 = 65534;
    let a32: u32 = 4000000000;
    let a64: u64 = 9000000000000000000;
    let a128: u128 = 1234567890123456789;
    let s8: i8 = -127;
    let s16: i16 = -32767;
    let s32: i32 = -2000000000;
    let s64: i64 = -9000000000000000000;
    let s128: i128 = -123456789012345;
    let u_big: u256 = 0;
    // Force every binding into a felt252 sum so the optimiser doesn't
    // fold them out (felt252 is the only type the Cairo VM lets us
    // return without an extra trait dance for these tests).
    let total: felt252 = a8.into() + a16.into() + a32.into() + a64.into()
        + a128.into() + s8.into() + s16.into() + s32.into() + s64.into() + s128.into();
    let _keep_u256 = u_big.low + u_big.high;
    total
}

fn main() -> felt252 {
    use_widths()
}
