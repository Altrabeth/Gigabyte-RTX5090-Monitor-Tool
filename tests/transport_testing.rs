//! Proves the `testing` module and `Retrying` work from outside the crate, as
//! the probe gate and the upload tests will use them. Nothing here opens a
//! device: `LinuxI2c` is deliberately absent.

use aorus_lcd::bus::transport::testing::{i2c_error, Mock};
use aorus_lcd::bus::transport::{Retrying, Transport};
use aorus_lcd::panel::RecordingSleep;
use aorus_lcd::Error;

#[test]
fn a_clone_sees_what_the_code_under_test_did() {
    let mock = Mock::new();
    mock.script_read([0xAB; 8]);
    let mut transport: Box<dyn Transport> = Box::new(mock.clone());
    let mut buf = [0u8; 8];

    transport.write(&[0xEB, 0x03]).unwrap();
    transport.read(&mut buf).unwrap();

    assert_eq!(mock.writes(), [[0xEB, 0x03]]);
    assert_eq!(buf, [0xAB; 8]);
}

#[test]
fn a_short_read_is_an_error_not_a_partial_buffer() {
    let mock = Mock::new();
    mock.script_read([0xAB; 3]);
    let mut transport = mock.clone();
    let mut buf = [0u8; 8];

    let result = transport.read(&mut buf);

    assert!(matches!(
        result,
        Err(Error::ShortRead {
            expected: 8,
            actual: 3
        })
    ));
}

#[test]
fn a_write_can_fail_part_way_through_a_sequence() {
    let mock = Mock::new();
    mock.fail_write(1, i2c_error());
    let mut transport = mock.clone();

    transport.write(&[1]).unwrap();
    assert!(transport.write(&[2]).is_err());
    transport.write(&[3]).unwrap();

    assert_eq!(mock.writes(), [[1], [3]]);
    assert_eq!(mock.write_attempts(), 3);
}

#[test]
fn retrying_wraps_a_boxed_transport() {
    let mock = Mock::new();
    mock.fail_write(0, i2c_error());
    let boxed: Box<dyn Transport> = Box::new(mock.clone());
    let mut transport = Retrying::with_sleep(boxed, RecordingSleep::default());

    transport.write(&[0xE7]).unwrap();

    assert_eq!(mock.writes(), [[0xE7]]);
}
