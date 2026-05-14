fn compute() -> felt252 {
    let opt_some: Option<felt252> = Option::Some(7);
    let opt_none: Option<felt252> = Option::None;
    let res_ok: Result<felt252, felt252> = Result::Ok(11);
    let res_err: Result<felt252, felt252> = Result::Err(5);
    let total: felt252 = 42;
    let _keep = (opt_some, opt_none, res_ok, res_err);
    total
}

fn main() -> felt252 {
    compute()
}
