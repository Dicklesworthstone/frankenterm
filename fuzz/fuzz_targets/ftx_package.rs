#![no_main]

use std::path::PathBuf;

use frankenterm_scripting::FtxPackage;
use libfuzzer_sys::fuzz_target;

const MAX_FTX_BYTES: usize = 512 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_FTX_BYTES {
        return;
    }

    let _ = FtxPackage::from_bytes(data, PathBuf::from("fuzz.ftx"));
});
