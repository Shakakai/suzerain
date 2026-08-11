//! Peer-credential checks for operator unix sockets (G6): only processes
//! running as the same local user may issue commands. Linux uses
//! `SO_PEERCRED`; macOS uses `LOCAL_PEERCRED`.

use std::io;
use std::os::unix::io::AsRawFd;

/// The peer process's effective uid, from the socket.
#[cfg(target_os = "linux")]
pub fn peer_uid<S: AsRawFd>(stream: &S) -> io::Result<u32> {
    #[repr(C)]
    struct Ucred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    const SO_PEERCRED: i32 = 17;
    let mut cred = Ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<Ucred>() as u32;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_PEERCRED,
            &mut cred as *mut Ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

#[cfg(target_os = "macos")]
pub fn peer_uid<S: AsRawFd>(stream: &S) -> io::Result<u32> {
    #[repr(C)]
    struct Xucred {
        version: u32,
        uid: u32,
        ngroups: i16,
        groups: [u32; 16],
    }
    const LOCAL_PEERCRED: i32 = 0x001;
    let mut cred = Xucred {
        version: 0,
        uid: 0,
        ngroups: 0,
        groups: [0; 16],
    };
    let mut len = std::mem::size_of::<Xucred>() as u32;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0, // SOL_LOCAL
            LOCAL_PEERCRED,
            &mut cred as *mut Xucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// True if the connecting peer runs as the same effective uid as us.
pub fn same_user<S: AsRawFd>(stream: &S) -> bool {
    match peer_uid(stream) {
        Ok(uid) => uid == unsafe { libc::geteuid() },
        Err(_) => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn peer_uid_matches_our_euid_on_local_sockets() {
        let dir = std::env::temp_dir().join(format!("peercred-test-{}", std::process::id()));
        let sock = dir.join("t.sock");
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let client = tokio::net::UnixStream::connect(&sock);
        let (accepted, connected) = tokio::join!(listener.accept(), client);
        let (stream, _) = accepted.unwrap();
        let _client = connected.unwrap();

        let ours = unsafe { libc::geteuid() };
        assert_eq!(peer_uid(&stream).unwrap(), ours);
        assert!(same_user(&stream));
    }
}
