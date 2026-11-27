//! The socket options that tell more about a connection's peer than
//! `SO_PEERCRED` does: `SO_PEERGROUPS`, `SO_PEERSEC` and `SO_PEERPIDFD`.
//! `rustix` 1.1 has no accessor for them, so this module calls
//! `getsockopt` through `libc`. It holds the only `unsafe` code of the
//! crate outside its tests.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// How often a variable-length option is read again with the size the
/// kernel asked for. The peer's groups and label are fixed when the
/// connection is made, so the second read fits; the bound only keeps a
/// kernel that never agrees from looping.
const SIZE_RETRIES: usize = 4;

// libc (0.2.191) defines `SO_PEERGROUPS` and `SO_PEERPIDFD` for Linux but
// not for Android, whose kernel defines them from Linux 4.13 and 6.5
// (older kernels answer `ENOPROTOOPT`). Every
// Android architecture takes them from the generic `asm-generic/socket.h`
// (59 and 77); sparc numbers them differently (0x3d and 0x56) and is no
// Android target, so any other architecture stops the build here. Use
// libc's constants once it defines them for Android.
#[cfg(all(
    target_os = "android",
    not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64"
    ))
))]
compile_error!("SO_PEERGROUPS and SO_PEERPIDFD are not known for this Android architecture");

#[cfg(target_os = "android")]
const SO_PEERGROUPS: libc::c_int = 59;
#[cfg(not(target_os = "android"))]
const SO_PEERGROUPS: libc::c_int = libc::SO_PEERGROUPS;

#[cfg(target_os = "android")]
const SO_PEERPIDFD: libc::c_int = 77;
#[cfg(not(target_os = "android"))]
const SO_PEERPIDFD: libc::c_int = libc::SO_PEERPIDFD;

/// An element type the kernel may fill byte by byte.
///
/// # Safety
///
/// Every bit pattern is a valid value of the type, and it has no padding.
unsafe trait Plain: Copy + Default {}

// SAFETY: integers: every bit pattern is a value, and there is no padding.
unsafe impl Plain for u8 {}
// SAFETY: as above. `gid_t` on Linux.
unsafe impl Plain for u32 {}
// SAFETY: as above. `c_int` on Linux.
unsafe impl Plain for i32 {}

/// `getsockopt(fd, SOL_SOCKET, option)` into `buf`.
///
/// `Ok(bytes)`: the option's value takes the first `bytes` bytes of `buf`.
/// `Err((e, bytes))`: the call failed, and the kernel left `bytes` in the
/// length argument: for `ERANGE`, the size the value needs.
fn read_option<T: Plain>(
    fd: BorrowedFd<'_>,
    option: libc::c_int,
    buf: &mut [T],
) -> Result<usize, (io::Error, usize)> {
    let size = std::mem::size_of_val(buf);
    let mut len = libc::socklen_t::try_from(size).unwrap_or(libc::socklen_t::MAX);
    // SAFETY: `buf` is valid for writes of `size` bytes and `len` is not
    // larger than `size`, so the kernel, which writes at most `len` bytes
    // to the value pointer, stays inside `buf`. `&mut len` is valid for a
    // read and a write of a `socklen_t`. Whatever bytes the kernel writes
    // are valid values of `T` (`Plain`), and the elements it does not
    // write keep the values the caller initialized them with. `fd` is open
    // for the call (`BorrowedFd`).
    let rc = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            option,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
        )
    };
    let len = len as usize;
    if rc == 0 {
        // A kernel that reported more than it was given would be a kernel
        // bug; the caller indexes `buf` with this.
        Ok(len.min(size))
    } else {
        Err((io::Error::last_os_error(), len))
    }
}

/// A variable-length option as elements of `T`, read again with the size
/// the kernel names when the first buffer is too small (`ERANGE`).
fn read_sized<T: Plain>(
    fd: BorrowedFd<'_>,
    option: libc::c_int,
    first_len: usize,
) -> io::Result<Vec<T>> {
    let element = std::mem::size_of::<T>();
    let mut buf = vec![T::default(); first_len];
    let mut last = None;
    for _ in 0..SIZE_RETRIES {
        match read_option(fd, option, &mut buf) {
            Ok(bytes) => {
                buf.truncate(bytes / element);
                return Ok(buf);
            }
            Err((e, needed))
                if e.raw_os_error() == Some(libc::ERANGE)
                    && needed > std::mem::size_of_val(buf.as_slice()) =>
            {
                buf = vec![T::default(); needed.div_ceil(element)];
                last = Some(e);
            }
            Err((e, _)) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::from_raw_os_error(libc::ERANGE)))
}

