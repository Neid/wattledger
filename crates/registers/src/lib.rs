//! Register maps for Modbus devices: loading and validating map files,
//! decoding raw register words into physical values, and rejecting values
//! that cannot be real.
//!
//! This crate does no I/O. The caller reads the declared blocks and hands the
//! words to a [`Decoder`], which keeps the small amount of state needed to
//! check that energy counters never run backwards.

mod decode;
mod decoder;
mod format;
mod map;

pub use decode::{decode_raw, to_f32, to_u32};
pub use decoder::{Decoder, Outcome, Rejection};
pub use format::format_value;
pub use map::{
    Block, DataType, DerivedOp, DeviceInfo, DeviceMap, MAX_BLOCK_REGISTERS, MapError,
    RegisterSource, Sensor, Source, StateClass, WordOrder,
};
