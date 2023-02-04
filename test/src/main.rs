#[allow(deprecated, invalid_value)]
fn main() {
    eprintln!("expecting a panic...");
    std::panic::catch_unwind(|| unsafe {
        let _ = std::ffi::CString::from_vec_unchecked(vec![0]);
    })
    .unwrap_err();
    eprintln!("... looking good!");
}
