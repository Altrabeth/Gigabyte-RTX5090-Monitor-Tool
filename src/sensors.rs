//! Polls GPU sensors into [`SensorSample`]s.
//!
//! NVML is reached through `nvml-wrapper`, which `dlopen`s the library at
//! runtime: the binary has no link-time dependency on the NVIDIA driver, and
//! only [`NvmlSensors::new`] fails where it is missing.
//!
//! Everything that decides a value is a pure function, so it is tested without
//! a GPU. The only test here that reaches NVML is the `#[ignore]`d live sample.

use std::ffi::OsStr;
use std::fmt::Debug;

use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::enums::device::SampleValue;
use nvml_wrapper::error::NvmlError;
use nvml_wrapper::struct_wrappers::device::{FieldValueSample, PciInfo};
use nvml_wrapper::structs::device::FieldId;
use nvml_wrapper::sys_exports::field_id::NVML_FI_DEV_POWER_INSTANT;
use nvml_wrapper::Nvml;

use crate::error::{Error, Result};
use crate::protocol::SensorSample;

/// Library names to try. The versioned name comes first because distributions
/// that split out `-dev` packages ship the bare `.so` only there. Same order as
/// C# `Nvml.Candidates`.
const LIBRARY_NAMES: [&str; 2] = ["libnvidia-ml.so.1", "libnvidia-ml.so"];

/// The panel shows a single fan number. The first fan stands for all of them,
/// as in C# `NvmlSensorSource`.
const FAN_INDEX: u32 = 0;

/// A source of [`SensorSample`]s.
pub trait SensorSource {
    /// Reads every sensor once.
    ///
    /// A single failed sensor zeroes its field and is not an error. An error
    /// means no sensor could be reached at all, see [`Error::Sensor`].
    fn read(&mut self) -> Result<SensorSample>;
}

/// A [`SensorSource`] for tests, which serves a fixed sample.
#[derive(Debug, Clone, Default)]
pub struct FakeSensors {
    /// What every read returns.
    pub sample: SensorSample,
    /// Makes every read fail, as if the sensor library had gone away.
    pub fail: bool,
    /// How many reads were made, including failed ones.
    pub reads: usize,
}

impl SensorSource for FakeSensors {
    fn read(&mut self) -> Result<SensorSample> {
        self.reads += 1;
        if self.fail {
            return Err(sensor_error("fake sensor failure", NvmlError::Unknown));
        }
        Ok(self.sample)
    }
}

/// Reads the sensors of one GPU through NVML.
#[derive(Debug)]
pub struct NvmlSensors {
    nvml: Nvml,
    /// A `Device` borrows the `Nvml` it came from, so it cannot live next to
    /// it. The PCI address is kept instead and resolved on every read, which
    /// also means a read can never land on another GPU if indices shift.
    pci_bus_id: String,
    fan_path: Option<FanPath>,
    power_path: Option<PowerPath>,
}

impl NvmlSensors {
    /// Loads NVML and selects the GPU on PCI bus `pci_bus`, which is the bus of
    /// the discovered I2C adapter (the `1` in `at 1:00.0`).
    ///
    /// Fails if NVML is unavailable, and refuses to guess unless exactly one
    /// GPU sits on that bus.
    pub fn new(pci_bus: u32) -> Result<Self> {
        let nvml = load_nvml(&LIBRARY_NAMES)?;
        let pci_bus_id = find_pci_bus_id(&nvml, pci_bus)?;
        Ok(Self {
            nvml,
            pci_bus_id,
            fan_path: None,
            power_path: None,
        })
    }
}

