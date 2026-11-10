use std::io;
use std::path::Path;

#[cfg(not(windows))]
pub(super) fn harden(_: &Path) -> io::Result<()> {
    Ok(())
}

/// Set a protected inheritable DACL only on a directory we just created.
#[cfg(windows)]
pub(super) fn harden(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // Every OS allocation is released after the last pointer use.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut length = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut length);
        let mut data = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            data.as_mut_ptr().cast(),
            length,
            &mut length,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sid = null_mut();
        if ConvertSidToStringSidW((*data.as_ptr().cast::<TOKEN_USER>()).User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut count = 0;
        while *sid.add(count) != 0 {
            count += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, count));
        LocalFree(sid.cast());
        let descriptor_text: Vec<u16> =
            format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{text})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
        let mut descriptor = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor_text.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = null_mut();
        let result =
            if GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) == 0
                || present == 0
                || acl.is_null()
            {
                Err(io::Error::last_os_error())
            } else {
                let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
                let code = SetNamedSecurityInfoW(
                    wide.as_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    acl,
                    null(),
                );
                if code == 0 {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(code as i32))
                }
            };
        LocalFree(descriptor);
        result
    }
}

/// Require ownership by the current user and no write access by unrelated users.
#[cfg(unix)]
pub(super) fn private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    // SAFETY: geteuid has no arguments or ownership requirements.
    if m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "cache must be owned by this user and not writable by group/others",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn private(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::{addr_of_mut, null_mut};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    // OS-returned descriptors/SIDs are used only while their owning buffers live.
    unsafe {
        let mut token = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
        let mut user = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        let ok = GetTokenInformation(token, TokenUser, user.as_mut_ptr().cast(), size, &mut size);
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let current = (*(user.as_ptr().cast::<TOKEN_USER>())).User.Sid;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        let error = GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error as i32));
        }
        let result = (|| {
            if owner.is_null() || dacl.is_null() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cache owner/DACL is not private",
                ));
            }
            let mut system = [0u32; 17];
            let mut admins = [0u32; 17];
            let mut creator = [0u32; 17];
            for (kind, buffer) in [
                (WinLocalSystemSid, &mut system),
                (WinBuiltinAdministratorsSid, &mut admins),
                (WinCreatorOwnerSid, &mut creator),
            ] {
                let mut length = std::mem::size_of_val(buffer) as u32;
                if CreateWellKnownSid(kind, null_mut(), buffer.as_mut_ptr().cast(), &mut length)
                    == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            if EqualSid(owner, current) == 0
                && EqualSid(owner, system.as_mut_ptr().cast()) == 0
                && EqualSid(owner, admins.as_mut_ptr().cast()) == 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cache has an unrelated owner",
                ));
            }
            for index in 0..(*dacl).AceCount as u32 {
                let mut ace = null_mut();
                if GetAce(dacl, index, &mut ace) == 0 {
                    return Err(io::Error::last_os_error());
                }
                let header = &*ace.cast::<ACE_HEADER>();
                if header.AceType == 1 {
                    continue;
                } // deny ACE cannot grant mutation
                if header.AceType != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unsupported cache ACL entry",
                    ));
                }
                let allow = &mut *ace.cast::<ACCESS_ALLOWED_ACE>();
                // FILE_WRITE_DATA/APPEND/EA/DELETE_CHILD/ATTRIBUTES, DELETE,
                // WRITE_DAC/OWNER and generic write/all. Read-only grants are fine.
                if allow.Mask & 0x500d_0156 == 0 {
                    continue;
                }
                let sid = addr_of_mut!(allow.SidStart).cast();
                if EqualSid(sid, current) == 0
                    && EqualSid(sid, system.as_mut_ptr().cast()) == 0
                    && EqualSid(sid, admins.as_mut_ptr().cast()) == 0
                    && EqualSid(sid, creator.as_mut_ptr().cast()) == 0
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "cache is writable by another principal",
                    ));
                }
            }
            Ok(())
        })();
        LocalFree(descriptor);
        result
    }
}

#[cfg(not(any(windows, unix)))]
pub(super) fn private(_: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "unsupported cache permissions",
    ))
}
