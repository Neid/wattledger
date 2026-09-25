//! Consumers of readings. Each runs in its own task behind a bounded channel
//! (see [`crate::reading::Fanout`]) so none of them can stall polling.

mod discovery;
pub mod influx;
pub mod mqtt;
pub mod stdout;
