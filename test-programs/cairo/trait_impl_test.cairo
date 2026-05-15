// M10 round 3: trait + impl pin.
//
// Cairo traits monomorphise into one Sierra function per impl method.
// This fixture declares a `Greeter` trait with a single `greet` method,
// two impls (`HelloImpl`, `HiImpl`) for distinct unit struct receivers,
// and a driver that calls each via direct impl-path syntax
// (`HelloImpl::greet(...)` / `HiImpl::greet(...)`).  The recorder must
// surface each impl method as its own entry in the function table with
// the module-qualified name (`HelloImpl::greet` / `HiImpl::greet`)
// rather than collapsing them under a shared bare `greet` key.
// `#[inline(never)]` keeps the Sierra optimiser from collapsing the
// impls into the driver.

#[derive(Drop, Copy)]
struct Hello {}

#[derive(Drop, Copy)]
struct Hi {}

trait Greeter<T> {
    fn greet(self: T) -> u32;
}

impl HelloImpl of Greeter<Hello> {
    #[inline(never)]
    fn greet(self: Hello) -> u32 {
        7
    }
}

impl HiImpl of Greeter<Hi> {
    #[inline(never)]
    fn greet(self: Hi) -> u32 {
        11
    }
}

#[inline(never)]
fn drive() -> u32 {
    let h: Hello = Hello {};
    let i: Hi = Hi {};
    let a: u32 = HelloImpl::greet(h);
    let b: u32 = HiImpl::greet(i);
    a + b
}

fn main() -> felt252 {
    let r: u32 = drive();
    r.into()
}
