//! The byte-level link to the panel controller.

use crate::error::Result;

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
