#![no_main]
use libfuzzer_sys::fuzz_target;

mod error {
    include!("../../src/error.rs");
}

include!("../../src/cmd.rs");

fuzz_target!(|data: &[u8]| {
    let _ = Command::new(data.to_vec());
});
