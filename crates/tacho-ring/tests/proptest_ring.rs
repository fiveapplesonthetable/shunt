#![cfg(not(loom))]

use proptest::prelude::*;
use tacho_ring::{Ring, CHUNK_PAYLOAD_CAPACITY, CHUNK_SIZE};

/// Frame a packet with a 4-byte little-endian length prefix.
fn frame_packet(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    framed.extend_from_slice(payload);
    framed
}

/// Unframe a chunk payload into individual packet payloads.
fn unframe_chunk(chunk_payload: &[u8]) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut offset = 0;
    while offset + 4 <= chunk_payload.len() {
        let len =
            u32::from_le_bytes(chunk_payload[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        assert!(
            offset + len <= chunk_payload.len(),
            "Framed packet length {len} exceeds remaining chunk bytes at offset {offset}"
        );
        packets.push(chunk_payload[offset..offset + len].to_vec());
        offset += len;
    }
    assert_eq!(
        offset,
        chunk_payload.len(),
        "Trailing unmatched bytes in chunk payload"
    );
    packets
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn test_random_packet_sequences(
        num_chunks in 2usize..=8,
        // Generates 1..=150 packets, payload lengths 1..=(CHUNK_PAYLOAD_CAPACITY - 4)
        payloads in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..=(CHUNK_PAYLOAD_CAPACITY - 4)),
            1..=150
        )
    ) {
        let ring_bytes = num_chunks * CHUNK_SIZE;
        let mut ring = Ring::new(ring_bytes);

        // Frame all packets
        let framed_packets: Vec<Vec<u8>> = payloads.iter().map(|p| frame_packet(p)).collect();

        // Simulate expected chunking according to the contract
        let mut simulated_chunks: Vec<Vec<Vec<u8>>> = Vec::new();
        let mut current_chunk: Vec<Vec<u8>> = Vec::new();
        let mut current_chunk_len = 0;

        for (i, framed) in framed_packets.iter().enumerate() {
            if current_chunk_len + framed.len() <= CHUNK_PAYLOAD_CAPACITY {
                current_chunk.push(payloads[i].clone());
                current_chunk_len += framed.len();
            } else {
                if !current_chunk.is_empty() {
                    simulated_chunks.push(current_chunk);
                }
                current_chunk = vec![payloads[i].clone()];
                current_chunk_len = framed.len();
            }
        }
        if !current_chunk.is_empty() {
            simulated_chunks.push(current_chunk);
        }

        // Write all framed packets into the ring
        for framed in &framed_packets {
            ring.write(framed).expect("Packet framed <= CHUNK_PAYLOAD_CAPACITY must fit");
        }

        // Expected chunks retained in the ring
        let chunks_lost = simulated_chunks.len().saturating_sub(num_chunks);
        let expected_chunks = &simulated_chunks[chunks_lost..];

        // Snapshot the ring
        let mut snapshot_chunks = Vec::new();
        let stats = ring.snapshot(&mut |chunk_payload| {
            snapshot_chunks.push(unframe_chunk(chunk_payload));
        });

        // 1. Snapshot output equals exactly the newest packets that fit in whole chunks, in order
        prop_assert_eq!(&snapshot_chunks, expected_chunks);

        // 2. Overwritten equals chunks lost
        prop_assert_eq!(stats.overwritten, chunks_lost as u64);

        // 3. Chunks count matches
        prop_assert_eq!(stats.chunks, expected_chunks.len());
        prop_assert_eq!(stats.chunks_skipped, 0);

        // 4. Bytes match total payload bytes
        let expected_bytes: usize = framed_packets
            .iter()
            .skip(payloads.len() - expected_chunks.iter().map(|c| c.len()).sum::<usize>())
            .map(|f| f.len())
            .sum();
        prop_assert_eq!(stats.bytes, expected_bytes);
    }
}
