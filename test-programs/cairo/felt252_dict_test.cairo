// M10 round 4: Felt252Dict pin.
//
// `Felt252Dict<T>` is Cairo's mutable felt252-keyed map.  At runtime
// the dict is a sparse vector indexed by the (low 251 bits of the)
// key felt; at recording time each `.insert(k, v)` and `.get(k)`
// surfaces as a dispatched method call into the corelib's
// `Felt252DictTrait` impl.  The trailing `.squash()` collapses the
// in-memory dict into a `SquashedFelt252Dict` whose entries can be
// iterated for the final state snapshot.
//
// Strict pin (current recorder behaviour): the driver function
// `use_dict()` surfaces in the trace's function table alongside the
// `main` wrapper.  The corelib dispatch into `Felt252DictTrait::insert`
// / `Felt252DictTrait::get` is *inlined* by the Sierra optimiser at
// the call sites — so the function table contains only the
// driver-side frames (main + use_dict), but the resulting `v` value
// (=100, the felt the driver inserted under the 'alice' key) flows
// out through `use_dict()`'s u32 return and surfaces on the
// `call_exit.return_value` of `use_dict` as a typed `ValueRecord::Int`.
// The recorder does not yet emit a synthetic SquashedFelt252Dict step
// variable — that's a downstream M11 extension.  This fixture pins
// the strict shape recorded today; richer dict-state surfacing lands
// as a *new* test variable rather than silently overwriting the
// current contract.

fn use_dict() -> u32 {
    let mut d: Felt252Dict<u32> = Default::default();
    d.insert('alice', 100);
    d.insert('bob', 200);
    let v: u32 = d.get('alice');
    v
}

fn main() -> felt252 {
    let r: u32 = use_dict();
    r.into()
}
