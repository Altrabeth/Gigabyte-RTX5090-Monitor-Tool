//! Finds the panel's I2C adapter by its sysfs name and validates `--bus`.
//!
//! This module only reads names under sysfs. It never opens `/dev/i2c-*`, so
//! nothing here can reach a device.

use std::fmt::Write as _;
use std::path::Path;
use std::{fs, io};

use crate::error::{Error, Result};

/// Where the kernel lists I2C adapters while `i2c-dev` is loaded.
pub const SYSFS_ROOT: &str = "/sys/class/i2c-dev";

/// Name prefix of the adapter that carries the panel controller. The ` at` is
/// what stops adapters numbered 10 or 11 from matching. From `aorus_lcd.py`
/// (`NVIDIA_BUS_PREFIX`).
const PANEL_ADAPTER_PREFIX: &str = "NVIDIA i2c adapter 1 at";

/// Name prefix of any adapter on an NVIDIA GPU. The trailing space keeps this
/// to the driver's `NVIDIA i2c adapter <n> at ...` names and nothing longer.
const NVIDIA_ADAPTER_PREFIX: &str = "NVIDIA i2c adapter ";

/// An I2C adapter as listed by sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    /// The `N` of `/dev/i2c-N`. Assigned by the kernel at load time, so it says
    /// nothing about which adapter this is.
    pub bus: u32,
    /// The adapter's sysfs name, without the trailing newline.
    pub name: String,
}

impl Adapter {
    /// The PCI bus of the device behind this adapter, from a name ending in
    /// ` at 1:00.0` or ` at 0000:01:00.0`. The field is hexadecimal. `None` if
    /// the name carries no PCI address.
    pub fn pci_bus(&self) -> Option<u32> {
        let (_, address) = self.name.rsplit_once(" at ")?;
        // Counted from the end because the domain field is optional.
        let mut fields = address.rsplit(':');
        let _device_and_function = fields.next();
        u32::from_str_radix(fields.next()?, 16).ok()
    }
}

/// Finds the adapter to use, by the rules of [`select_adapter`], among those
/// under `root`. `root` is [`SYSFS_ROOT`] outside tests.
pub fn discover(root: &Path, bus_override: Option<u32>) -> Result<Adapter> {
    select_adapter(&list_adapters(root)?, bus_override).cloned()
}

/// Lists the adapters under `root`, sorted by bus number.
///
/// A missing `root` is an empty list, because that is how an unloaded `i2c-dev`
/// looks. An entry whose `name` cannot be read is left out: an adapter we cannot
/// identify must not be selectable.
pub fn list_adapters(root: &Path) -> Result<Vec<Adapter>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Error::Adapter(format!(
                "cannot list I2C adapters in {}: {error}",
                root.display()
            )))
        }
    };

    let mut adapters: Vec<Adapter> = entries
        .filter_map(|entry| read_adapter(&entry.ok()?.path()))
        .collect();
    adapters.sort_by_key(|adapter| adapter.bus);
    Ok(adapters)
}

/// Reads one `i2c-N` entry, or returns `None` if `dir` is not a readable one.
fn read_adapter(dir: &Path) -> Option<Adapter> {
    let bus = dir.file_name()?.to_str()?.strip_prefix("i2c-")?;
    let name = fs::read_to_string(dir.join("name")).ok()?;
    Some(Adapter {
        bus: bus.parse().ok()?,
        name: name.trim_end().to_owned(),
    })
}

/// Chooses the adapter to use from `adapters`, sorted as [`list_adapters`] sorts.
///
/// Without an override this is the first adapter named for the panel, so a
/// second GPU of the same kind is ignored. With one, `--bus N` must still be an
/// NVIDIA adapter: this is stricter than `aorus_lcd.py` on purpose, because an
/// I801 SMBus or DesignWare adapter must never receive a frame.
pub fn select_adapter(adapters: &[Adapter], bus_override: Option<u32>) -> Result<&Adapter> {
    match bus_override {
        Some(bus) => check_override(adapters, bus),
        None => adapters
            .iter()
            .find(|adapter| adapter.name.starts_with(PANEL_ADAPTER_PREFIX))
            .ok_or_else(|| {
                not_found(
                    &format!("no adapter named \"{PANEL_ADAPTER_PREFIX} ...\""),
                    adapters,
                )
            }),
    }
}

/// Returns the adapter for `--bus bus`, or refuses it.
fn check_override(adapters: &[Adapter], bus: u32) -> Result<&Adapter> {
    let Some(adapter) = adapters.iter().find(|adapter| adapter.bus == bus) else {
        return Err(not_found(
            &format!("no adapter with a readable name at --bus {bus}"),
            adapters,
        ));
    };
    if !adapter.name.starts_with(NVIDIA_ADAPTER_PREFIX) {
        return Err(Error::Adapter(format!(
            "refusing --bus {bus}: i2c-{bus} is \"{}\", not an NVIDIA adapter",
            adapter.name
        )));
    }
    Ok(adapter)
}

