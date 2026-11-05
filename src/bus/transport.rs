//! The byte-level link to the panel controller: the trait, the real device, a
//! dry-run printer, a retry wrapper, and test doubles.
//!
//! [`LinuxI2c`] and the kernel structs just above it are the only code in the
//! crate that reaches hardware, and they are kept small enough to review by
//! eye.

use std::ffi::{c_int, c_void};
use std::fs::File;
use std::io::{Stdout, Write};
use std::marker::PhantomData;
use std::path::PathBuf;
use std::time::Duration;

use rustix::io::Errno;
use rustix::ioctl::{ioctl, Ioctl, IoctlOutput, Opcode};

use crate::error::{Error, Result};
use crate::panel::{Sleep, ThreadSleep};
use crate::protocol::{opcode_name, MAGIC};

/// A raw link to the panel controller.
///
/// There is deliberately no address parameter, here or on any implementation's
/// public API. The bus also carries an RGB controller where a stray read can
/// wedge the GPU's I2C engine, so the controller's address must stay a private
/// detail of the implementation that talks to the device.
///
/// A write and a following read are two separate transfers with no repeated
/// start between them, as in the reference's `i2c_rdwr` usage. Implementations
/// must not combine them.
///
/// The trait stays dyn-compatible so a command can hold a
/// `Box<dyn Transport>` and choose the real or dry-run link at runtime.
pub trait Transport {
    /// Sends `frame` as one I2C transfer. All of it is sent, or an error is
    /// returned.
    fn write(&mut self, frame: &[u8]) -> Result<()>;

    /// Fills all of `buf` from one I2C transfer. There is no byte count to
    /// check: a partial read is [`Error::ShortRead`](crate::Error::ShortRead),
    /// so a caller cannot mistake it for a full read-back. `buf`'s contents
    /// are unspecified after an error.
    fn read(&mut self, buf: &mut [u8]) -> Result<()>;
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        (**self).write(frame)
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        (**self).read(buf)
    }
}

/// The panel controller's 7-bit address, in the unshifted form `i2c-dev`
/// expects (NVAPI wants it shifted left by one, `i2c-dev` does not). This is
/// the only address in the crate: the RGB controller shares the bus, and a
/// read there can wedge the GPU's I2C engine. `docs/PROTOCOL.md` section 1.
const PANEL_ADDRESS: u16 = 0x61;

/// `I2C_RDWR` from `linux/i2c-dev.h`.
const I2C_RDWR: Opcode = 0x0707;

/// `I2C_M_RD` from `linux/i2c.h`. A write has no flag at all.
const I2C_M_RD: u16 = 0x0001;

/// `struct i2c_msg` from `linux/i2c.h`.
///
/// The constructors are the only way to make one, so `addr` cannot be anything
/// but [`PANEL_ADDRESS`]. The lifetime ties the message to the buffer behind
/// its raw pointer.
#[repr(C)]
#[derive(Debug)]
struct I2cMsg<'a> {
    addr: u16,
    flags: u16,
    len: u16,
    buf: *mut u8,
    buffer: PhantomData<&'a [u8]>,
}

impl<'a> I2cMsg<'a> {
    fn write(frame: &'a [u8]) -> Result<Self> {
        Ok(Self {
            addr: PANEL_ADDRESS,
            flags: 0,
            len: message_len(frame.len())?,
            // The kernel only reads from the buffer of a message without
            // I2C_M_RD, so this `*mut` is never written through.
            buf: frame.as_ptr().cast_mut(),
            buffer: PhantomData,
        })
    }

    fn read(buf: &'a mut [u8]) -> Result<Self> {
        Ok(Self {
            addr: PANEL_ADDRESS,
            flags: I2C_M_RD,
            len: message_len(buf.len())?,
            buf: buf.as_mut_ptr(),
            buffer: PhantomData,
        })
    }
}