impl SensorSource for NvmlSensors {
    fn read(&mut self) -> Result<SensorSample> {
        let device = self
            .nvml
            .device_by_pci_bus_id(self.pci_bus_id.as_str())
            .map_err(|source| {
                sensor_error(format!("cannot reach GPU {}", self.pci_bus_id), source)
            })?;

        let (fan_path, fan_speed) = pick_fan(device.fan_speed_rpm(FAN_INDEX).ok(), || {
            device.fan_speed(FAN_INDEX).ok()
        });
        let instant = device.field_values_for(&[FieldId(NVML_FI_DEV_POWER_INSTANT)]);
        let (power_path, power_mw) =
            pick_power(instant_power_mw(instant), || device.power_usage().ok());
        note_path_change(&mut self.fan_path, fan_path, "fan");
        note_path_change(&mut self.power_path, power_path, "power");

        Ok(to_sample(&Readings {
            temperature_c: device.temperature(TemperatureSensor::Gpu).ok(),
            graphics_clock_mhz: device.clock_info(Clock::Graphics).ok(),
            memory_clock_mhz: device.clock_info(Clock::Memory).ok(),
            gpu_utilization_percent: device.utilization_rates().ok().map(|rates| rates.gpu),
            vram_used_and_total: device
                .memory_info()
                .ok()
                .map(|memory| (memory.used, memory.total)),
            fan_speed,
            power_mw,
        }))
    }
}

/// Where the fan number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FanPath {
    /// `nvmlDeviceGetFanSpeedRPM`: real RPM.
    Rpm,
    /// `nvmlDeviceGetFanSpeed_v2`: percent of the maximum.
    Percent,
    Unavailable,
}

/// Where the power number came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PowerPath {
    /// The `NVML_FI_DEV_POWER_INSTANT` field.
    Instant,
    /// `nvmlDeviceGetPowerUsage`.
    PowerUsage,
    Unavailable,
}

/// One round of raw NVML results. `None` is a sensor that failed.
#[derive(Debug, Default)]
struct Readings {
    temperature_c: Option<u32>,
    graphics_clock_mhz: Option<u32>,
    memory_clock_mhz: Option<u32>,
    gpu_utilization_percent: Option<u32>,
    vram_used_and_total: Option<(u64, u64)>,
    fan_speed: u32,
    power_mw: u32,
}

fn to_sample(readings: &Readings) -> SensorSample {
    SensorSample {
        gpu_temp_c: readings.temperature_c.unwrap_or(0),
        gpu_clock_mhz: readings.graphics_clock_mhz.unwrap_or(0),
        gpu_usage_percent: readings.gpu_utilization_percent.unwrap_or(0),
        fan_speed: readings.fan_speed,
        ram_clock_mhz: readings.memory_clock_mhz.unwrap_or(0),
        ram_usage_percent: readings
            .vram_used_and_total
            .map_or(0, |(used, total)| vram_percent(used, total)),
        fps: 0, // NVML has no FPS source.
        tgp_watts: milliwatts_to_watts(readings.power_mw),
    }
}

/// VRAM used as a percentage of the total, rounded down.
///
/// The C# code sends memory-controller utilization here. The README defines
/// this widget as VRAM usage, so it is computed from the frame buffer instead.
fn vram_percent(used: u64, total: u64) -> u32 {
    if total == 0 {
        return 0;
    }
    u32::try_from(used.saturating_mul(100) / total).unwrap_or(u32::MAX)
}

/// Rounded up, like C# `NvmlSensorSource`. `div_ceil` rather than adding 999:
/// it cannot overflow on a sentinel value.
fn milliwatts_to_watts(milliwatts: u32) -> u32 {
    milliwatts.div_ceil(1000)
}

/// Prefers real RPM. A successful 0 RPM is a real value, the fans stop at idle,
/// so only a failed call falls back to percent.
fn pick_fan(rpm: Option<u32>, percent: impl FnOnce() -> Option<u32>) -> (FanPath, u32) {
    if let Some(rpm) = rpm {
        return (FanPath::Rpm, rpm);
    }
    percent().map_or((FanPath::Unavailable, 0), |percent| {
        (FanPath::Percent, percent)
    })
}

/// Prefers the instant field: `nvmlDeviceGetPowerUsage` reports a stuck value
/// on RTX 50 cards (C# `NvmlSensorSource`, README). Both are in milliwatts.
fn pick_power(instant: Option<u32>, usage: impl FnOnce() -> Option<u32>) -> (PowerPath, u32) {
    if let Some(instant) = instant {
        return (PowerPath::Instant, instant);
    }
    usage().map_or((PowerPath::Unavailable, 0), |usage| {
        (PowerPath::PowerUsage, usage)
    })
}

