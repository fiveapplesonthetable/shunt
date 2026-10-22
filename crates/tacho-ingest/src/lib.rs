//! Ingestion parsers for tachograph.
//!
//! Pure crate: no sockets, no threads, no clocks.

pub mod emit;
pub mod hooks;
pub mod otlp;
pub mod otlp_server;
pub mod streams;
