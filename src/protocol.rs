//! Pure frame builders, opcodes and read-back parsers. No I/O.

/// One reading of the GPU sensors, the input to an `E3` sensor-feed frame.
///
/// Fields are wider than their wire encoding on purpose: sensor code stores
/// what it read, and the `E3` builder clamps each value to its `u8` or `u16`
/// field. Layout reference: `docs/PROTOCOL.md` section 4 and C#
/// `SensorSample.cs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SensorSample {
    /// GPU temperature in degrees Celsius.
    pub gpu_temp_c: u32,
    /// Graphics clock in MHz.
    pub gpu_clock_mhz: u32,
    /// GPU utilization in percent.
    pub gpu_usage_percent: u32,
    /// Fan speed as displayed by the panel, which prints the raw number: RPM
    /// where available, otherwise percent.
    pub fan_speed: u32,
    /// Memory clock in MHz.
    pub ram_clock_mhz: u32,
    /// VRAM used, as a percentage of the total.
    pub ram_usage_percent: u32,
    /// Frames per second. NVML has no FPS source, so this stays 0.
    pub fps: u32,
    /// Board power in whole watts. GCC sent deci-watts, but the panel prints
    /// the raw number.
    pub tgp_watts: u32,
}
