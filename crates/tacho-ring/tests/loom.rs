#![cfg(loom)]

use tacho_ring::{Ring, CHUNK_SIZE};

#[test]
fn loom_test() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(2);

    builder.check(|| {
        let (mut writer, reader) = Ring::new(CHUNK_SIZE * 2).into_parts();

        let jh = loom::thread::spawn(move || {
            let p1 = [1u8; 8];
            let p2 = [2u8; 8];
            let _ = writer.write(&p1);
            let _ = writer.write(&p2);

            // 16 + 24 = 40 > 32: moves to chunk 1
            let p3 = [3u8; 24];
            let _ = writer.write(&p3);

            // 24 + 16 = 40 > 32: wraps to chunk 0
            let p4 = [4u8; 16];
            let _ = writer.write(&p4);
        });

        let mut emitted_chunks = Vec::new();
        let stats = reader.snapshot(&mut |chunk| {
            emitted_chunks.push(chunk.to_vec());
        });

        // Verify that every emitted chunk is whole, consistent, and never torn
        for chunk in &emitted_chunks {
            match chunk.len() {
                8 => {
                    // Whole packet 1
                    assert_eq!(chunk.as_slice(), &[1u8; 8]);
                }
                16 => {
                    // Either [p1, p2] or [p4]
                    let is_p1_p2 = chunk[..8] == [1u8; 8] && chunk[8..] == [2u8; 8];
                    let is_p4 = chunk.as_slice() == &[4u8; 16];
                    assert!(
                        is_p1_p2 || is_p4,
                        "16-byte chunk must be [p1, p2] or [p4], got: {chunk:?}"
                    );
                }
                24 => {
                    // Whole packet 3
                    assert_eq!(chunk.as_slice(), &[3u8; 24]);
                }
                other => panic!("Unexpected chunk payload length {other}: {chunk:?}"),
            }
        }

        // Stats invariants: bytes equals sum of emitted chunk lengths
        let total_bytes: usize = emitted_chunks.iter().map(|c| c.len()).sum();
        assert_eq!(stats.bytes, total_bytes);
        assert_eq!(stats.chunks, emitted_chunks.len());

        jh.join().unwrap();
    });
}
