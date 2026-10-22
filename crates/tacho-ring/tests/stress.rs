#![cfg(not(loom))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use tacho_ring::Ring;

const TOTAL_PACKETS: u64 = 1_000_000;
const PAYLOAD_LEN: usize = 32;
const RING_BYTES: usize = 16 * 1024 * 1024; // 16 MiB = 4096 chunks

fn compute_checksum(seq: u64, payload: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for b in seq.to_le_bytes() {
        hash = (hash ^ (b as u32)).wrapping_mul(0x01000193);
    }
    for &b in payload {
        hash = (hash ^ (b as u32)).wrapping_mul(0x01000193);
    }
    hash
}

fn make_packet(seq: u64, buf: &mut [u8; 4 + 8 + 4 + PAYLOAD_LEN]) {
    let total_len = (8 + 4 + PAYLOAD_LEN) as u32;
    buf[0..4].copy_from_slice(&total_len.to_le_bytes());
    buf[4..12].copy_from_slice(&seq.to_le_bytes());

    // Generate payload pattern based on seq
    let mut payload = [0u8; PAYLOAD_LEN];
    for (i, b) in payload.iter_mut().enumerate() {
        *b = (seq as u8).wrapping_add(i as u8);
    }

    let checksum = compute_checksum(seq, &payload);
    buf[12..16].copy_from_slice(&checksum.to_le_bytes());
    buf[16..16 + PAYLOAD_LEN].copy_from_slice(&payload);
}

#[test]
fn test_stress_one_writer_four_readers() {
    let ring = Ring::new(RING_BYTES);
    let (mut writer, reader) = ring.into_parts();

    let done = Arc::new(AtomicBool::new(false));

    // Spawn 4 reader threads
    let mut reader_handles = Vec::new();
    for _reader_id in 0..4 {
        let r = reader.clone();
        let d = Arc::clone(&done);

        let handle = thread::spawn(move || {
            let mut snapshots_taken = 0usize;

            while !d.load(Ordering::Acquire) {
                let mut chunk_packet_seqs: Vec<Vec<u64>> = Vec::new();

                let stats = r.snapshot(&mut |chunk_payload| {
                    let mut seqs_in_chunk = Vec::new();
                    let mut offset = 0;

                    while offset + 4 <= chunk_payload.len() {
                        let total_len = u32::from_le_bytes(
                            chunk_payload[offset..offset + 4].try_into().unwrap(),
                        ) as usize;
                        offset += 4;

                        assert!(
                            offset + total_len <= chunk_payload.len(),
                            "Packet length {total_len} exceeds chunk payload boundary"
                        );

                        let packet_bytes = &chunk_payload[offset..offset + total_len];
                        assert!(
                            packet_bytes.len() >= 12,
                            "Packet too small for seq and checksum"
                        );

                        let seq = u64::from_le_bytes(packet_bytes[0..8].try_into().unwrap());
                        let checksum = u32::from_le_bytes(packet_bytes[8..12].try_into().unwrap());
                        let payload = &packet_bytes[12..];

                        // Validate checksum: packet must be whole, never torn
                        let expected_checksum = compute_checksum(seq, payload);
                        assert_eq!(
                            checksum, expected_checksum,
                            "Torn packet detected at seq {seq}!"
                        );

                        seqs_in_chunk.push(seq);
                        offset += total_len;
                    }

                    assert_eq!(
                        offset,
                        chunk_payload.len(),
                        "Unprocessed trailing bytes in chunk"
                    );

                    if !seqs_in_chunk.is_empty() {
                        chunk_packet_seqs.push(seqs_in_chunk);
                    }
                });

                // Verify snapshot properties:
                // 1. Within each chunk, seqs are strictly contiguous (seq[k+1] == seq[k] + 1)
                // 2. Seqs are strictly increasing across all emitted chunks
                // 3. Across chunks, gap in seq can only occur if chunks_skipped > 0
                let mut last_seq: Option<u64> = None;
                for chunk_seqs in &chunk_packet_seqs {
                    let first_seq = chunk_seqs[0];
                    if let Some(prev) = last_seq {
                        assert!(
                            first_seq > prev,
                            "Seqs must strictly increase: prev {prev}, next {first_seq}"
                        );
                        if first_seq != prev + 1 {
                            assert!(
                                stats.chunks_skipped > 0,
                                "Non-contiguous seq jump from {prev} to {first_seq} without reported chunks_skipped (skipped: {})",
                                stats.chunks_skipped
                            );
                        }
                    }

                    for i in 0..chunk_seqs.len() - 1 {
                        assert_eq!(
                            chunk_seqs[i + 1],
                            chunk_seqs[i] + 1,
                            "Packets within chunk must be strictly contiguous"
                        );
                    }

                    last_seq = Some(*chunk_seqs.last().unwrap());
                }

                snapshots_taken += 1;
            }

            snapshots_taken
        });

        reader_handles.push(handle);
    }

    // Writer writes 1,000,000 packets
    let mut packet_buf = [0u8; 4 + 8 + 4 + PAYLOAD_LEN];
    for seq in 0..TOTAL_PACKETS {
        make_packet(seq, &mut packet_buf);
        writer.write(&packet_buf).expect("write failed");
    }

    // Signal done to readers and join
    done.store(true, Ordering::Release);

    let total_snapshots: usize = reader_handles
        .into_iter()
        .map(|h| h.join().expect("reader panicked"))
        .sum();

    println!("Stress test completed successfully: 1,000,000 writes, {total_snapshots} total snapshots verified across 4 readers.");
}