/// Checks a transfer length. Zero is refused because a zero-length write is a
/// "quick write" probe, which the NVIDIA adapter rejects and unconnected DDC
/// ports falsely acknowledge (`docs/PROTOCOL.md` section 1). A length that does
/// not fit is refused because truncating it would send part of a frame.
fn message_len(len: usize) -> Result<u16> {
    match u16::try_from(len) {
        Ok(len) if len > 0 => Ok(len),
        _ => Err(Error::InvalidArgument(format!(
            "an I2C transfer must be 1 to {} bytes, not {len}",
            u16::MAX
        ))),
    }
}

/// `struct i2c_rdwr_ioctl_data` from `linux/i2c-dev.h`.
#[repr(C)]
#[derive(Debug)]
struct I2cRdwrIoctlData<'a> {
    msgs: *mut I2cMsg<'a>,
    nmsgs: u32,
}

impl<'a> I2cRdwrIoctlData<'a> {
    /// The argument for one transfer. Taking exactly one message keeps a write
    /// and its read-back in separate ioctls: two messages in one `I2C_RDWR`
    /// would join them with a repeated start, which the reference never does.
    fn single(msg: &mut I2cMsg<'a>) -> Self {
        Self {
            msgs: std::ptr::from_mut(msg),
            nmsgs: 1,
        }
    }
}

// SAFETY: `I2C_RDWR` takes a pointer to `struct i2c_rdwr_ioctl_data`, which
// `as_ptr` returns. It returns the number of messages transferred, and rustix
// has already turned a negative return into an error. It writes read data to
// userspace, hence `IS_MUTATING`.
unsafe impl Ioctl for I2cRdwrIoctlData<'_> {
    type Output = c_int;

    const IS_MUTATING: bool = true;

    fn opcode(&self) -> Opcode {
        I2C_RDWR
    }

    fn as_ptr(&mut self) -> *mut c_void {
        std::ptr::from_mut(self).cast()
    }

    unsafe fn output_from_ptr(
        transferred: IoctlOutput,
        _: *mut c_void,
    ) -> rustix::io::Result<c_int> {
        Ok(transferred)
    }
}

/// The real link: `/dev/i2c-N`, with every transfer sent to the panel
/// controller as one `I2C_RDWR` ioctl with one message, like `write_frame` and
/// `read_cmd` in `aorus_lcd.py`.
#[derive(Debug)]
pub struct LinuxI2c {
    file: File,
    bus: u32,
}

impl LinuxI2c {
    /// Opens `/dev/i2c-{bus}`. No traffic happens here, but an open handle can
    /// reach real hardware, so this belongs behind adapter discovery, the bus
    /// lock and the probe.
    pub fn open(bus: u32) -> Result<Self> {
        let path = device_path(bus);
        let file = File::options()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| device_error(format!("opening {}", path.display()), source))?;
        Ok(Self { file, bus })
    }

    /// Runs one transfer and returns how many messages the kernel completed.
    /// Taking the message by value keeps it alive for the whole call.
    fn transfer(&self, mut msg: I2cMsg<'_>) -> std::io::Result<c_int> {
        // SAFETY: `file` is an open descriptor, the argument matches `I2C_RDWR`
        // (see the `Ioctl` impl), `msg` is alive for the call, and its lifetime
        // keeps the buffer of `len` bytes borrowed for that time.
        let transferred = unsafe { ioctl(&self.file, I2cRdwrIoctlData::single(&mut msg)) }?;
        Ok(transferred)
    }

    fn transfer_error(&self, action: &str, source: std::io::Error) -> Error {
        let path = device_path(self.bus);
        device_error(format!("{action} {}", path.display()), source)
    }
}

impl Transport for LinuxI2c {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        match self.transfer(I2cMsg::write(frame)?) {
            Ok(1) => Ok(()),
            Ok(_) => Err(self.transfer_error("writing to the panel on", Errno::IO.into())),
            Err(source) => Err(self.transfer_error("writing to the panel on", source)),
        }
    }

    // The kernel fills a whole I2C_RDWR read message or fails, and does not
    // update `len`, so a completed message is a full read. Zero completed
    // messages is the only short read that can be seen here.
    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        let expected = buf.len();
        match self.transfer(I2cMsg::read(buf)?) {
            Ok(1) => Ok(()),
            Ok(_) => Err(Error::ShortRead {
                expected,
                actual: 0,
            }),
            Err(source) => Err(self.transfer_error("reading from the panel on", source)),
        }
    }
}

