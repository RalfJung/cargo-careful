#![allow(deprecated, invalid_value)]

use std::mem;

#[test]
#[should_panic]
fn uninit_bool_array() {
    unsafe { mem::uninitialized::<[bool; 1]>() };
}

// We cannot test get_unchecked bounds checking since that will abort, not unwind.
