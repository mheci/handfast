//! Windows named-pipe transport for the local control channel.
//!
//! The pipe name carries the current-user SID. Every pipe instance gets a
//! protected DACL with one access-allowed ACE that grants `FILE_ALL_ACCESS` to
//! that SID, so another local user cannot open the control channel. Every
//! `windows-sys` call lives in this module; the `unsafe_code` allow sits on the
//! functions that call FFI.
//!
//! shortcut: the DACL rejects a second local user, but that check needs a
//! second local user, so it stays manual. The Windows test
//! `pipe_security_descriptor_is_one_current_user_ace` proves the descriptor
//! shape. The Windows job builds this module.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{LocalFree, ERROR_PIPE_BUSY};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

use handfast_desktop::winacl::current_user_sid_string;

use super::{serve, ControlError, ControlServerHandle, DaemonCommand, CONTROL_IO_TIMEOUT};

/// Named-pipe path prefix. The rest of the name is the current-user SID.
pub(super) const PIPE_PREFIX: &str = r"\\.\pipe\handfastd-";

/// The control pipe name for `sid`.
pub(super) fn pipe_name(sid: &str) -> String {
    format!("{PIPE_PREFIX}{sid}")
}

pub(super) async fn start(
    commands: mpsc::Sender<DaemonCommand>,
) -> Result<ControlServerHandle, ControlError> {
    let sid = current_user_sid_string()?;
    let name = pipe_name(&sid);
    let security = PipeSecurity::new(&sid)?;

    let mut options = ServerOptions::new();
    options.first_pipe_instance(true);
    options.reject_remote_clients(true);
    let mut server = create_server(&options, &name, &security)?;

    let (shutdown, mut shutdown_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                connected = server.connect() => {
                    match connected {
                        Ok(()) => {
                            let connected = server;
                            let commands = commands.clone();
                            tokio::spawn(async move { serve(connected, commands).await; });
                        }
                        Err(error) => {
                            tracing::warn!(%error, "control pipe accept failed");
                            drop(server);
                        }
                    }
                    options.first_pipe_instance(false);
                    match create_server(&options, &name, &security) {
                        Ok(next) => server = next,
                        Err(error) => {
                            tracing::warn!(%error, "failed to create the next control pipe instance");
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(ControlServerHandle { task, shutdown })
}

pub(super) async fn connect() -> Result<NamedPipeClient, ControlError> {
    let sid = current_user_sid_string()?;
    let name = pipe_name(&sid);
    let deadline = tokio::time::Instant::now() + CONTROL_IO_TIMEOUT;
    loop {
        match ClientOptions::new().open(&name) {
            Ok(client) => return Ok(client),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ControlError::NotRunning);
            }
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// A protected security descriptor for the current-user SID.
///
/// The descriptor is a `LocalAlloc` allocation from
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, owned and freed by
/// [`Drop`].
struct PipeSecurity {
    descriptor: *mut c_void,
}

// SAFETY: the descriptor is an owned allocation that this value never shares.
// It is only read while the allocation is alive, and the server task moves the
// value between threads without concurrent access.
#[allow(unsafe_code)]
unsafe impl Send for PipeSecurity {}

impl PipeSecurity {
    /// Build `D:P(A;;FA;;;<sid>)`: a protected DACL with one access-allowed ACE
    /// granting `FILE_ALL_ACCESS` to the current-user SID.
    #[allow(unsafe_code)]
    fn new(sid: &str) -> io::Result<Self> {
        let sddl = format!("D:P(A;;FA;;;{sid})");
        let mut wide: Vec<u16> = sddl.encode_utf16().collect();
        wide.push(0);
        let mut descriptor: *mut c_void = ptr::null_mut();
        // SAFETY: `wide` is a live null-terminated UTF-16 string, `descriptor`
        // receives a LocalAlloc allocation owned by the returned value, and no
        // descriptor size output is requested.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                wide.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }
}

impl Drop for PipeSecurity {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if !self.descriptor.is_null() {
            // SAFETY: the descriptor came from
            // ConvertStringSecurityDescriptorToSecurityDescriptorW, whose
            // documented release is LocalFree.
            unsafe { LocalFree(self.descriptor) };
        }
    }
}

#[allow(unsafe_code)]
fn create_server(
    options: &ServerOptions,
    name: &str,
    security: &PipeSecurity,
) -> io::Result<NamedPipeServer> {
    let mut attributes = security.attributes();
    // SAFETY: `attributes` points at a live security descriptor owned by
    // `security`, which outlives every pipe instance created here.
    unsafe {
        options.create_with_security_attributes_raw(
            name,
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr;

    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetSecurityDescriptorDacl, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION, PSID,
        SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    // ACE type 0 is ACCESS_ALLOWED_ACE_TYPE. windows-sys 0.61 keeps the named
    // constant in Win32_System_SystemServices, a feature this module does not
    // need for one ABI value.
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

    #[test]
    fn pipe_security_descriptor_is_one_current_user_ace() {
        let sid = handfast_desktop::winacl::current_user_sid_string().expect("a SID");
        let security = PipeSecurity::new(&sid).expect("build the pipe security");
        assert!(
            has_single_user_ace(&security, &sid),
            "the pipe DACL is one allow ACE for the current-user SID"
        );
    }

    #[allow(unsafe_code)]
    fn has_single_user_ace(security: &PipeSecurity, sid: &str) -> bool {
        let Some(expected) = LocalSid::new(sid) else {
            return false;
        };

        let mut control = 0u16;
        let mut revision = 0u32;
        // SAFETY: `security.descriptor` is a live security descriptor.
        if unsafe { GetSecurityDescriptorControl(security.descriptor, &mut control, &mut revision) }
            == 0
        {
            return false;
        }
        if control & SE_DACL_PROTECTED == 0 {
            return false;
        }

        let mut present = 0;
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: the descriptor is live and the out pointers are valid.
        if unsafe {
            GetSecurityDescriptorDacl(security.descriptor, &mut present, &mut dacl, &mut defaulted)
        } == 0
            || present == 0
            || dacl.is_null()
        {
            return false;
        }

        let mut info = ACL_SIZE_INFORMATION::default();
        // SAFETY: `dacl` points into the live descriptor and `info` has the size
        // `AclSizeInformation` writes.
        if unsafe {
            GetAclInformation(
                dacl,
                (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                size_of::<ACL_SIZE_INFORMATION>() as u32,
                AclSizeInformation,
            )
        } == 0
            || info.AceCount != 1
        {
            return false;
        }

        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: the ACL reports exactly one ACE at index 0.
        if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
            return false;
        }
        // SAFETY: a type 0 ACE is an `ACCESS_ALLOWED_ACE`.
        let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        let ace_sid = ptr::addr_of!(ace.SidStart) as PSID;
        // SAFETY: both SIDs point into live allocations.
        ace.Header.AceType == ACCESS_ALLOWED_ACE_TYPE
            && ace.Mask == FILE_ALL_ACCESS
            && unsafe { EqualSid(expected.0, ace_sid) } != 0
    }

    struct LocalSid(PSID);

    impl LocalSid {
        #[allow(unsafe_code)]
        fn new(sid: &str) -> Option<Self> {
            let mut wide: Vec<u16> = sid.encode_utf16().collect();
            wide.push(0);
            let mut value: PSID = ptr::null_mut();
            // SAFETY: `wide` is a live null-terminated UTF-16 string and
            // `value` receives a LocalAlloc SID owned by the returned value.
            if unsafe { ConvertStringSidToSidW(wide.as_ptr(), &mut value) } == 0 {
                return None;
            }
            Some(Self(value))
        }
    }

    impl Drop for LocalSid {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `ConvertStringSidToSidW` documents LocalFree as the
                // release.
                unsafe { LocalFree(self.0) };
            }
        }
    }
}