fn device_path(bus: u32) -> PathBuf {
    PathBuf::from(format!("/dev/i2c-{bus}"))
}

/// Builds an [`Error::I2c`], adding what the errno likely means.
fn device_error(action: String, source: std::io::Error) -> Error {
    let context = match errno_hint(&source) {
        Some(hint) => format!("{action}: {hint}"),
        None => action,
    };
    Error::I2c { context, source }
}

fn errno_hint(source: &std::io::Error) -> Option<&'static str> {
    match Errno::from_io_error(source)? {
        Errno::NOENT => Some("no such device, is the i2c-dev kernel module loaded?"),
        Errno::ACCESS => Some("permission denied, is this user in the i2c group?"),
        Errno::IO | Errno::NXIO | Errno::REMOTEIO => Some("no answer from the panel controller"),
        _ => None,
    }
}

/// How many bytes of a payload chunk its summary line shows.
const SUMMARY_BYTES: usize = 8;

const DUMP_ROW_BYTES: usize = 16;

/// A link that opens nothing: it prints what would be sent, so holding one
/// proves no frame can reach a device.
///
/// Commands are always dumped in full. A payload chunk gets a one-line summary
/// unless `verbose` is set, because an upload has hundreds of them.
#[derive(Debug)]
pub struct DryRun<W = Stdout> {
    out: W,
    verbose: bool,
}

impl DryRun {
    /// Prints to stdout.
    pub fn new(verbose: bool) -> Self {
        Self::with_output(std::io::stdout(), verbose)
    }
}

impl<W: Write> DryRun<W> {
    /// Prints to `out`.
    pub fn with_output(out: W, verbose: bool) -> Self {
        Self { out, verbose }
    }

    fn print_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        let len = frame.len();
        // Payload chunks carry no magic, which tells them apart from commands.
        if frame.get(1..5) == Some(&MAGIC) {
            let opcode = frame[0];
            let name = opcode_name(opcode);
            writeln!(self.out, "{opcode:02X} {name} ({len} bytes)")?;
            hex_dump(&mut self.out, frame)
        } else if self.verbose {
            writeln!(self.out, "payload chunk ({len} bytes)")?;
            hex_dump(&mut self.out, frame)
        } else {
            let start = hex_row(&frame[..len.min(SUMMARY_BYTES)]);
            writeln!(self.out, "payload chunk ({len} bytes): {start}")
        }
    }
}

impl<W: Write> Transport for DryRun<W> {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        // Refuses what the real link refuses, so a preview cannot pass a frame
        // that would fail for real.
        message_len(frame.len())?;
        self.print_frame(frame).map_err(|source| Error::I2c {
            context: "printing the dry-run dump".into(),
            source,
        })
    }

    fn read(&mut self, _buf: &mut [u8]) -> Result<()> {
        Err(Error::ReadbackUnavailable)
    }
}

fn hex_dump(out: &mut impl Write, bytes: &[u8]) -> std::io::Result<()> {
    for (row, chunk) in bytes.chunks(DUMP_ROW_BYTES).enumerate() {
        writeln!(out, "  {:04X}  {}", row * DUMP_ROW_BYTES, hex_row(chunk))?;
    }
    Ok(())
}

fn hex_row(bytes: &[u8]) -> String {
    let hex: Vec<String> = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    hex.join(" ")
}

/// Attempts per transfer. `RetryingI2cBus.cs` in the C# reference.
const MAX_ATTEMPTS: u32 = 5;

/// Fixed wait between attempts, as in `RetryingI2cBus.cs`.
const RETRY_DELAY: Duration = Duration::from_millis(200);

