/// This tests doctests.
/// ```rust
/// assert!(true);
/// ```
/// 
/// In particular those that use external dependencies.
/// ```rust
/// use byteorder::{BigEndian, ByteOrder};
/// <BigEndian as ByteOrder>::read_u64(&[1, 2, 3, 4, 5, 6, 7, 8]);
/// ```
#[allow(unused)]
pub fn test() {}

#[test]
fn extern_dep() {
    use byteorder::{BigEndian, ByteOrder};
    let _ = <BigEndian as ByteOrder>::read_u64(&[1, 2, 3, 4, 5, 6, 7, 8]);
}
