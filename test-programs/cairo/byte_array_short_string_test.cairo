// M10 round 4: ByteArray + felt252 short-string pin.
//
// `ByteArray` is Cairo's arbitrary-length text type — internally a
// struct of `data: Array<bytes31>` plus a tail byte buffer.  Short
// strings (`'STX_OK'`, `'alice'`, ...) are felt252 literals whose
// big-endian byte representation is the printable ASCII payload — so
// every short-string fits in 31 bytes and surfaces as a scalar
// felt252 / Int in the trace.
//
// Strict pin (current recorder behaviour): the `tag` short-string
// binding surfaces as a `ValueRecord::Int` whose `i` field decodes to
// the canonical big-endian felt encoding of `'STX_OK'`
// (= 0x53_54_58_5F_4F_4B = 91621724999499).  The `_greeting` ByteArray
// binding does not yet surface as a typed Struct — the recorder's
// current source-level heuristics handle scalar / Sequence / Tuple /
// Struct-literal / Variant shapes only — but the call/return frame for
// `make_greeting()` is captured so consumers can still dispatch on
// the function name.  ByteArray Struct decoding is a downstream M11
// extension; this fixture pins the strict shape recorded today so a
// future ByteArray decoder lands as a *new* test variable rather than
// silently overwriting the current contract.

fn make_greeting() -> ByteArray {
    "Hello, world!"
}

fn make_tag() -> felt252 {
    'STX_OK'
}

fn main() -> felt252 {
    let _greeting: ByteArray = make_greeting();
    let tag: felt252 = make_tag();
    tag
}