/// Retries transfers that failed for a reason that may pass: the GPU's I2C
/// engine sometimes rejects a valid transfer while the bus is healthy, which
/// would otherwise surface as a frozen panel.
///
/// For uploads and the daemon. The probe stays on the raw transport, so that
/// presence detection fails fast.
#[derive(Debug)]
pub struct Retrying<T, S = ThreadSleep> {
    inner: T,
    sleep: S,
}

impl<T: Transport> Retrying<T> {
    /// Wraps `inner`, waiting on the real clock.
    pub fn new(inner: T) -> Self {
        Self::with_sleep(inner, ThreadSleep)
    }
}

impl<T: Transport, S: Sleep> Retrying<T, S> {
    /// Wraps `inner`, waiting through `sleep`.
    pub fn with_sleep(inner: T, sleep: S) -> Self {
        Self { inner, sleep }
    }

    /// Takes the wrapper apart, so a test can inspect its `sleep`.
    pub fn into_parts(self) -> (T, S) {
        (self.inner, self.sleep)
    }

    fn with_retries(&mut self, mut attempt: impl FnMut(&mut T) -> Result<()>) -> Result<()> {
        for _ in 1..MAX_ATTEMPTS {
            match attempt(&mut self.inner) {
                Err(error) if is_transient(&error) => self.sleep.sleep(RETRY_DELAY),
                outcome => return outcome,
            }
        }
        attempt(&mut self.inner)
    }
}

impl<T: Transport, S: Sleep> Transport for Retrying<T, S> {
    fn write(&mut self, frame: &[u8]) -> Result<()> {
        self.with_retries(|inner| inner.write(frame))
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<()> {
        self.with_retries(|inner| inner.read(buf))
    }
}

/// Only a failed transfer may pass. A short read must reach the caller, since
/// it is never presence, and dry-run has no device to wait for.
fn is_transient(error: &Error) -> bool {
    matches!(error, Error::I2c { .. })
}

/// Test doubles. This is plain `pub` so that integration tests in `tests/` can
/// use it. Nothing here touches a device.
pub mod testing {
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, Mutex, MutexGuard};

    use rustix::io::Errno;

    use super::Transport;
    use crate::error::{Error, Result};

    /// An [`Error::I2c`] like a failed transfer on a real bus, for injection.
    pub fn i2c_error() -> Error {
        Error::I2c {
            context: "injected by the mock".into(),
            source: Errno::IO.into(),
        }
    }

    /// A scripted transport. It records writes, serves scripted reads in order,
    /// and fails on request.
    ///
    /// Clones share one state, so a test can keep a clone to inspect after
    /// handing the other to the code under test, even inside a
    /// `Box<dyn Transport>`.
    #[derive(Debug, Clone, Default)]
    pub struct Mock {
        state: Arc<Mutex<State>>,
    }

    #[derive(Debug, Default)]
    struct State {
        writes: Vec<Vec<u8>>,
        write_attempts: usize,
        write_failures: HashMap<usize, Error>,
        reads: VecDeque<Result<Vec<u8>>>,
        read_attempts: usize,
    }

    impl Mock {
        /// A mock with nothing scripted.
        pub fn new() -> Self {
            Self::default()
        }

        /// Queues the bytes for a coming read. Like a real read, fewer bytes
        /// than asked for is [`Error::ShortRead`]. Bytes beyond the buffer are
        /// dropped.
        pub fn script_read(&self, bytes: impl Into<Vec<u8>>) {
            self.state().reads.push_back(Ok(bytes.into()));
        }

        /// Queues a failure in the same queue as [`Mock::script_read`].
        pub fn script_read_error(&self, error: Error) {
            self.state().reads.push_back(Err(error));
        }

        /// Makes write attempt number `attempt` fail with `error`, counted from
        /// 0 over the mock's whole life and including failed attempts.
        pub fn fail_write(&self, attempt: usize, error: Error) {
            self.state().write_failures.insert(attempt, error);
        }

