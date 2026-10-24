//! Event-driven helpers shared by unit tests (no sleeps, no deadlines).

use std::path::Path;

/// Blocks (no timeout) until `path` exists, waking on inotify events in its directory.
///
/// Writers must create the file atomically (write a temp file, then `mv`), so existence
/// implies complete content.
pub fn wait_for_file(path: &Path) {
    let dir = path.parent().expect("path has a parent");
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    assert!(fd >= 0, "inotify_init1 failed");
    let dir_c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            dir_c.as_ptr(),
            libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        )
    };
    assert!(wd >= 0, "inotify_add_watch failed");
    // Checked after the watch is armed, so a creation in between cannot be missed.
    while !path.exists() {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            panic!("inotify read failed: {}", std::io::Error::last_os_error());
        }
    }
    unsafe { libc::close(fd) };
}
