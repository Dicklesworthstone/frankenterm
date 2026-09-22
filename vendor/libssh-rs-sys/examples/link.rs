use libssh_rs_sys::*;

fn main() -> Result<(), String> {
    let session = unsafe { ssh_new() };
    assert!(!session.is_null(), "failed to allocate session");
    Ok(())
}
