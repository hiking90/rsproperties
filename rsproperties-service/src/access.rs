//! Who may connect to the service's sockets: [`Listener`] names a socket,
//! [`SocketAccess`] is the owner group and mode of its file.

use rsproperties::{PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME, PROPERTY_SERVICE_SOCKET_NAME};

/// One of the two sockets the service listens on. Both take the same
/// requests; init gives them different file modes, and so can a host with
/// [`ServiceOptions::socket_access`](crate::ServiceOptions::socket_access).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Listener {
    /// `property_service`, which every set goes to except as below. init
    /// creates it 0666 and leaves the check of each set to SELinux.
    Property,
    /// `property_service_for_system`. init creates it 0660 with group
    /// `system`. bionic and the `rsproperties` client send `sys.powerctl`
    /// to it, and to `property_service` when it cannot be connected to.
    System,
}

impl Listener {
    /// Both listeners, in the order the service creates their sockets.
    pub(crate) const ALL: [Self; 2] = [Self::Property, Self::System];

    /// The socket's file name in the socket dir.
    pub const fn socket_name(self) -> &'static str {
        match self {
            Self::Property => PROPERTY_SERVICE_SOCKET_NAME,
            Self::System => PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME,
        }
    }

    /// `"property"` or `"system"`, as the accept loop's log lines name
    /// the listener.
    pub(crate) const fn short_name(self) -> &'static str {
        match self {
            Self::Property => "property",
            Self::System => "system",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Property => 0,
            Self::System => 1,
        }
    }
}

impl std::fmt::Display for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.socket_name())
    }
}

/// The owner group and mode of one socket file. A peer can `connect()`
/// only with write permission on the file, so these decide which uids
/// reach the service through that socket.
///
/// The default is mode `0o660` and the group the file gets from the
/// service's process (its fsgid, or the socket dir's group when the dir
/// has the setgid bit).
///
/// ```
/// use rsproperties_service::SocketAccess;
///
/// // rw for the service's user and for gid 1000, nothing for others.
/// let access = SocketAccess::new(0o660).with_group(1000);
/// assert_eq!(access.mode(), 0o660);
/// assert_eq!(access.group(), Some(1000));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SocketAccess {
    mode: u32,
    group: Option<u32>,
}

impl SocketAccess {
    /// The file gets `mode` and keeps the group it is created with.
    ///
    /// [`run`](crate::run) fails at
    /// [`StartupStage::Config`](crate::StartupStage::Config) when `mode`
    /// has a bit outside `0o666`: the setuid, setgid, sticky and execute
    /// bits have no meaning on a socket. It fails there too for a bit for
    /// others (`0o006`) unless the options have an
    /// [authorizer](crate::ServiceOptions::authorizer): without one, every
    /// local uid that can reach the socket dir could set any property.
    pub const fn new(mode: u32) -> Self {
        Self { mode, group: None }
    }

    /// The file's group is changed to `gid` before the socket listens.
    ///
    /// A numeric gid: the service does not look up group names. `u32::MAX`
    /// (`(gid_t)-1`) is refused at
    /// [`StartupStage::Config`](crate::StartupStage::Config). Without
    /// `CAP_CHOWN` the service's user must be a member of `gid`; otherwise
    /// [`run`](crate::run) fails at
    /// [`StartupStage::SocketService`](crate::StartupStage::SocketService)
    /// with `EPERM`.
    pub const fn with_group(self, gid: u32) -> Self {
        Self {
            group: Some(gid),
            ..self
        }
    }

    /// The file mode, without the file type bits.
    pub const fn mode(&self) -> u32 {
        self.mode
    }

    /// The gid the file's group is changed to, or `None` to keep it.
    pub const fn group(&self) -> Option<u32> {
        self.group
    }

    /// Why [`run`](crate::run) refuses this access for `listener`.
    /// `has_authorizer`: whether the options have an
    /// [authorizer](crate::ServiceOptions::authorizer), which a bit for
    /// others needs.
    pub(crate) fn check(
        &self,
        listener: Listener,
        has_authorizer: bool,
    ) -> Result<(), rsproperties::Error> {
        let allowed: u32 = if has_authorizer { 0o666 } else { 0o660 };
        if self.group == Some(u32::MAX) {
            return Err(rsproperties::Error::InvalidArgument(format!(
                "socket group {} for {listener} is (gid_t)-1, which chown reads as \
                 \"leave the group unchanged\"",
                u32::MAX
            )));
        }
        if self.mode & !allowed == 0 {
            return Ok(());
        }
        let reason = if self.mode & !0o777 != 0 {
            "has a bit above 0o777 (setuid, setgid, sticky)"
        } else if self.mode & 0o111 != 0 {
            "has an execute bit"
        } else {
            "has a bit for others, which needs ServiceOptions::authorizer (without one, every \
             local uid that can connect could set any property)"
        };
        Err(rsproperties::Error::InvalidArgument(format!(
            "socket mode {:#o} for {listener} {reason}; the mode must be within {allowed:#o}",
            self.mode
        )))
    }
}

impl Default for SocketAccess {
    /// Mode `0o660`, group unchanged.
    fn default() -> Self {
        Self::new(crate::socket_service::SOCKET_FILE_MODE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_outside_the_allowed_bits_are_refused() {
        for mode in [0o660, 0o600, 0o640, 0o620, 0o000] {
            for has_authorizer in [false, true] {
                SocketAccess::new(mode)
                    .check(Listener::Property, has_authorizer)
                    .unwrap();
            }
        }
        // `Some(bound)`: refused with and without an authorizer, and the
        // message names this bound for the service with one.
        for (mode, reason, with_authorizer) in [
            (0o666, "for others", None),
            (0o662, "for others", None),
            (0o664, "for others", None),
            (0o670, "execute", Some("0o666")),
            (0o760, "execute", Some("0o666")),
            (0o667, "execute", Some("0o666")),
            (0o2660, "above 0o777", Some("0o666")),
            (0o4666, "above 0o777", Some("0o666")),
            (0o1660, "above 0o777", Some("0o666")),
            (0o10660, "above 0o777", Some("0o666")),
        ] {
            let err = SocketAccess::new(mode)
                .check(Listener::System, false)
                .unwrap_err()
                .to_string();
            assert!(err.contains(reason), "{mode:#o}: {err}");
            assert!(err.contains("within 0o660"), "{mode:#o}: {err}");
            assert!(
                err.contains(PROPERTY_SERVICE_FOR_SYSTEM_SOCKET_NAME),
                "{err}"
            );
            let checked = SocketAccess::new(mode).check(Listener::System, true);
            match with_authorizer {
                None => checked.unwrap(),
                Some(bound) => {
                    let err = checked.unwrap_err().to_string();
                    assert!(err.contains(reason), "{mode:#o}: {err}");
                    assert!(err.contains(bound), "{mode:#o}: {err}");
                }
            }
        }
    }

    #[test]
    fn group_minus_one_is_refused() {
        SocketAccess::new(0o660)
            .with_group(0)
            .check(Listener::Property, false)
            .unwrap();
        SocketAccess::new(0o660)
            .with_group(u32::MAX - 1)
            .check(Listener::Property, false)
            .unwrap();
        for has_authorizer in [false, true] {
            let err = SocketAccess::new(0o660)
                .with_group(u32::MAX)
                .check(Listener::Property, has_authorizer)
                .unwrap_err()
                .to_string();
            assert!(err.contains("unchanged"), "{err}");
        }
    }
}
