//! Protobuf wire format encoding primitives.

/// Appends a varint-encoded 64-bit integer to the buffer.
#[inline]
pub fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push(((value as u8) & 0x7f) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Appends a protobuf field tag (field number and wire type).
#[inline]
pub fn encode_tag(field_number: u32, wire_type: u32, buf: &mut Vec<u8>) {
    encode_varint(((field_number as u64) << 3) | (wire_type as u64), buf);
}

/// Encodes a varint field (wire type 0).
#[inline]
pub fn encode_varint_field(field_number: u32, value: u64, buf: &mut Vec<u8>) {
    encode_tag(field_number, 0, buf);
    encode_varint(value, buf);
}

/// Encodes a fixed64 field (wire type 1, 8 little-endian bytes).
#[inline]
pub fn encode_fixed64_field(field_number: u32, value: u64, buf: &mut Vec<u8>) {
    encode_tag(field_number, 1, buf);
    buf.extend_from_slice(&value.to_le_bytes());
}

/// Encodes a length-delimited bytes field (wire type 2).
#[inline]
pub fn encode_bytes_field(field_number: u32, bytes: &[u8], buf: &mut Vec<u8>) {
    encode_tag(field_number, 2, buf);
    encode_varint(bytes.len() as u64, buf);
    buf.extend_from_slice(bytes);
}

/// Encodes a length-delimited UTF-8 string field (wire type 2).
#[inline]
pub fn encode_string_field(field_number: u32, s: &str, buf: &mut Vec<u8>) {
    encode_bytes_field(field_number, s.as_bytes(), buf);
}

/// Frames a TracePacket into a Perfetto trace file chunk.
/// The outer container is `perfetto.protos.Trace`, whose only field is
/// `repeated TracePacket packet = 1;` (wire type 2).
#[inline]
pub fn frame_packet(packet_bytes: &[u8], out: &mut Vec<u8>) {
    encode_bytes_field(1, packet_bytes, out);
}
