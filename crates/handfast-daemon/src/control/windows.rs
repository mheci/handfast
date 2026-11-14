//! Windows named-pipe transport for the local control channel.
//!
//! The pipe name carries the current-user SID. Every pipe instance gets a
//! protected DACL with one access-allowed ACE that grants `FILE_ALL_ACCESS` to
//! that SID, so another local user cannot open the control channel. Every
//! `windows-sys` call lives in this module; the `unsafe_code` allow sits on the
//! functions that call FFI.
//!
//! The DACL rejection itself stays a manual check until CI has a second local
//! user. The Windows job builds this module.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::ptr;
use std::time::Duration;

use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_PIPE_BUSY, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{serve, ControlError, ControlServerHandle, DaemonCommand, CONTROL_IO_TIMEOUT};

/// Named-pipe path prefix. The rest of the name is the current-user SID.
pub(super) const PIPE_PREFIX: &str = r"\\.\pipe\handfastd-";

/// The control pipe name for the current user.
pub(super) fn pipe_name() -> io::Result<String> {
    match current_user_sid_string() {
        Ok(sid) => Ok(format!("{PIPE_PREFIX}{sid}")),
        Err(_) => std::env::var("USERNAME")
            .map(|name| format!("{PIPE_PREFIX}{name}"))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no current-user SID or USERNAME is available",
                )
            }),
    }
}

pub(super) async fn start(
    commands: mpsc::Sender<DaemonCommand>,
) -> Result<ControlServerHandle, ControlError> {
    let sid = current_user_sid_string()?;
    let name = format!("{PIPE_PREFIX}{sid}");
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
                    if let Err(error) = connected {
                        tracing::warn!(%error, "control pipe accept failed");
                        break;
                    }
                    let connected = server;
                    let commands = commands.clone();
                    tokio::spawn(async move { serve(connected, commands).await; });
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
    let name = pipe_name()?;
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
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`. It is stored as an
/// address so the value stays `Send`; the allocation is owned and freed by
/// [`Drop`].
struct PipeSecurity {
    descriptor: usize,
}

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
        Ok(Self {
            descriptor: descriptor as usize,
        })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor as *mut c_void,
            bInheritHandle: 0,
        }
    }
}

impl Drop for PipeSecurity {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.descriptor != 0 {
            // SAFETY: the descriptor came from
            // ConvertStringSecurityDescriptorToSecurityDescriptorW, whose
            // documented release is LocalFree.
            unsafe { LocalFree(self.descriptor as *mut c_void) };
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

#[allow(unsafe_code)]
fn current_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a pseudo handle that is not closed;
    // OpenProcessToken fills `token` on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = token_user_sid(token);
    // SAFETY: `token` was opened above and is not used again.
    unsafe { CloseHandle(token) };
    let user = result?;
    sid_to_string(user.sid())
}

/// The process token's user SID, backed by the token information buffer the
/// SID points into.
struct ProcessUser {
    buffer: Vec<u64>,
}

impl ProcessUser {
    #[allow(unsafe_code)]
    fn sid(&self) -> PSID {
        // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the
        // start of the buffer and its Sid points inside the same buffer, which
        // `self` keeps alive.
        let user = unsafe { &*self.buffer.as_ptr().cast::<TOKEN_USER>() };
        user.User.Sid
    }
}

#[allow(unsafe_code)]
fn token_user_sid(token: HANDLE) -> io::Result<ProcessUser> {
    let mut length = 0u32;
    // SAFETY: a null buffer with length 0 is the documented size probe. It
    // writes the required size even though it returns false.
    unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    // u64 elements align the buffer for the TOKEN_USER pointer and the SID.
    let mut buffer = vec![0u64; (length as usize).div_ceil(size_of::<u64>())];
    // SAFETY: the buffer is at least `length` bytes and correctly aligned.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            length,
            &mut length,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ProcessUser { buffer })
}

#[allow(unsafe_code)]
fn sid_to_string(sid: PSID) -> io::Result<String> {
    let mut string: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` points into the live token buffer. ConvertSidToStringSidW
    // allocates a null-terminated UTF-16 string, freed below.
    if unsafe { ConvertSidToStringSidW(sid, &mut string) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut length = 0usize;
    // SAFETY: `string` is a live null-terminated UTF-16 buffer.
    unsafe {
        while *string.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: the loop above bounded the string length.
    let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(string, length) });
    // SAFETY: ConvertSidToStringSidW documents LocalFree as the release.
    unsafe { LocalFree(string.cast()) };
    Ok(value)
}
