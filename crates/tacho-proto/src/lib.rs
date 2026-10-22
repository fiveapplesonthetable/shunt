//! Generated protobuf definitions for Perfetto and tachograph (tacho).
//!
//! Provides the typed body messages for tacho events, the unified `Event`
//! oneof, Perfetto trace definitions, and the raw binary `DESCRIPTOR_SET`
//! used for the `ExtensionDescriptor` packet.

#[allow(clippy::all)]
#[rustfmt::skip]
pub mod perfetto {
    pub mod protos {
        include!(concat!(env!("OUT_DIR"), "/perfetto.protos.rs"));
    }
}

#[allow(clippy::all)]
#[rustfmt::skip]
pub mod tacho {
    include!(concat!(env!("OUT_DIR"), "/tacho.rs"));
}

/// The binary `FileDescriptorSet` of `tacho.proto`, emitted in an `ExtensionDescriptor`
/// packet so Trace Processor can decode custom TrackEvent extension fields (4000-4099).
pub const DESCRIPTOR_SET: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tacho_descriptor.bin"));
