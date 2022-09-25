#[allow(deprecated, invalid_value)]
fn main() {
    eprintln!("expecting a panic...");
    std::panic::catch_unwind(|| unsafe {
        // We want the super strict checks, so this should panic.
        let _bad: u8 = std::mem::uninitialized();
    })
    .unwrap_err();
    eprintln!("... looking good!");
}