/// `SO_PEERGROUPS` (Linux 4.13): the supplementary groups the peer had
/// when it called `connect()`.
pub(crate) fn peer_groups(fd: BorrowedFd<'_>) -> io::Result<Vec<u32>> {
    // 32 is `NGROUPS_SMALL`, enough for most processes without a second
    // call.
    read_sized::<libc::gid_t>(fd, SO_PEERGROUPS, 32)
}

/// `SO_PEERSEC`: the peer's security label in the format of the LSM that
/// labels the socket, without the NUL bytes at its end (the length the
/// kernel returns may count a terminating NUL or not, `SO_PEERSEC(2const)`).
/// `ENOPROTOOPT` when no LSM of the kernel labels sockets.
pub(crate) fn peer_security_label(fd: BorrowedFd<'_>) -> io::Result<Vec<u8>> {
    let mut label = read_sized::<u8>(fd, libc::SO_PEERSEC, 256)?;
    while label.last() == Some(&0) {
        label.pop();
    }
    Ok(label)
}

/// `SO_PEERPIDFD` (Linux 6.5): a new pidfd, close-on-exec, for the process
/// that called `connect()`.
pub(crate) fn peer_pidfd(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let mut pidfd: [libc::c_int; 1] = [-1];
    let bytes = read_option(fd, SO_PEERPIDFD, &mut pidfd).map_err(|(e, _)| e)?;
    if bytes != std::mem::size_of::<libc::c_int>() || pidfd[0] < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SO_PEERPIDFD returned no descriptor",
        ));
    }
    // SAFETY: the call succeeded and wrote a whole `int`, which for
    // `SO_PEERPIDFD` is a descriptor the kernel installed for this call.
    // Nothing else knows its number, so this is its only owner.
    Ok(unsafe { OwnedFd::from_raw_fd(pidfd[0]) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    /// `id -G` of this process, sorted.
    fn own_groups() -> Vec<u32> {
        let mut groups: Vec<u32> = rustix::process::getgroups()
            .unwrap()
            .into_iter()
            .map(|gid| gid.as_raw())
            .collect();
        groups.sort_unstable();
        groups
    }

    #[test]
    fn peer_groups_are_the_connecting_process_groups() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut groups = peer_groups(a.as_fd()).unwrap();
        groups.sort_unstable();
        assert_eq!(groups, own_groups());
    }

    /// The second read, with the size the kernel named, returns the same
    /// groups as a first read that fits.
    #[test]
    fn a_small_first_buffer_is_read_again() {
        let (a, _b) = UnixStream::pair().unwrap();
        let expected = own_groups();
        if expected.is_empty() {
            eprintln!("skipped: this process has no supplementary group");
            return;
        }
        let mut groups = read_sized::<libc::gid_t>(a.as_fd(), SO_PEERGROUPS, 0).unwrap();
        groups.sort_unstable();
        assert_eq!(groups, expected);
    }

    #[test]
    fn peer_pidfd_names_the_connecting_process() {
        let (a, _b) = UnixStream::pair().unwrap();
        let pidfd = match peer_pidfd(a.as_fd()) {
            Ok(pidfd) => pidfd,
            // Linux before 6.5.
            Err(e) if e.raw_os_error() == Some(libc::ENOPROTOOPT) => {
                eprintln!("skipped: the kernel has no SO_PEERPIDFD");
                return;
            }
            Err(e) => panic!("SO_PEERPIDFD: {e}"),
        };
        let flags = rustix::io::fcntl_getfd(&pidfd).unwrap();
        assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        // `/proc/self/fdinfo/<fd>` of a pidfd has a `Pid:` line.
        let info =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd())).unwrap();
        let pid = info
            .lines()
            .find_map(|line| line.strip_prefix("Pid:"))
            .expect("a Pid: line")
            .trim()
            .parse::<u32>()
            .unwrap();
        assert_eq!(pid, std::process::id());
    }

    /// With an LSM that labels sockets the label has no NUL; without one
    /// the kernel answers `ENOPROTOOPT`.
    #[test]
    fn peer_security_label_or_no_lsm() {
        let (a, _b) = UnixStream::pair().unwrap();
        match peer_security_label(a.as_fd()) {
            Ok(label) => assert!(!label.contains(&0), "{label:?}"),
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOPROTOOPT), "{e}"),
        }
    }

    #[test]
    fn a_descriptor_that_is_no_socket_fails() {
        let file = std::fs::File::open("/dev/null").unwrap();
        let e = peer_groups(file.as_fd()).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::ENOTSOCK));
    }
}
