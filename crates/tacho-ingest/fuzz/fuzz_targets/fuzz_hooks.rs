#![no_main]
use libfuzzer_sys::fuzz_target;
use tacho_ingest::hooks::HookEnv;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
            let env = HookEnv::new(1000);
            let _ = tacho_ingest::hooks::claude::map(&val, &env);
            let _ = tacho_ingest::hooks::gemini::map(&val, &env);
            let _ = tacho_ingest::hooks::jetski::map(&val, &env);
        }
    }
});
