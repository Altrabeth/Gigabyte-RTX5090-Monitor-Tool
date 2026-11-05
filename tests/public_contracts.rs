//! Proves the shared contracts are usable from outside the crate, the way
//! later integration tests and test doubles will use them.

use std::time::Duration;

use aorus_lcd::bus::transport::Transport;
use aorus_lcd::panel::{RecordingSleep, Sleep};
use aorus_lcd::protocol::SensorSample;
use aorus_lcd::Result;

/// Records writes, and serves one fixed byte to every read.
#[derive(Default)]
struct Loopback {
    writes: Vec<Vec<u8>>,
}

impl Transport for Loopback {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        self.writes.push(frame.to_vec());
        Ok(())
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        buf.fill(0xAB);
        Ok(())
    }
}

#[test]
fn transport_works_through_a_trait_object() {
    let mut transport: Box<dyn Transport> = Box::new(Loopback::default());
    let mut buf = [0u8; 8];

    transport.write(&[1, 2, 3]).unwrap();
    transport.read(&mut buf).unwrap();

    assert_eq!(buf, [0xAB; 8]);
}

#[test]
fn boxed_transport_satisfies_a_generic_bound() {
    fn send(mut transport: impl Transport) -> Result<()> {
        transport.write(&[0xEB])
    }

    let boxed: Box<dyn Transport> = Box::new(Loopback::default());
    send(boxed).unwrap();
}

#[test]
fn recording_sleep_keeps_delays_in_order() {
    let mut sleep = RecordingSleep::default();

    sleep.sleep(Duration::from_millis(500));
    sleep.sleep(Duration::from_secs(1));

    assert_eq!(
        sleep.delays,
        [Duration::from_millis(500), Duration::from_secs(1)]
    );
}

#[test]
fn sensor_sample_defaults_to_all_zero() {
    let sample = SensorSample::default();

    assert_eq!(sample.gpu_temp_c, 0);
    assert_eq!(sample.fps, 0);
}
