//! Windows DACL hardening for the daemon state and incoming files.
//!
//! Unix uses mode bits: 0700 for directories and 0600 for the state file. The
//! Windows equivalent is a protected DACL that grants full control to the
//! current user and to no other SID. A directory's ACE inherits to children,
//! so a state file or temp file created inside it inherits the single
//! user-only ACE. The protected flag is per object and is not inherited, so
//! the child still needs its own call to become protected.
//!
//! Every `windows-sys` call lives in this module. The unsafe_code allow sits
//! on the functions that call FFI, so it does not cover the rest of the file.

use std::ffi::c_void;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    AclSizeInformation, AddAccessAllowedAceEx, EqualSid, GetAce, GetAclInformation, GetLengthSid,
    GetSecurityDescriptorControl, GetTokenInformation, InitializeAcl, TokenUser,
    ACCESS_ALLOWED_ACE, ACL, ACL_REVISION, ACL_SIZE_INFORMATION, CONTAINER_INHERIT_ACE,
    DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION, PSID,
    SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

// ACE type 0 is ACCESS_ALLOWED_ACE_TYPE. windows-sys 0.61 keeps the named
// constant in Win32_System_SystemServices, a feature this module does not need
// for one ABI value.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// Replace the DACL of `path` with a protected DACL for the current user.
///
/// The DACL holds one access-allowed ACE granting `FILE_ALL_ACCESS` to the
/// process token's user SID. A directory's ACE also inherits to children. The
/// protected flag drops inherited ACEs, including the profile's SYSTEM and
/// Administrators entries. Repeating the call rewrites the same DACL.
///
/// # Errors
///
/// Fails when the security descriptor cannot be written, the process token
/// cannot be read, or the DACL cannot be built.
#[allow(unsafe_code)]
pub fn protect_current_user(path: &Path) -> io::Result<()> {
    let mut name = wide(path);
    let user = current_user_sid()?;
    let acl = user_acl(user.sid(), path.is_dir())?;

    // SAFETY: `name` is a live null-terminated UTF-16 buffer, `acl` is a live
    // ACL built by `InitializeAcl`, and both outlive the call. No owner,
    // group or SACL is replaced.
    let result = unsafe {
        SetNamedSecurityInfoW(
            name.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl.as_ptr().cast::<ACL>(),
            ptr::null(),
        )
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    Ok(())
}

/// Read the DACL of `path` back and report whether it is protected and grants
/// full control to the current user SID and to no other SID.
///
/// This is the read-back the Windows tests use to prove the policy, and an
/// operator audit for a live state path.
///
/// # Errors
///
/// Fails when the security descriptor cannot be read or parsed.
pub fn is_private_to_current_user(path: &Path) -> io::Result<bool> {
    let name = wide(path);
    let user = current_user_sid()?;
    let facts = read_dacl(&name, &user)?;
    Ok(facts.protected && facts.user_only)
}

/// The current process token's user SID as a string.
///
/// Use this for a same-user boundary such as a named-pipe name or DACL.
///
/// # Errors
///
/// Fails when the process token cannot be opened or the SID cannot be
/// converted.
pub fn current_user_sid_string() -> io::Result<String> {
    let user = current_user_sid()?;
    sid_to_string(user.sid())
}

/// Whether `path`'s DACL grants full control to the current user and to no
/// other SID.
///
/// Unlike [`is_private_to_current_user`], this ignores the protected flag, so
/// it reports the effective access a child inherits from its directory.
#[cfg(test)]
pub(crate) fn grants_only_current_user(path: &Path) -> io::Result<bool> {
    let name = wide(path);
    let user = current_user_sid()?;
    Ok(read_dacl(&name, &user)?.user_only)
}

/// The flags on `path`'s single ACE, or `None` when the DACL is not exactly
/// one ACE.
///
/// Only the Windows tests call this.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn ace_flags(path: &Path) -> io::Result<Option<u8>> {
    let name = wide(path);
    let user = current_user_sid()?;
    let facts = read_dacl(&name, &user)?;
    Ok((facts.ace_count == 1).then_some(facts.ace_flags))
}

/// The facts the read-back needs from one DACL.
struct DaclFacts {
    protected: bool,
    ace_count: u32,
    ace_flags: u8,
    user_only: bool,
}

