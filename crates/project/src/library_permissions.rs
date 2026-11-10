//! Access metadata that must survive a library file replacement.

use std::fs::File;
use std::io;

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) fn fingerprint(file: &File) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;

    if has_extended_acl(file)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "extended ACL cannot be preserved by library replacement",
        ));
    }
    let metadata = file.metadata()?;
    Ok(format!("{}:{}", metadata.uid(), metadata.gid()))
}

#[cfg(target_os = "macos")]
fn has_extended_acl(file: &File) -> io::Result<bool> {
    use std::ffi::c_void;
    use std::os::fd::AsRawFd;

    unsafe extern "C" {
        fn acl_get_fd(fd: libc::c_int) -> *mut c_void;
        fn acl_free(acl: *mut c_void) -> libc::c_int;
    }

    // Darwin returns a separately allocated ACL or ENOENT for no ACL.
    // The File keeps the descriptor live through the query.
    let acl = unsafe { acl_get_fd(file.as_raw_fd()) };
    if !acl.is_null() {
        // SAFETY: acl_get_fd returned this allocation. Release it once.
        unsafe { acl_free(acl) };
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(target_os = "linux")]
fn has_extended_acl(file: &File) -> io::Result<bool> {
    use std::os::fd::AsRawFd;

    // With no output buffer, fgetxattr reports only the attribute size.
    let size = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            c"system.posix_acl_access".as_ptr(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size >= 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENODATA) {
        Ok(false)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
pub(super) fn fingerprint(file: &File) -> io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::{
            Authorization::{GetSecurityInfo, SE_FILE_OBJECT},
            GetSecurityDescriptorLength, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION,
            LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        },
    };

    let mut descriptor = null_mut();
    // SAFETY: the live file handle and out pointer are valid. The returned
    // self-relative descriptor belongs to this call and is freed below.
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | GROUP_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | LABEL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            null_mut(),
            null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    if descriptor.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing security descriptor",
        ));
    }
    // SAFETY: GetSecurityInfo returned a valid descriptor of this length.
    let fingerprint = unsafe {
        let size = GetSecurityDescriptorLength(descriptor);
        let bytes = std::slice::from_raw_parts(descriptor.cast::<u8>(), size as usize);
        let fingerprint = format!("{:x}", Sha256::digest(bytes));
        LocalFree(descriptor);
        fingerprint
    };
    Ok(fingerprint)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) fn fingerprint(_file: &File) -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file access permissions cannot be verified on this platform",
    ))
}
