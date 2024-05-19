#![allow(deprecated, invalid_value, unexpected_cfgs)]

use std::ffi::CString;
use std::mem;

// We cannot test `uninitialized` checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn uninit_bool_array() {
    let _ = unsafe { mem::uninitialized::<[bool; 1]>() };
}

// We cannot test `uninitialized` checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn uninit_u8() {
    // We want the super strict checks, so this should panic.
    let _ = unsafe { std::mem::uninitialized::<u8>() };
}

// We cannot test `read` alignment checks since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn read_unaligned() {
    let arr = [1u8, 2, 3, 4];
    for n in [0, 1] {
        let val = unsafe { arr.as_ptr().add(n).cast::<u16>().read() };
        println!("The value is {val}!");
    }
}

#[test]
#[should_panic]
fn c_str() {
    let _ = unsafe { CString::from_vec_unchecked(vec![0]) };
}

// We cannot test get_unchecked bounds checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn get_unchecked() {
    let _ = unsafe { [0].get_unchecked(1) };
}

#[test]
fn cfg_flag() {
    assert!(cfg!(careful));
}
