//! Windows credentials must be private when the file is created. Changing
//! permissions on an existing file cannot revoke handles already opened to it.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    EqualSid, GetAce, GetSecurityDescriptorControl, GetSecurityDescriptorOwner,
    GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, DACL_SECURITY_INFORMATION,
    OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

struct Token(HANDLE);
impl Drop for Token {
    fn drop(&mut self) {
        // SAFETY: this wrapper owns the handle returned by Windows.
        unsafe { CloseHandle(self.0) };
    }
}

struct LocalAllocation(*mut c_void);
impl Drop for LocalAllocation {
    fn drop(&mut self) {
        // SAFETY: the conversion functions allocate these buffers with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

fn process_token(access: u32) -> io::Result<Token> {
    let mut handle = null_mut();
    // SAFETY: handle points to an initialized output slot, and GetCurrentProcess
    // supplies the pseudo-handle for the running process.
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut handle) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Token(handle))
}

fn process_user_sid() -> io::Result<String> {
    let token = process_token(TOKEN_QUERY)?;
    let mut length = 0;
    // SAFETY: the first call asks for the size; the second writes that many
    // bytes into an aligned buffer which stays alive while its SID is used.
    unsafe { GetTokenInformation(token.0, TokenUser, null_mut(), 0, &mut length) };
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut storage = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
    let mut sid_string = null_mut();
    unsafe {
        if GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            length,
            &mut length,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid_string) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _allocation = LocalAllocation(sid_string.cast());
        // The conversion returns a NUL-terminated Windows SID string.
        let length = (0..).take_while(|&i| *sid_string.add(i) != 0).count();
        Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
            sid_string, length,
        )))
    }
}

fn create_file(path: &Path, descriptor: &LocalAllocation) -> io::Result<File> {
    let path: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    if path[..path.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows file path",
        ));
    }
    let security = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: both input buffers live through CreateFileW and a successful
    // call transfers ownership of the new file handle to std.
    unsafe {
        let handle = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &security,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(File::from_raw_handle(handle))
    }
}

fn security_descriptor(sddl: &str) -> io::Result<LocalAllocation> {
    let sddl: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    let mut descriptor = null_mut();
    // SAFETY: sddl is terminated and descriptor is an output slot.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

fn require_private_acl(file: &File, expected: &LocalAllocation) -> io::Result<()> {
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: these output pointers are valid. On success, owner and DACL
    // point inside the allocated descriptor held for the rest of this function.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = LocalAllocation(descriptor);
    let mut expected_owner = null_mut();
    let mut defaulted = 0;
    let mut control = 0;
    let mut revision = 0;
    let mut ace = null_mut();
    // SAFETY: the security descriptors are live and self-relative. Validate the
    // ACE type before treating the API's output as an ACCESS_ALLOWED_ACE.
    let private = unsafe {
        if GetSecurityDescriptorOwner(expected.0, &mut expected_owner, &mut defaulted) == 0
            || GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if owner.is_null()
            || dacl.is_null()
            || control & SE_DACL_PROTECTED == 0
            || EqualSid(owner, expected_owner) == 0
            || (*dacl).AceCount != 1
        {
            false
        } else {
            if GetAce(dacl, 0, &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = &*ace.cast::<ACE_HEADER>();
            header.AceType as u32 == ACCESS_ALLOWED_ACE_TYPE && header.AceFlags == 0 && {
                let entry = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                entry.Mask == FILE_ALL_ACCESS
                    && EqualSid(std::ptr::addr_of!(entry.SidStart).cast_mut().cast(), owner) != 0
            }
        }
    };
    if !private {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Windows did not apply the owner-only credential ACL",
        ));
    }
    Ok(())
}

/// Create a same-directory temporary file with a protected DACL granting
/// access to the process user's SID only. Persist it to replace an older
/// file without retaining the destination's inherited permissions.
pub fn tempfile_in(parent: &Path) -> io::Result<tempfile::NamedTempFile> {
    let user = process_user_sid()?;
    let descriptor = security_descriptor(&format!("O:{user}D:P(A;;FA;;;{user})"))?;
    // Canonicalize for an absolute, verbatim path usable by CreateFileW for
    // long paths as well as paths through a directory junction.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    }
    .canonicalize()?;
    let temp = tempfile::Builder::new().make_in(parent, |path| create_file(path, &descriptor))?;
    // Check the actual handle before any credential bytes are written.
    require_private_acl(temp.as_file(), &descriptor)?;
    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::ptr::null;
    use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, ImpersonateLoggedOnUser, RevertToSelf, DISABLE_MAX_PRIVILEGE,
        SID_AND_ATTRIBUTES, TOKEN_DUPLICATE,
    };

    struct Impersonation;
    impl Drop for Impersonation {
        fn drop(&mut self) {
            // SAFETY: this guard is scoped to the thread which impersonated.
            if unsafe { RevertToSelf() } == 0 {
                std::process::abort();
            }
        }
    }

    #[test]
    fn windows_private_file_acl_blocks_ungranted_reader_before_and_after_replace() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("凭据.json");
        let shared = directory.path().join("shared.json");
        let user = process_user_sid().unwrap();
        let descriptor =
            security_descriptor(&format!("O:{user}D:P(A;;FA;;;{user})(A;;FR;;;WD)")).unwrap();
        create_file(&target, &descriptor)
            .unwrap()
            .write_all(b"old")
            .unwrap();
        create_file(&shared, &descriptor)
            .unwrap()
            .write_all(b"control")
            .unwrap();

        let expected = security_descriptor(&format!("O:{user}D:P(A;;FA;;;{user})")).unwrap();
        assert_eq!(
            require_private_acl(&File::open(&shared).unwrap(), &expected)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );

        let token = process_token(TOKEN_DUPLICATE | TOKEN_QUERY).unwrap();
        let world = "S-1-1-0\0".encode_utf16().collect::<Vec<_>>();
        let mut world_sid = null_mut();
        let mut restricted = null_mut();
        // SAFETY: World SID and both out-pointers are valid for their calls.
        let restricted = unsafe {
            assert_ne!(ConvertStringSidToSidW(world.as_ptr(), &mut world_sid), 0);
            let _allocation = LocalAllocation(world_sid);
            let sid = SID_AND_ATTRIBUTES {
                Sid: world_sid,
                Attributes: 0,
            };
            assert_ne!(
                CreateRestrictedToken(
                    token.0,
                    DISABLE_MAX_PRIVILEGE,
                    0,
                    null(),
                    0,
                    null(),
                    1,
                    &sid,
                    &mut restricted
                ),
                0
            );
            Token(restricted)
        };
        let assert_read_denied = |path: &Path| {
            // The restricted token also needs a grant to its World SID. The
            // control file proves that the token can traverse this directory.
            assert_ne!(unsafe { ImpersonateLoggedOnUser(restricted.0) }, 0);
            let _impersonation = Impersonation;
            assert_eq!(std::fs::read(&shared).unwrap(), b"control");
            assert_eq!(
                std::fs::read(path).unwrap_err().kind(),
                io::ErrorKind::PermissionDenied
            );
        };

        let new_target = directory.path().join("new.json");
        for (target, contents) in [(&new_target, b"first"), (&target, b"again")] {
            let mut temp = tempfile_in(directory.path()).unwrap();
            assert_read_denied(temp.path()); // Even an empty new file is private.
            temp.write_all(contents).unwrap();
            temp.as_file().sync_all().unwrap();
            temp.persist(target).unwrap();
            assert_read_denied(target);
            assert_eq!(&std::fs::read(target).unwrap(), contents);
        }
    }
}
