use std::env;
use apk_proofs::test_helpers;

fn main() {
    let args: Vec<String> = env::args().collect();
    println!("{:?}", args);
    let log_n = args[1].parse().unwrap();
    test_helpers::test_packed_scheme(log_n);
}