/// Extracts the milliwatts from a `nvmlDeviceGetFieldValues` result for the
/// single instant-power field. The call can fail as a whole or per field.
fn instant_power_mw(
    fields: std::result::Result<Vec<std::result::Result<FieldValueSample, NvmlError>>, NvmlError>,
) -> Option<u32> {
    let sample = fields.ok()?.into_iter().next()?.ok()?;
    match sample.value.ok()? {
        SampleValue::U32(milliwatts) => Some(milliwatts),
        SampleValue::U64(milliwatts) => milliwatts.try_into().ok(),
        SampleValue::I64(milliwatts) => milliwatts.try_into().ok(),
        SampleValue::F64(_) => None,
    }
}

/// Logs a sensor path when it changes, rather than once per second.
fn note_path_change<T: Copy + PartialEq + Debug>(last: &mut Option<T>, now: T, sensor: &str) {
    if *last != Some(now) {
        log::debug!("{sensor} sensor path: {now:?}");
        *last = Some(now);
    }
}

fn load_nvml(library_names: &[&str]) -> Result<Nvml> {
    let mut failure = NvmlError::LibraryNotFound;
    for name in library_names {
        match Nvml::builder().lib_path(OsStr::new(name)).init() {
            Ok(nvml) => return Ok(nvml),
            Err(load_failure @ NvmlError::LibloadingError(_)) => failure = load_failure,
            // The library loaded, so another name would not change the outcome.
            Err(other) => {
                failure = other;
                break;
            }
        }
    }
    Err(sensor_error(
        "cannot initialize NVML; is the NVIDIA driver installed and loaded?",
        failure,
    ))
}

/// Finds the PCI address of the GPU on `pci_bus`. A device whose PCI info cannot
/// be read is skipped: it cannot be proven to be the wanted one.
fn find_pci_bus_id(nvml: &Nvml, pci_bus: u32) -> Result<String> {
    let count = nvml
        .device_count()
        .map_err(|source| sensor_error("cannot count NVML devices", source))?;
    let devices: Vec<PciInfo> = (0..count)
        .filter_map(|index| nvml.device_by_index(index).ok()?.pci_info().ok())
        .collect();
    pick_by_bus(pci_bus, &devices).map(|device| device.bus_id.clone())
}

/// Picks the one device on `pci_bus`. No match, or a match in two PCI domains,
/// is an error: a wrong pick would feed the panel another card's sensors.
fn pick_by_bus(pci_bus: u32, devices: &[PciInfo]) -> Result<&PciInfo> {
    let mut matching = devices.iter().filter(|device| device.bus == pci_bus);
    let problem = match (matching.next(), matching.next()) {
        (Some(device), None) => return Ok(device),
        (Some(_), Some(_)) => "several NVML devices match".to_string(),
        (None, _) => {
            let seen: Vec<_> = devices.iter().map(|device| &device.bus_id).collect();
            format!("no NVML device matches among {seen:?}")
        }
    };
    Err(sensor_error(
        format!("refusing to guess the GPU on PCI bus {pci_bus:#04x}"),
        problem,
    ))
}