/// Builds the failure for a missing adapter: what was wanted, what was seen,
/// and the usual cure.
fn not_found(problem: &str, adapters: &[Adapter]) -> Error {
    let mut message = format!("{problem}; adapters seen:\n");
    if adapters.is_empty() {
        message.push_str("  (none)\n");
    }
    for adapter in adapters {
        // Writing to a String cannot fail.
        let _ = writeln!(message, "  i2c-{}: {}", adapter.bus, adapter.name);
    }
    message.push_str("is i2c-dev loaded? try: sudo modprobe i2c-dev");
    Error::Adapter(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    const PANEL: &str = "NVIDIA i2c adapter 1 at 1:00.0";
    const OTHER_NVIDIA: &str = "NVIDIA i2c adapter 3 at 1:00.0";
    const SMBUS: &str = "SMBus I801 adapter at 0000:00:1f.4";
    const DESIGNWARE: &str = "Synopsys DesignWare I2C adapter";

    /// A throwaway sysfs tree, removed on drop.
    struct FakeSysfs(PathBuf);

    impl FakeSysfs {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let unique = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("aorus-lcd-sysfs-{}-{unique}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn with_adapters(adapters: &[(u32, &str)]) -> Self {
            let sysfs = Self::new();
            for (bus, name) in adapters {
                let dir = sysfs.add_dir(&format!("i2c-{bus}"));
                // The kernel ends the file with a newline.
                fs::write(dir.join("name"), format!("{name}\n")).unwrap();
            }
            sysfs
        }

        fn add_dir(&self, entry: &str) -> PathBuf {
            let dir = self.0.join(entry);
            fs::create_dir(&dir).unwrap();
            dir
        }

        fn list(&self) -> Vec<Adapter> {
            list_adapters(&self.0).unwrap()
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn adapter(bus: u32, name: &str) -> Adapter {
        Adapter {
            bus,
            name: name.to_owned(),
        }
    }

    fn message(error: Error) -> String {
        assert!(matches!(error, Error::Adapter(_)), "{error:?}");
        error.to_string()
    }

    #[test]
    fn lists_adapters_in_numeric_order_without_the_newline() {
        let sysfs = FakeSysfs::with_adapters(&[(10, DESIGNWARE), (2, OTHER_NVIDIA), (0, PANEL)]);

        assert_eq!(
            sysfs.list(),
            [
                adapter(0, PANEL),
                adapter(2, OTHER_NVIDIA),
                adapter(10, DESIGNWARE),
            ]
        );
    }

    #[test]
    fn a_missing_root_lists_nothing() {
        let sysfs = FakeSysfs::new();

        assert_eq!(list_adapters(&sysfs.0.join("absent")).unwrap(), []);
    }

    #[test]
    fn an_entry_without_a_name_file_is_left_out() {
        let sysfs = FakeSysfs::with_adapters(&[(4, PANEL)]);
        sysfs.add_dir("i2c-0");

        assert_eq!(sysfs.list(), [adapter(4, PANEL)]);
    }

    #[test]
    fn entries_not_named_i2c_n_are_left_out() {
        let sysfs = FakeSysfs::with_adapters(&[(4, PANEL)]);
        for entry in ["i2c-x", "spi-0"] {
            let dir = sysfs.add_dir(entry);
            fs::write(dir.join("name"), PANEL).unwrap();
        }

        assert_eq!(sysfs.list(), [adapter(4, PANEL)]);
    }

    #[test]
    fn autodetect_picks_adapter_1_among_the_names_on_this_machine() {
        let sysfs = FakeSysfs::with_adapters(&[
            (0, PANEL),
            (1, OTHER_NVIDIA),
            (2, "NVIDIA i2c adapter 4 at 1:00.0"),
            (3, "NVIDIA i2c adapter 5 at 1:00.0"),
            (4, "NVIDIA i2c adapter 6 at 1:00.0"),
            (5, DESIGNWARE),
            (6, DESIGNWARE),
            (7, DESIGNWARE),
            (8, SMBUS),
        ]);

        assert_eq!(discover(&sysfs.0, None).unwrap(), adapter(0, PANEL));
    }

    #[test]
    fn autodetect_goes_by_name_when_adapter_1_is_not_lowest_numbered() {
        let sysfs = FakeSysfs::with_adapters(&[(0, SMBUS), (1, OTHER_NVIDIA), (7, PANEL)]);

        assert_eq!(discover(&sysfs.0, None).unwrap(), adapter(7, PANEL));
    }

    #[test]
    fn autodetect_takes_the_first_of_two_gpus() {
        let second_gpu = "NVIDIA i2c adapter 1 at 41:00.0";
        let sysfs = FakeSysfs::with_adapters(&[(9, second_gpu), (3, PANEL)]);

        assert_eq!(discover(&sysfs.0, None).unwrap(), adapter(3, PANEL));
    }

    #[test]
    fn autodetect_never_matches_adapters_numbered_10_or_11() {
        let adapters = [
            adapter(0, "NVIDIA i2c adapter 10 at 1:00.0"),
            adapter(1, "NVIDIA i2c adapter 11 at 1:00.0"),
        ];

        assert!(select_adapter(&adapters, None).is_err());
    }

    #[test]
    fn autodetect_ignores_an_adapter_without_a_name() {
        let sysfs = FakeSysfs::with_adapters(&[(4, OTHER_NVIDIA)]);
        sysfs.add_dir("i2c-0");

        let text = message(discover(&sysfs.0, None).unwrap_err());

        assert!(text.contains(&format!("i2c-4: {OTHER_NVIDIA}")), "{text}");
    }

    #[test]
    fn the_failure_lists_what_was_seen_and_hints_at_modprobe() {
        let adapters = [adapter(5, DESIGNWARE), adapter(8, SMBUS)];

        let text = message(select_adapter(&adapters, None).unwrap_err());

        assert!(text.contains(&format!("i2c-5: {DESIGNWARE}")), "{text}");
        assert!(text.contains(&format!("i2c-8: {SMBUS}")), "{text}");
        assert!(text.contains("modprobe i2c-dev"), "{text}");
    }

    #[test]
    fn no_adapters_at_all_still_hints_at_modprobe() {
        let sysfs = FakeSysfs::new();

        let text = message(discover(&sysfs.0, None).unwrap_err());

        assert!(text.contains("(none)"), "{text}");
        assert!(text.contains("modprobe i2c-dev"), "{text}");
    }

    #[test]
    fn override_accepts_any_nvidia_adapter() {
        let sysfs = FakeSysfs::with_adapters(&[(0, PANEL), (1, OTHER_NVIDIA)]);

        assert_eq!(
            discover(&sysfs.0, Some(1)).unwrap(),
            adapter(1, OTHER_NVIDIA)
        );
    }

    #[test]
    fn override_refuses_a_non_nvidia_adapter_and_shows_its_name() {
        for name in [SMBUS, DESIGNWARE] {
            let sysfs = FakeSysfs::with_adapters(&[(0, PANEL), (8, name)]);

            let text = message(discover(&sysfs.0, Some(8)).unwrap_err());

            assert!(text.contains("refusing --bus 8"), "{text}");
            assert!(text.contains(name), "{text}");
        }
    }

    #[test]
    fn override_refuses_a_name_that_only_begins_like_nvidia() {
        let adapters = [adapter(0, "NVIDIA i2c adapterless")];

        assert!(select_adapter(&adapters, Some(0)).is_err());
    }

    #[test]
    fn override_refuses_an_unknown_bus_and_lists_what_was_seen() {
        let adapters = [adapter(0, PANEL)];

        let text = message(select_adapter(&adapters, Some(9)).unwrap_err());

        assert!(text.contains("--bus 9"), "{text}");
        assert!(text.contains(&format!("i2c-0: {PANEL}")), "{text}");
        assert!(text.contains("modprobe i2c-dev"), "{text}");
    }

    #[test]
    fn override_refuses_an_adapter_without_a_name() {
        let sysfs = FakeSysfs::with_adapters(&[(4, PANEL)]);
        sysfs.add_dir("i2c-0");

        assert!(discover(&sysfs.0, Some(0)).is_err());
    }

    #[test]
    fn pci_bus_is_the_hex_field_before_the_colon() {
        assert_eq!(adapter(0, PANEL).pci_bus(), Some(1));
        assert_eq!(
            adapter(0, "NVIDIA i2c adapter 3 at 41:00.0").pci_bus(),
            Some(0x41)
        );
        assert_eq!(
            adapter(0, "NVIDIA i2c adapter 1 at 0a:00.0").pci_bus(),
            Some(10)
        );
    }

    #[test]
    fn pci_bus_skips_a_domain_field() {
        assert_eq!(
            adapter(0, "SMBus I801 adapter at 0000:41:1f.4").pci_bus(),
            Some(0x41)
        );
    }

    #[test]
    fn pci_bus_is_none_without_a_pci_address() {
        assert_eq!(adapter(0, DESIGNWARE).pci_bus(), None);
        assert_eq!(adapter(0, "adapter at nowhere").pci_bus(), None);
        assert_eq!(adapter(0, "adapter at xy:00.0").pci_bus(), None);
    }
}
