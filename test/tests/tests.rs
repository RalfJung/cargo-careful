#![allow(deprecated, invalid_value)]

use std::ffi::CString;
use std::mem;

// We cannot test `uninitialized` checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn uninit_bool_array() {
    unsafe { mem::uninitialized::<[bool; 1]>() };
}

// We cannot test `uninitialized` checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn uninit_u8() {
    // We want the super strict checks, so this should panic.
    unsafe { std::mem::uninitialized::<u8>() };
}

#[test]
#[should_panic]
fn c_str() {
    unsafe { CString::from_vec_unchecked(vec![0]) };
}

// We cannot test get_unchecked bounds checking since that will abort, not unwind.
#[test]
#[ignore]
#[should_panic]
fn get_unchecked() {
    unsafe { [0].get_unchecked(1) };
}

#[test]
fn cfg_flag() {
    assert!(cfg!(careful));
}
