//! Who may connect to the service's sockets: [`Listener`] names a socket,
//! [`SocketAccess`] is the owner group, mode and SELinux label of its file.

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

/// The owner group, mode and SELinux label of one socket file. A peer can
/// `connect()` only with write permission on the file, and under SELinux
/// only when its domain may write the file's type, so these decide which
/// processes reach the service through that socket.
///
/// SELinux also checks `unix_stream_socket connectto` from the peer's
/// domain to the listening socket, which has the service's domain (the
/// service does not set a sockcreate context). The label does not change
/// that check. AOSP's `set_prop` macro (`te_macros`) grants `connectto`
/// on `init` only, so a peer whose policy was written for init connects
/// only when the policy grants the same on the service's domain.
///
/// The default is mode `0o660`, the group the file gets from the
/// service's process (its fsgid, or the socket dir's group when the dir
/// has the setgid bit), and the label the policy gives a new file in the
/// socket dir.
///
/// ```
/// use rsproperties_service::SocketAccess;
///
/// // rw for the service's user and for gid 1000, nothing for others.
/// let access = SocketAccess::new(0o660).with_group(1000);
/// assert_eq!(access.mode(), 0o660);
/// assert_eq!(access.group(), Some(1000));
/// assert_eq!(access.label(), None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketAccess {
    mode: u32,
    group: Option<u32>,
    label: Option<String>,
}

impl SocketAccess {
    /// The file gets `mode` and keeps the group and label it is created
    /// with.
    ///
    /// [`run`](crate::run) fails at
    /// [`StartupStage::Config`](crate::StartupStage::Config) when `mode`
    /// has a bit outside `0o666`: the setuid, setgid, sticky and execute
    /// bits have no meaning on a socket. It fails there too for a bit for
    /// others (`0o006`) unless the options have an
    /// [authorizer](crate::ServiceOptions::authorizer): without one, every
    /// local uid that can reach the socket dir could set any property.
    pub const fn new(mode: u32) -> Self {
        Self {
            mode,
            group: None,
            label: None,
        }
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
    pub fn with_group(self, gid: u32) -> Self {
        Self {
            group: Some(gid),
            ..self
        }
    }

    /// The file is created with the SELinux context `label`, as init
    /// creates its sockets with the context `file_contexts` gives them
    /// (`u:object_r:property_socket:s0` for both in AOSP).
    ///
    /// The service writes `label` to `/proc/thread-self/attr/fscreate`
    /// and binds the socket in a thread it starts for that `bind` alone:
    /// the attribute belongs to the thread and ends with it, so no other
    /// file the process creates gets the label. The file has the label
    /// from the moment its path exists, before any peer can `connect()`.
    ///
    /// The kernel ignores the fscreate attribute on a filesystem mounted
    /// with `context=` (the file gets the mount's context) and on one
    /// without per-file labels (the file gets the label the policy gives
    /// it), and neither the write nor the `bind` fails there: `run`
    /// succeeds with the socket under another label. AOSP init does not
    /// check this either.
    ///
    /// [`run`](crate::run) fails at
    /// [`StartupStage::Config`](crate::StartupStage::Config) for an empty
    /// `label` or one with whitespace or a control byte (NUL included),
    /// which no SELinux context has: the kernel reads an empty value or
    /// one that starts with a newline as "no fscreate context" and would
    /// create the file under the default label. It fails at
    /// [`StartupStage::SocketService`](crate::StartupStage::SocketService)
    /// when the kernel refuses the label, at the write to the fscreate
    /// attribute or at the `bind` that creates the file under it. The
    /// errno, and the conditions under which the kernel refuses, depend on
    /// the security module and its policy. For example, with SELinux off
    /// and no other security module that takes the attribute the write
    /// fails with `EINVAL`, and under a policy that does not let the
    /// service's domain create a `sock_file` of the label's type the `bind`
    /// fails with `EACCES`. Whether a context the policy does not define
    /// is refused depends on the policy too: SELinux may accept it from a
    /// service with `CAP_MAC_ADMIN` in a permissive domain, under
    /// permissive mode, or in a domain granted `capability2 mac_admin`.
    ///
    /// After `bind` the service uses the file under the label's type, so
    /// the policy must also grant its domain `sock_file getattr`,
    /// `setattr` (when the group or mode is changed), `rename`, and
    /// `unlink` (to replace the file a previous run left and to remove it
    /// at shutdown) on that type. Without `getattr` [`run`](crate::run)
    /// fails at `StartupStage::SocketService` with `EACCES`; a refused
    /// `unlink` at shutdown leaves the file behind with a warning.
    ///
    /// ```
    /// use rsproperties_service::SocketAccess;
    ///
    /// // init's `property_service`.
    /// let access = SocketAccess::new(0o666).with_label("u:object_r:property_socket:s0");
    /// assert_eq!(access.label(), Some("u:object_r:property_socket:s0"));
    /// ```
    pub fn with_label(self, label: impl Into<String>) -> Self {
        Self {
            label: Some(label.into()),
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

    /// The SELinux context the file is created with, or `None` for the
    /// one the policy gives it.
    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
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
        if let Some(label) = &self.label {
            // The kernel reads an empty value or one that starts with a
            // newline as "reset fscreate" (`selinux_lsm_setattr`).
            check_label(label, &format!("socket label for {listener}"))?;
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

/// Refuses a `label` no SELinux context can be: empty, or with whitespace
/// or a control byte. `what` names the option in the message.
pub(crate) fn check_label(label: &str, what: &str) -> Result<(), rsproperties::Error> {
    if label.is_empty()
        || label
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(rsproperties::Error::InvalidArgument(format!(
            "{what} {label:?} is empty or has whitespace or a control byte, which no SELinux \
             context has"
        )));
    }
    Ok(())
}

impl Default for SocketAccess {
    /// Mode `0o660`, group and label unchanged.
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
    fn empty_label_and_label_with_whitespace_or_control_byte_are_refused() {
        for label in [
            "u:object_r:property_socket:s0",
            "system_u:object_r:var_run_t:s0-s15:c0.c1023",
        ] {
            SocketAccess::new(0o660)
                .with_label(label)
                .check(Listener::Property, false)
                .unwrap();
        }
        // A leading newline makes the kernel reset fscreate instead of
        // refusing the write.
        for label in [
            "",
            "u:object_r:a\0b:s0",
            "\n",
            "\nu:object_r:property_socket:s0",
            "u:object_r:property_socket:s0\n",
            " u:object_r:property_socket:s0",
            "u:object_r:a b:s0",
            "u:object_r:a\tb:s0",
            "u:object_r:a\x7fb:s0",
        ] {
            let err = SocketAccess::new(0o660)
                .with_label(label)
                .check(Listener::Property, true)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("empty or has whitespace or a control byte"),
                "{label:?}: {err}"
            );
            assert!(err.contains(PROPERTY_SERVICE_SOCKET_NAME), "{err}");
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