fn sensor_error(
    context: impl Into<String>,
    source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> Error {
    Error::Sensor {
        context: context.into(),
        source: source.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    fn pci(bus: u32) -> PciInfo {
        PciInfo {
            bus,
            bus_id: format!("00000000:{bus:02x}:00.0"),
            device: 0,
            domain: 0,
            pci_device_id: 0,
            pci_sub_system_id: None,
        }
    }

    fn instant(value: std::result::Result<SampleValue, NvmlError>) -> Option<u32> {
        instant_power_mw(Ok(vec![Ok(FieldValueSample {
            field: FieldId(NVML_FI_DEV_POWER_INSTANT),
            timestamp: 0,
            latency: 0,
            value,
        })]))
    }

    #[test]
    fn maps_every_reading_to_its_field() {
        let sample = to_sample(&Readings {
            temperature_c: Some(61),
            graphics_clock_mhz: Some(2407),
            memory_clock_mhz: Some(14001),
            gpu_utilization_percent: Some(97),
            vram_used_and_total: Some((8_000, 32_000)),
            fan_speed: 1450,
            power_mw: 412_001,
        });

        assert_eq!(
            sample,
            SensorSample {
                gpu_temp_c: 61,
                gpu_clock_mhz: 2407,
                gpu_usage_percent: 97,
                fan_speed: 1450,
                ram_clock_mhz: 14001,
                ram_usage_percent: 25,
                fps: 0,
                tgp_watts: 413,
            }
        );
    }

    #[test]
    fn a_failed_sensor_zeroes_only_its_own_field() {
        let sample = to_sample(&Readings {
            temperature_c: None,
            graphics_clock_mhz: Some(2407),
            ..Readings::default()
        });

        assert_eq!(sample.gpu_temp_c, 0);
        assert_eq!(sample.gpu_clock_mhz, 2407);
    }

    #[test]
    fn all_sensors_failing_gives_an_all_zero_sample() {
        assert_eq!(to_sample(&Readings::default()), SensorSample::default());
    }

    #[test]
    fn vram_percent_is_used_over_total() {
        assert_eq!(vram_percent(0, 32_000), 0);
        assert_eq!(vram_percent(16_000, 32_000), 50);
        assert_eq!(vram_percent(32_000, 32_000), 100);
        assert_eq!(vram_percent(1, 3), 33);
    }

    #[test]
    fn vram_percent_survives_a_zero_total() {
        assert_eq!(vram_percent(5, 0), 0);
    }

    #[test]
    fn milliwatts_round_up_to_whole_watts() {
        assert_eq!(milliwatts_to_watts(0), 0);
        assert_eq!(milliwatts_to_watts(1), 1);
        assert_eq!(milliwatts_to_watts(1000), 1);
        assert_eq!(milliwatts_to_watts(1001), 2);
    }

    #[test]
    fn milliwatt_rounding_does_not_overflow() {
        assert_eq!(milliwatts_to_watts(u32::MAX), 4_294_968);
    }

    #[test]
    fn fan_prefers_rpm_and_does_not_query_percent() {
        let picked = pick_fan(Some(1450), || panic!("percent must not be read"));
        assert_eq!(picked, (FanPath::Rpm, 1450));
    }

    #[test]
    fn stopped_fans_at_zero_rpm_do_not_fall_back() {
        assert_eq!(pick_fan(Some(0), || Some(30)), (FanPath::Rpm, 0));
    }

    #[test]
    fn fan_falls_back_to_percent_then_zero() {
        assert_eq!(pick_fan(None, || Some(30)), (FanPath::Percent, 30));
        assert_eq!(pick_fan(None, || None), (FanPath::Unavailable, 0));
    }

    #[test]
    fn power_prefers_the_instant_field_and_does_not_query_usage() {
        let picked = pick_power(Some(61_000), || panic!("usage must not be read"));
        assert_eq!(picked, (PowerPath::Instant, 61_000));
    }

    #[test]
    fn power_falls_back_to_usage_then_zero() {
        assert_eq!(
            pick_power(None, || Some(30_000)),
            (PowerPath::PowerUsage, 30_000)
        );
        assert_eq!(pick_power(None, || None), (PowerPath::Unavailable, 0));
    }

    #[test]
    fn instant_power_accepts_integer_value_types() {
        assert_eq!(instant(Ok(SampleValue::U32(59_720))), Some(59_720));
        assert_eq!(instant(Ok(SampleValue::U64(59_720))), Some(59_720));
        assert_eq!(instant(Ok(SampleValue::I64(59_720))), Some(59_720));
    }

    #[test]
    fn instant_power_rejects_unusable_values() {
        assert_eq!(instant(Ok(SampleValue::F64(59.7))), None);
        assert_eq!(instant(Ok(SampleValue::I64(-1))), None);
        assert_eq!(instant(Ok(SampleValue::U64(u64::MAX))), None);
    }

    #[test]
    fn instant_power_handles_each_way_the_field_query_fails() {
        assert_eq!(instant(Err(NvmlError::NotSupported)), None);
        assert_eq!(instant_power_mw(Ok(vec![Err(NvmlError::Unknown)])), None);
        assert_eq!(instant_power_mw(Ok(vec![])), None);
        assert_eq!(instant_power_mw(Err(NvmlError::NotSupported)), None);
        let missing_symbol = NvmlError::FailedToLoadSymbol("nvmlDeviceGetFieldValues".into());
        assert_eq!(instant_power_mw(Err(missing_symbol)), None);
    }

    #[test]
    fn picks_the_device_on_the_wanted_bus() {
        let devices = [pci(3), pci(1), pci(0x41)];
        let picked = pick_by_bus(1, &devices).unwrap();
        assert_eq!(picked.bus_id, "00000000:01:00.0");
    }

    #[test]
    fn refuses_to_guess_when_no_device_matches() {
        let error = pick_by_bus(1, &[pci(3), pci(4)]).unwrap_err();

        assert!(matches!(error, Error::Sensor { .. }));
        assert_eq!(
            error.to_string(),
            "refusing to guess the GPU on PCI bus 0x01"
        );
        let cause = error.source().unwrap().to_string();
        assert!(cause.contains("00000000:03:00.0"), "{cause}");
    }

    #[test]
    fn refuses_to_guess_when_there_are_no_devices() {
        assert!(matches!(pick_by_bus(1, &[]), Err(Error::Sensor { .. })));
    }

    #[test]
    fn refuses_to_guess_between_two_pci_domains() {
        let other_domain = PciInfo {
            domain: 1,
            ..pci(1)
        };
        let devices = [pci(1), other_domain];
        assert!(matches!(
            pick_by_bus(1, &devices),
            Err(Error::Sensor { .. })
        ));
    }

    #[test]
    fn a_missing_library_is_an_error_not_a_crash() {
        let result = load_nvml(&["libaorus-lcd-test-no-such-library.so"]);
        assert!(matches!(result, Err(Error::Sensor { .. })));
    }

    #[test]
    fn fake_serves_its_sample_and_counts_reads() {
        let sample = SensorSample {
            gpu_temp_c: 42,
            ..SensorSample::default()
        };
        let mut fake = FakeSensors {
            sample,
            ..FakeSensors::default()
        };

        assert_eq!(fake.read().unwrap(), sample);
        assert_eq!(fake.read().unwrap(), sample);
        assert_eq!(fake.reads, 2);
    }

    #[test]
    fn fake_can_fail_like_a_lost_library() {
        let mut fake = FakeSensors {
            fail: true,
            ..FakeSensors::default()
        };

        assert!(matches!(fake.read(), Err(Error::Sensor { .. })));
        assert_eq!(fake.reads, 1);
    }

    #[test]
    fn sensors_can_move_to_the_daemon_thread() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<NvmlSensors>();
    }

    /// Run with `cargo test live_sample -- --ignored --nocapture` on a machine
    /// with the card. It only reads NVML, which does not touch the panel bus.
    /// It samples for a while to show whether `nvmlDeviceGetPowerUsage` moves.
    #[test]
    #[ignore = "needs an NVIDIA GPU on PCI bus 1"]
    fn live_sample() {
        const BUS_OF_THE_RTX_5090: u32 = 1;
        const ROUNDS: usize = 8;

        let mut sensors = NvmlSensors::new(BUS_OF_THE_RTX_5090).unwrap();
        println!("device: {}", sensors.pci_bus_id);

        for _ in 0..ROUNDS {
            let started = std::time::Instant::now();
            let sample = sensors.read().unwrap();
            let elapsed = started.elapsed();
            println!("{sample:?}");
            println!(
                "paths: {:?} {:?}, read took {elapsed:?}",
                sensors.fan_path, sensors.power_path
            );

            let device = sensors
                .nvml
                .device_by_pci_bus_id(sensors.pci_bus_id.as_str())
                .unwrap();
            let fans = device.num_fans();
            println!("fans: {fans:?}");
            for fan in 0..fans.as_ref().map_or(0, |count| *count) {
                println!(
                    "  fan {fan}: rpm {:?}, percent {:?}",
                    device.fan_speed_rpm(fan),
                    device.fan_speed(fan)
                );
            }
            let fields = device.field_values_for(&[FieldId(NVML_FI_DEV_POWER_INSTANT)]);
            println!(
                "instant {:?} mW, power_usage {:?} mW",
                instant_power_mw(fields),
                device.power_usage()
            );

            std::thread::sleep(std::time::Duration::from_secs(1));
        }
    }
}
