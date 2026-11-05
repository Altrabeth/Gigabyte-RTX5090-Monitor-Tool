//! Access to the panel's I2C bus: finding the adapter, moving bytes, and
//! keeping other processes off the bus. One file per concern, so the three can
//! change independently.

pub mod discovery;
pub mod lock;
pub mod transport;
