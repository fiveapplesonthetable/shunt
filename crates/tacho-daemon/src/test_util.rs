//! Event-driven helpers shared by unit tests (no sleeps, no deadlines).

use std::path::Path;

/// Blocks (no timeout) until `path` exists, waking on inotify events in its directory.
///
/// Writers must create the file atomically (write a temp file, then `mv`), so existence
/// implies complete content.
pub fn wait_for_file(path: &Path) {
    let dir = path.parent().expect("path has a parent");
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        panic!(
            "cannot wait for {}: inotify_init1 failed ({err}); the machine is out of inotify \
             instances (raise fs.inotify.max_user_instances)",
            path.display()
        );
    }
    let dir_c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes()).unwrap();
    let wd = unsafe {
        libc::inotify_add_watch(
            fd,
            dir_c.as_ptr(),
            libc::IN_CREATE | libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO,
        )
    };
    if wd < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        panic!(
            "cannot wait for {}: inotify_add_watch({}) failed ({err}); the directory must \
             exist (raise fs.inotify.max_user_watches if ENOSPC)",
            path.display(),
            dir.display()
        );
    }
    // Checked after the watch is armed, so a creation in between cannot be missed.
    while !path.exists() {
        let mut buf = [0u8; 4096];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            panic!(
                "inotify read failed while waiting for {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
    }
    unsafe { libc::close(fd) };
}
