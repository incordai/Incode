//! The write-restricted token, and the small Win32 helpers the rest of the
//! module shares. Ported from incode's `sandbox::windows::imp`.

use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    ConvertStringSidToSidW,
};
use windows_sys::Win32::Security::{
    CreateRestrictedToken, GetSecurityDescriptorDacl, GetTokenInformation, SetTokenInformation,
    TokenDefaultDacl, TokenLogonSid, TokenUser, ACL, DISABLE_MAX_PRIVILEGE, SID_AND_ATTRIBUTES,
    TOKEN_ACCESS_MASK, TOKEN_DEFAULT_DACL, TOKEN_GROUPS, TOKEN_INFORMATION_CLASS, TOKEN_USER,
    WRITE_RESTRICTED,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Everyone, S-1-1-0.
const EVERYONE: &str = "S-1-1-0";
/// `TOKEN_ALL_ACCESS`, spelled numerically (the named constant is in a
/// feature this crate does not enable).
const TOKEN_ALL: TOKEN_ACCESS_MASK = 0x000f_01ff;

/// A wide, NUL-terminated string for a Win32 W-API.
pub(crate) fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// A handle closed once on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: owned, closed once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

/// A `LocalAlloc`-owned pointer (a SID, a security descriptor, an ACL) freed
/// once on drop.
pub(crate) struct LocalPtr(pub(crate) *mut core::ffi::c_void);

impl Drop for LocalPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: from a Local* allocator, freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The last Win32 error, named by the call that produced it.
pub(crate) fn last_error(call: &str) -> String {
    // SAFETY: reads the calling thread's error value; no arguments.
    let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
    format!("{call} failed with error {code}")
}

/// A SID built from its string form.
pub(crate) fn sid(s: &str) -> Result<LocalPtr, String> {
    let mut out: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: `wide(s)` is NUL-terminated; `out` receives an owned SID.
    let ok = unsafe { ConvertStringSidToSidW(wide(s).as_ptr(), &mut out) };
    if ok == 0 || out.is_null() {
        return Err(format!("could not build the SID {s}"));
    }
    Ok(LocalPtr(out))
}

/// A write-restricted token for one run: this process's token with every
/// privilege but `SeChangeNotify` removed, restricted to `run_sid`,
/// `Everyone` and the logon SID, so it writes only where one of them is
/// granted. Its default DACL lets the command open its own process.
pub(crate) fn restricted(run_sid: &str) -> Result<OwnedHandle, String> {
    // SAFETY: Win32 handle work; each call is checked and every handle this
    // owns is closed. The SIDs and the array outlive the calls that read
    // them.
    unsafe {
        let mut base: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ALL, &mut base) == 0 {
            return Err(last_error("OpenProcessToken"));
        }
        let base = OwnedHandle(base);
        let private = sid(run_sid)?;
        let everyone = sid(EVERYONE)?;
        let logon = logon_sid(base.0)?;
        let restricting = [
            SID_AND_ATTRIBUTES {
                Sid: private.0,
                Attributes: 0,
            },
            SID_AND_ATTRIBUTES {
                Sid: everyone.0,
                Attributes: 0,
            },
            SID_AND_ATTRIBUTES {
                Sid: logon.0,
                Attributes: 0,
            },
        ];
        let mut token: HANDLE = std::ptr::null_mut();
        if CreateRestrictedToken(
            base.0,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            restricting.len() as u32,
            restricting.as_ptr(),
            &mut token,
        ) == 0
        {
            return Err(last_error("CreateRestrictedToken"));
        }
        let token = OwnedHandle(token);
        drop((private, everyone, logon));
        let user = token_user_sid(base.0)?;
        set_default_dacl(token.0, &super::default_dacl_sddl(&user, run_sid))?;
        Ok(token)
    }
}

/// Give `token` the default DACL `sddl`.
unsafe fn set_default_dacl(token: HANDLE, sddl: &str) -> Result<(), String> {
    let mut sd: *mut core::ffi::c_void = std::ptr::null_mut();
    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
        wide(sddl).as_ptr(),
        1,
        &mut sd,
        std::ptr::null_mut(),
    ) == 0
        || sd.is_null()
    {
        return Err(last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    let sd = LocalPtr(sd);
    let mut present = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut defaulted = 0;
    if GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted) == 0 {
        return Err(last_error("GetSecurityDescriptorDacl"));
    }
    let info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
    if SetTokenInformation(
        token,
        TokenDefaultDacl,
        std::ptr::from_ref(&info).cast(),
        std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
    ) == 0
    {
        return Err(last_error("SetTokenInformation(TokenDefaultDacl)"));
    }
    Ok(())
}

/// The user SID of `token`, as a string.
unsafe fn token_user_sid(token: HANDLE) -> Result<String, String> {
    let buf = token_information(token, TokenUser)?;
    let user = &*buf.as_ptr().cast::<TOKEN_USER>();
    sid_to_string(user.User.Sid)
}

/// The logon SID of `token`, as an owned copy.
unsafe fn logon_sid(token: HANDLE) -> Result<LocalPtr, String> {
    let buf = token_information(token, TokenLogonSid)?;
    let groups = &*buf.as_ptr().cast::<TOKEN_GROUPS>();
    if groups.GroupCount == 0 {
        return Err("the token has no logon SID".to_string());
    }
    // Round-trip through the string form so the SID outlives `buf`.
    let text = sid_to_string(groups.Groups.as_ptr().read().Sid)?;
    sid(&text)
}

/// One variable-length token information class, in a buffer aligned for the
/// structure read out of it.
unsafe fn token_information(
    token: HANDLE,
    class: TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u64>, String> {
    let mut len = 0u32;
    // The first call sizes the buffer; it "fails" with ERROR_INSUFFICIENT_BUFFER.
    GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut len);
    if len == 0 {
        return Err(last_error("GetTokenInformation(size)"));
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    if GetTokenInformation(token, class, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
        return Err(last_error("GetTokenInformation"));
    }
    Ok(buf)
}

/// A SID as its string form.
unsafe fn sid_to_string(sid: *mut core::ffi::c_void) -> Result<String, String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut out) == 0 || out.is_null() {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    let owned = LocalPtr(out.cast());
    let mut len = 0usize;
    while *out.add(len) != 0 {
        len += 1;
    }
    let text = String::from_utf16_lossy(std::slice::from_raw_parts(out, len));
    drop(owned);
    Ok(text)
}