/// Read `name`'s DACL. `user_only` means exactly one allow ACE for the current
/// user with full control.
#[allow(unsafe_code)]
fn read_dacl(name: &[u16], user: &ProcessUser) -> io::Result<DaclFacts> {
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: *mut c_void = ptr::null_mut();

    // SAFETY: `name` is a live null-terminated UTF-16 buffer. The call fills
    // `dacl` and `descriptor`; `descriptor` owns both and is freed below.
    let result = unsafe {
        GetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let descriptor = SecurityDescriptor(descriptor);
    if dacl.is_null() {
        return Ok(DaclFacts {
            protected: false,
            ace_count: 0,
            ace_flags: 0,
            user_only: false,
        });
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` is the descriptor returned above and still alive.
    if unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let protected = control & SE_DACL_PROTECTED != 0;

    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `dacl` points into the live descriptor and `info` has the size
    // `AclSizeInformation` writes.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.AceCount != 1 {
        return Ok(DaclFacts {
            protected,
            ace_count: info.AceCount,
            ace_flags: 0,
            user_only: false,
        });
    }

    let mut ace: *mut c_void = ptr::null_mut();
    // SAFETY: the ACL reports exactly one ACE at index 0.
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `GetAce` returns a pointer inside the live ACL. A type 0 ACE is
    // an `ACCESS_ALLOWED_ACE`, the type the ACL was built with.
    let ace = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
    let ace_sid = ptr::addr_of!(ace.SidStart) as PSID;
    // SAFETY: `user` owns the token buffer holding its SID and `descriptor`
    // owns the DACL holding the ACE SID.
    let user_only = ace.Header.AceType == ACCESS_ALLOWED_ACE_TYPE
        && ace.Mask == FILE_ALL_ACCESS
        && unsafe { EqualSid(user.sid(), ace_sid) } != 0;
    Ok(DaclFacts {
        protected,
        ace_count: 1,
        ace_flags: ace.Header.AceFlags,
        user_only,
    })
}

/// A security descriptor allocated by `GetNamedSecurityInfoW`.
struct SecurityDescriptor(*mut c_void);

impl Drop for SecurityDescriptor {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `GetNamedSecurityInfoW` documents LocalFree as the
            // matching release for the returned descriptor.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The process token's user SID, backed by the token information buffer the
/// SID points into.
struct ProcessUser {
    buffer: Vec<u64>,
}

impl ProcessUser {
    #[allow(unsafe_code)]
    fn sid(&self) -> PSID {
        // SAFETY: `GetTokenInformation(TokenUser)` wrote a `TOKEN_USER` at the
        // start of the buffer and its `Sid` points inside the same buffer,
        // which `self` keeps alive.
        let user = unsafe { &*self.buffer.as_ptr().cast::<TOKEN_USER>() };
        user.User.Sid
    }
}

#[allow(unsafe_code)]
fn current_user_sid() -> io::Result<ProcessUser> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo handle that must not be
    // closed; `OpenProcessToken` fills `token` on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = token_user(token);
    // SAFETY: `token` was opened above and is not used again.
    unsafe { CloseHandle(token) };
    result
}

#[allow(unsafe_code)]
fn token_user(token: HANDLE) -> io::Result<ProcessUser> {
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
    // SAFETY: `sid` points into the live token buffer. `ConvertSidToStringSidW`
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
    // SAFETY: `ConvertSidToStringSidW` documents LocalFree as the release.
    unsafe { LocalFree(string.cast()) };
    Ok(value)
}

#[allow(unsafe_code)]
fn user_acl(sid: PSID, directory: bool) -> io::Result<Vec<u32>> {
    // SAFETY: `sid` points into the live token buffer.
    let sid_len = unsafe { GetLengthSid(sid) } as usize;
    let bytes = size_of::<ACL>() + size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + sid_len;
    // u32 elements keep the ACL aligned for its DWORD fields.
    let mut buffer = vec![0u32; bytes.div_ceil(size_of::<u32>())];
    let acl = buffer.as_mut_ptr().cast::<ACL>();
    let acl_len = (buffer.len() * size_of::<u32>()) as u32;

    // SAFETY: the buffer holds the ACL header plus one ACE and is zeroed.
    if unsafe { InitializeAcl(acl, acl_len, ACL_REVISION) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // A directory's ACE inherits to children; a file's ACE has no children.
    let flags = if directory {
        OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
    } else {
        0
    };
    // SAFETY: the ACL has room for one `ACCESS_ALLOWED_ACE` and `sid` is live.
    if unsafe { AddAccessAllowedAceEx(acl, ACL_REVISION, flags, FILE_ALL_ACCESS, sid) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buffer)
}

fn wide(path: &Path) -> Vec<u16> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_sid_string_is_a_sid() {
        let sid = current_user_sid_string().unwrap();
        assert!(sid.starts_with("S-1-"), "{sid}");
    }

    #[test]
    fn protected_directory_is_private_to_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        protect_current_user(dir.path()).unwrap();
        assert!(is_private_to_current_user(dir.path()).unwrap());
        assert_eq!(
            ace_flags(dir.path()).unwrap(),
            Some((OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) as u8)
        );

        let file = dir.path().join("state.db");
        std::fs::write(&file, b"state").unwrap();
        protect_current_user(&file).unwrap();
        assert!(is_private_to_current_user(&file).unwrap());
        assert_eq!(ace_flags(&file).unwrap(), Some(0));
    }

    #[test]
    fn child_of_a_protected_directory_is_not_protected_until_asked() {
        let dir = tempfile::tempdir().unwrap();
        protect_current_user(dir.path()).unwrap();
        let child = dir.path().join("state.db");
        std::fs::write(&child, b"state").unwrap();

        // The child inherits the single user ACE. The protected flag is per
        // object and is not inherited, so the strict check needs the explicit
        // call.
        assert!(grants_only_current_user(&child).unwrap());
        assert!(!is_private_to_current_user(&child).unwrap());
        protect_current_user(&child).unwrap();
        assert!(is_private_to_current_user(&child).unwrap());
    }
}