        /// The frames written successfully, in order. Failed attempts are not
        /// included: see [`Mock::write_attempts`].
        pub fn writes(&self) -> Vec<Vec<u8>> {
            self.state().writes.clone()
        }

        /// All calls to `write`, failed ones included.
        pub fn write_attempts(&self) -> usize {
            self.state().write_attempts
        }

        /// All calls to `read`, failed ones included.
        pub fn read_attempts(&self) -> usize {
            self.state().read_attempts
        }

        fn state(&self) -> MutexGuard<'_, State> {
            self.state.lock().unwrap()
        }
    }

    impl Transport for Mock {
        fn write(&mut self, frame: &[u8]) -> Result<()> {
            let mut state = self.state();
            let attempt = state.write_attempts;
            state.write_attempts += 1;
            if let Some(error) = state.write_failures.remove(&attempt) {
                return Err(error);
            }
            state.writes.push(frame.to_vec());
            Ok(())
        }

        fn read(&mut self, buf: &mut [u8]) -> Result<()> {
            let mut state = self.state();
            state.read_attempts += 1;
            // An unscripted read acts as an absent device rather than a panic,
            // so a test of the failure path needs no script.
            let bytes = state.reads.pop_front().unwrap_or_else(|| {
                Err(Error::I2c {
                    context: "the mock has no scripted read left".into(),
                    source: Errno::NXIO.into(),
                })
            })?;
            let Some(scripted) = bytes.get(..buf.len()) else {
                return Err(Error::ShortRead {
                    expected: buf.len(),
                    actual: bytes.len(),
                });
            };
            buf.copy_from_slice(scripted);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::mem::{offset_of, size_of};

    use super::testing::{i2c_error, Mock};
    use super::*;
    use crate::panel::RecordingSleep;

    fn dump(verbose: bool, frame: &[u8]) -> String {
        let mut out = Vec::new();
        DryRun::with_output(&mut out, verbose).write(frame).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn command_frame(opcode: u8) -> [u8; 256] {
        let mut frame = [0u8; 256];
        frame[0] = opcode;
        frame[1..5].copy_from_slice(&MAGIC);
        frame
    }

    #[test]
    fn ioctl_constants_match_the_kernel_headers() {
        assert_eq!(I2C_RDWR, 0x0707);
        assert_eq!(I2C_M_RD, 0x0001);
    }

    #[test]
    fn i2c_msg_has_the_kernel_layout() {
        let pointer = size_of::<*mut u8>();

        assert_eq!(offset_of!(I2cMsg, addr), 0);
        assert_eq!(offset_of!(I2cMsg, flags), 2);
        assert_eq!(offset_of!(I2cMsg, len), 4);
        assert_eq!(offset_of!(I2cMsg, buf), 8);
        assert_eq!(size_of::<I2cMsg>(), 8 + pointer);
    }

    #[test]
    fn rdwr_data_has_the_kernel_layout() {
        let pointer = size_of::<*mut u8>();

        assert_eq!(offset_of!(I2cRdwrIoctlData, msgs), 0);
        assert_eq!(offset_of!(I2cRdwrIoctlData, nmsgs), pointer);
        assert_eq!(size_of::<I2cRdwrIoctlData>(), 2 * pointer);
    }

    #[test]
    fn write_message_is_addressed_to_the_panel_with_no_flags() {
        let frame = [0xEB; 256];

        let msg = I2cMsg::write(&frame).unwrap();

        assert_eq!(msg.addr, 0x61);
        assert_eq!(msg.flags, 0);
        assert_eq!(msg.len, 256);
        assert_eq!(msg.buf.cast_const(), frame.as_ptr());
    }

    #[test]
    fn read_message_is_addressed_to_the_panel_with_the_read_flag() {
        let mut buf = [0u8; 8];
        let start = buf.as_mut_ptr();

        let msg = I2cMsg::read(&mut buf).unwrap();

        assert_eq!(msg.addr, 0x61);
        assert_eq!(msg.flags, 1);
        assert_eq!(msg.len, 8);
        assert_eq!(msg.buf, start);
    }

    #[test]
    fn a_transfer_is_one_message_in_one_ioctl() {
        let frame = [0xEB; 8];
        let mut msg = I2cMsg::write(&frame).unwrap();
        let expected = std::ptr::from_mut(&mut msg);

        let mut data = I2cRdwrIoctlData::single(&mut msg);

        assert_eq!(data.nmsgs, 1);
        assert_eq!(data.msgs, expected);
        assert_eq!(data.opcode(), 0x0707);
        assert_eq!(data.as_ptr(), std::ptr::from_mut(&mut data).cast());
    }

    #[test]
    fn empty_transfers_are_refused() {
        assert!(matches!(I2cMsg::write(&[]), Err(Error::InvalidArgument(_))));
        assert!(matches!(
            I2cMsg::read(&mut []),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn lengths_that_do_not_fit_the_message_are_refused() {
        assert_eq!(message_len(usize::from(u16::MAX)).unwrap(), u16::MAX);
        assert!(matches!(
            message_len(usize::from(u16::MAX) + 1),
            Err(Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn device_path_names_the_bus() {
        assert_eq!(device_path(7), PathBuf::from("/dev/i2c-7"));
    }

    #[test]
    fn errno_maps_to_a_hint_and_keeps_the_source() {
        let cases = [
            (Errno::NOENT, "i2c-dev kernel module"),
            (Errno::ACCESS, "i2c group"),
            (Errno::IO, "no answer"),
            (Errno::NXIO, "no answer"),
            (Errno::REMOTEIO, "no answer"),
        ];

        for (errno, hint) in cases {
            let error = device_error("opening /dev/i2c-7".into(), errno.into());

            let message = error.to_string();
            assert!(message.starts_with("opening /dev/i2c-7: "), "{message}");
            assert!(message.contains(hint), "{message}");
            let Error::I2c { source, .. } = error else {
                panic!("not an I2c error: {error:?}");
            };
            assert_eq!(source.raw_os_error(), Some(errno.raw_os_error()));
        }
    }

    #[test]
    fn an_unknown_errno_adds_no_guess() {
        let error = device_error("opening /dev/i2c-7".into(), Errno::BUSY.into());

        assert_eq!(error.to_string(), "opening /dev/i2c-7");
    }

    #[test]
    fn dry_run_dumps_a_whole_command_with_its_name() {
        let output = dump(false, &command_frame(0xE7));

        assert!(output.starts_with("E7 OpenLcd (256 bytes)\n"), "{output}");
        assert!(output.contains("  0000  E7 CB 55 AC 38 00"), "{output}");
        assert!(output.contains("  00F0  00 00"), "{output}");
    }

    #[test]
    fn dry_run_names_an_unknown_command_as_unknown() {
        let output = dump(false, &command_frame(0x42));

        assert!(output.starts_with("42 unknown opcode"), "{output}");
    }

    #[test]
    fn dry_run_summarizes_a_payload_chunk() {
        let output = dump(false, &[0x11; 256]);

        assert_eq!(
            output,
            "payload chunk (256 bytes): 11 11 11 11 11 11 11 11\n"
        );
    }

    #[test]
    fn dry_run_dumps_a_payload_chunk_when_verbose() {
        let output = dump(true, &[0x11; 256]);

        assert!(
            output.starts_with("payload chunk (256 bytes)\n"),
            "{output}"
        );
        assert!(output.contains("  00F0  11 11"), "{output}");
    }

    #[test]
    fn dry_run_handles_a_frame_shorter_than_the_magic() {
        assert_eq!(dump(false, &[0xAB]), "payload chunk (1 bytes): AB\n");
    }

    #[test]
    fn dry_run_refuses_an_empty_frame_like_the_real_link() {
        let mut dry_run = DryRun::with_output(Vec::new(), false);

        assert!(matches!(dry_run.write(&[]), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn dry_run_reads_report_that_read_backs_are_unavailable() {
        let mut dry_run = DryRun::with_output(Vec::new(), false);
        let mut buf = [0u8; 8];

        assert!(matches!(
            dry_run.read(&mut buf),
            Err(Error::ReadbackUnavailable)
        ));
    }

    #[test]
    fn retrying_recovers_after_four_failed_writes() {
        let mock = Mock::new();
        for attempt in 0..4 {
            mock.fail_write(attempt, i2c_error());
        }
        let mut retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());

        retrying.write(&[0xE7]).unwrap();

        assert_eq!(mock.write_attempts(), 5);
        assert_eq!(mock.writes(), [[0xE7]]);
        let (_, sleep) = retrying.into_parts();
        assert_eq!(sleep.delays, [Duration::from_millis(200); 4]);
    }

    #[test]
    fn retrying_gives_up_after_five_attempts_without_a_last_sleep() {
        let mock = Mock::new();
        for attempt in 0..5 {
            mock.fail_write(attempt, i2c_error());
        }
        let mut retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());

        assert!(matches!(retrying.write(&[0xE7]), Err(Error::I2c { .. })));

        assert_eq!(mock.write_attempts(), 5);
        assert!(mock.writes().is_empty());
        let (_, sleep) = retrying.into_parts();
        assert_eq!(sleep.delays.len(), 4);
    }

    #[test]
    fn retrying_does_not_wait_after_a_success() {
        let mock = Mock::new();
        let mut retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());

        retrying.write(&[0xE7]).unwrap();

        assert_eq!(mock.write_attempts(), 1);
        let (_, sleep) = retrying.into_parts();
        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn retrying_retries_failed_reads() {
        let mock = Mock::new();
        mock.script_read_error(i2c_error());
        mock.script_read([1, 2, 3, 4]);
        let mut retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());
        let mut buf = [0u8; 4];

        retrying.read(&mut buf).unwrap();

        assert_eq!(buf, [1, 2, 3, 4]);
        assert_eq!(mock.read_attempts(), 2);
    }

    #[test]
    fn retrying_passes_a_short_read_straight_through() {
        let mock = Mock::new();
        mock.script_read([1, 2]);
        let mut retrying = Retrying::with_sleep(mock.clone(), RecordingSleep::default());
        let mut buf = [0u8; 8];

        let result = retrying.read(&mut buf);

        assert!(matches!(
            result,
            Err(Error::ShortRead {
                expected: 8,
                actual: 2
            })
        ));
        assert_eq!(mock.read_attempts(), 1);
        let (_, sleep) = retrying.into_parts();
        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn retrying_passes_readback_unavailable_straight_through() {
        let dry_run = DryRun::with_output(Vec::new(), false);
        let mut retrying = Retrying::with_sleep(dry_run, RecordingSleep::default());
        let mut buf = [0u8; 8];

        let result = retrying.read(&mut buf);

        assert!(matches!(result, Err(Error::ReadbackUnavailable)));
        let (_, sleep) = retrying.into_parts();
        assert!(sleep.delays.is_empty());
    }

    #[test]
    fn mock_turns_a_short_scripted_read_into_short_read() {
        let mut mock = Mock::new();
        mock.script_read([1, 2, 3]);
        let mut buf = [0u8; 8];

        assert!(matches!(
            mock.read(&mut buf),
            Err(Error::ShortRead {
                expected: 8,
                actual: 3
            })
        ));
    }

    #[test]
    fn mock_drops_scripted_bytes_beyond_the_buffer() {
        let mut mock = Mock::new();
        mock.script_read([1, 2, 3]);
        let mut buf = [0u8; 2];

        mock.read(&mut buf).unwrap();

        assert_eq!(buf, [1, 2]);
    }

    #[test]
    fn mock_acts_as_an_absent_device_when_no_read_is_scripted() {
        let mut mock = Mock::new();
        let mut buf = [0u8; 8];

        assert!(matches!(mock.read(&mut buf), Err(Error::I2c { .. })));
        assert_eq!(mock.read_attempts(), 1);
    }
}
