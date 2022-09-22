#[allow(deprecated, invalid_value)]
fn main() {
    unsafe {
        // We want the super strict checks.
        let _bad: u8 = std::mem::uninitialized();
    }
}
