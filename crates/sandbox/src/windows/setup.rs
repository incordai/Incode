//! `sbx setup-windows`: the offline account, its password, and the WFP
//! filters that cut it off ([`super::offline`] says why each is so); and,
//! at run time, finding them ([`ready`]) and reading the password
//! ([`password`]).
//!
//! Everything here that changes the machine needs an elevated process and
//! refuses clearly without one. Each step is idempotent: running the setup
//! again resets the password, re-adds each filter by its fixed key, and
//! keeps the folders granted for reading.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    LocalFree, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, FWP_E_ALREADY_EXISTS,
    FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND, FWP_E_PROVIDER_NOT_FOUND, FWP_E_SUBLAYER_NOT_FOUND,
    HANDLE,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    NetLocalGroupAddMembers, NetLocalGroupDelMembers, NetUserAdd, NetUserDel, NetUserSetInfo,
    LOCALGROUP_MEMBERS_INFO_0, UF_DONT_EXPIRE_PASSWD, UF_NORMAL_ACCOUNT, UF_PASSWD_CANT_CHANGE,
    UF_SCRIPT, USER_INFO_1, USER_INFO_1003, USER_INFO_1008, USER_PRIV_USER,
};
use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform::{
    FwpmEngineClose0, FwpmEngineOpen0, FwpmFilterAdd0, FwpmFilterDeleteByKey0, FwpmFilterGetByKey0,
    FwpmFreeMemory0, FwpmProviderAdd0, FwpmProviderDeleteByKey0, FwpmSubLayerAdd0,
    FwpmSubLayerDeleteByKey0, FwpmTransactionAbort0, FwpmTransactionBegin0, FwpmTransactionCommit0,
    FWPM_ACTION0, FWPM_ACTION0_0, FWPM_CONDITION_ALE_USER_ID, FWPM_CONDITION_IP_PROTOCOL,
    FWPM_CONDITION_IP_REMOTE_ADDRESS, FWPM_CONDITION_IP_REMOTE_PORT, FWPM_DISPLAY_DATA0,
    FWPM_FILTER0, FWPM_FILTER0_0, FWPM_FILTER_CONDITION0, FWPM_FILTER_FLAG_PERSISTENT,
    FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6,
    FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
    FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4, FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6, FWPM_PROVIDER0,
    FWPM_PROVIDER_FLAG_PERSISTENT, FWPM_SESSION0, FWPM_SUBLAYER0, FWPM_SUBLAYER_FLAG_PERSISTENT,
    FWP_ACTION_BLOCK, FWP_ACTION_PERMIT, FWP_BYTE_BLOB, FWP_CONDITION_VALUE0,
    FWP_CONDITION_VALUE0_0, FWP_MATCH_EQUAL, FWP_MATCH_RANGE, FWP_RANGE0, FWP_RANGE_TYPE,
    FWP_SECURITY_DESCRIPTOR_TYPE, FWP_UINT16, FWP_UINT32, FWP_UINT8, FWP_VALUE0, FWP_VALUE0_0,
};
use windows_sys::Win32::Security::Authorization::{SetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, CryptProtectData, CryptUnprotectData, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
    CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};
use windows_sys::Win32::Security::{
    GetSecurityDescriptorLength, LogonUserW, LookupAccountNameW, LookupAccountSidW,
    DACL_SECURITY_INFORMATION, LOGON32_LOGON_INTERACTIVE, LOGON32_PROVIDER_DEFAULT,
    PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, SID_NAME_USE,
};
use windows_sys::Win32::Storage::FileSystem::{CreateDirectoryW, FILE_ATTRIBUTE_REPARSE_POINT};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE,
    KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE,
};
use windows_sys::Win32::System::WindowsProgramming::GetComputerNameW;

use super::offline::{
    self, password_from, Cond, FilterSpec, Layer, Ready, Record, SetupArgs, ACCOUNT,
    ACCOUNT_COMMENT, DEFAULT_PROXY_PORTS, DPAPI_ENTROPY, FILTERS, MAIN_FILTER, PROVIDER_KEY,
    RECORD_FILE, STATE_DIR, SUBLAYER_KEY, SUBLAYER_WEIGHT,
};
use super::token::{
    current_user_sid, dacl_of, is_elevated, last_error, security_descriptor, sid, sid_to_string,
    wide, LocalPtr, OwnedHandle,
};
use super::{acl, refuse_broad};

use windows_sys::core::GUID as Guid;

/// `NERR_UserExists` / `NERR_UserNotFound` (lmerr.h).
const NERR_USER_EXISTS: u32 = 2224;
const NERR_USER_NOT_FOUND: u32 = 2221;
/// `ERROR_MEMBER_NOT_IN_ALIAS` / `ERROR_MEMBER_IN_ALIAS`.
const ERROR_MEMBER_NOT_IN_ALIAS: u32 = 1377;
const ERROR_MEMBER_IN_ALIAS: u32 = 1378;
/// `RPC_C_AUTHN_DEFAULT`, as Codex opens the engine.
const RPC_C_AUTHN_DEFAULT: u32 = 0xFFFF_FFFF;
/// The local groups, by well-known SID (their names are localised).
const USERS: &str = "S-1-5-32-545";
const ADMINISTRATORS: &str = "S-1-5-32-544";
const SYSTEM: &str = "S-1-5-18";
/// Where Windows keeps accounts hidden from the sign-in screen.
const USERLIST_KEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

const NEEDS_ELEVATION: &str = "sbx setup-windows changes the machine (a local account and \
     firewall filters) and needs administrator rights: run it from an elevated prompt \
     (right-click your terminal, Run as administrator)";

// ---- Secrets. ----

/// A password as a NUL-terminated wide string, zeroed when dropped.
pub(crate) struct Secret(Vec<u16>);

impl Secret {
    /// Take `text`, zeroing it.
    fn from_string(text: String) -> Self {
        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
        zero(&mut text.into_bytes());
        Self(wide)
    }

    pub(crate) fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }

    /// Its UTF-8 bytes (zero them after use).
    fn utf8(&self) -> Vec<u8> {
        String::from_utf16_lossy(&self.0[..self.0.len() - 1]).into_bytes()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        zero(&mut self.0);
    }
}

/// Overwrite `buf` with zeros in a way the compiler may not skip.
fn zero<T: Copy + Default>(buf: &mut [T]) {
    for x in buf.iter_mut() {
        // SAFETY: a valid, aligned element of a live slice.
        unsafe { std::ptr::write_volatile(x, T::default()) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

fn random_bytes() -> Result<[u8; 32], String> {
    let mut buf = [0u8; 32];
    // SAFETY: a buffer of the length passed; the system RNG needs no handle.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(format!("BCryptGenRandom failed with status {status:#x}"));
    }
    Ok(buf)
}

/// DPAPI, machine scope: any process on this machine that can read the blob
/// can decrypt it, so the record's ACL is what protects it; the blob is
/// useless anywhere else (a backup, a copied disk). Chosen over user scope
/// because setup may run elevated as a different administrator than the
/// user who runs sandboxes (then a user-scoped blob would be that admin's),
/// and over LSA private data, which only administrators can read back, and
/// runs are not elevated. Codex makes the same choice (`dpapi.rs`).
fn protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(plain);
    let entropy = blob(DPAPI_ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: the input blobs point at live slices; the output is
    // LocalAlloc'd by the call and freed below.
    unsafe {
        if CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE,
            &mut out,
        ) == 0
        {
            return Err(last_error("CryptProtectData"));
        }
        let owned = LocalPtr(out.pbData.cast());
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        drop(owned);
        Ok(bytes)
    }
}

fn unprotect(sealed: &[u8]) -> Result<Vec<u8>, String> {
    let input = blob(sealed);
    let entropy = blob(DPAPI_ENTROPY);
    let mut out = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    // SAFETY: as in `protect`; the plaintext is zeroed before it is freed.
    unsafe {
        if CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        ) == 0
        {
            return Err(last_error("CryptUnprotectData"));
        }
        let plain = std::slice::from_raw_parts_mut(out.pbData, out.cbData as usize);
        let bytes = plain.to_vec();
        zero(plain);
        LocalFree(out.pbData.cast());
        Ok(bytes)
    }
}

fn blob(bytes: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        // The API takes a mutable pointer but only reads an input blob.
        pbData: bytes.as_ptr().cast_mut(),
    }
}

// ---- Where things are. ----

/// `%ProgramData%\sbx`. From the environment the parent was started with
/// (a user who points it elsewhere only misleads their own runs).
pub(crate) fn state_dir() -> PathBuf {
    let base = std::env::var_os("ProgramData")
        .or_else(|| std::env::var_os("ALLUSERSPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join(STATE_DIR)
}

pub(crate) fn record_path() -> PathBuf {
    state_dir().join(RECORD_FILE)
}

/// `COMPUTER\name`: a local account, never a domain one of the same name.
fn qualified(name: &str) -> String {
    let mut buf = [0u16; 64];
    let mut len = buf.len() as u32;
    // SAFETY: a buffer and its length in characters.
    if unsafe { GetComputerNameW(buf.as_mut_ptr(), &mut len) } != 0 {
        format!("{}\\{name}", String::from_utf16_lossy(&buf[..len as usize]))
    } else {
        name.to_string()
    }
}

/// The offline account's SID, if it exists.
pub(crate) fn account_sid() -> Option<String> {
    lookup_sid(&qualified(ACCOUNT)).ok()
}

fn lookup_sid(name: &str) -> Result<String, String> {
    let name_w = wide(name);
    let mut sid_buf = vec![0u64; 16]; // 128 bytes, aligned: room for any SID
    let mut sid_len = (sid_buf.len() * 8) as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: buffers with their sizes; the SID is read while `sid_buf` lives.
    unsafe {
        if LookupAccountNameW(
            std::ptr::null(),
            name_w.as_ptr(),
            sid_buf.as_mut_ptr().cast(),
            &mut sid_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        ) == 0
        {
            return Err(last_error("LookupAccountNameW"));
        }
        sid_to_string(sid_buf.as_mut_ptr().cast())
    }
}

/// A local group's (localised) name, from its well-known SID.
fn group_name(sid_text: &str) -> Result<Vec<u16>, String> {
    let group = sid(sid_text)?;
    let mut name = vec![0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: buffers with their sizes; `group` lives across the call.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            group.0,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    if ok == 0 {
        return Err(last_error("LookupAccountSidW"));
    }
    name.truncate(name_len as usize);
    name.push(0);
    Ok(name)
}

// ---- Run time: is it set up, and the password. ----

/// The setup, if it is in place and this user can use it. Checked once per
/// process: reading the record, looking the account up, and (where this
/// user may) asking WFP for the main filter.
pub(crate) fn ready() -> &'static Result<Ready, String> {
    static READY: OnceLock<Result<Ready, String>> = OnceLock::new();
    READY.get_or_init(probe)
}

fn probe() -> Result<Ready, String> {
    let path = record_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err("the offline account is not set up".to_string())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(format!(
                "the offline account was set up for another user ({} is not readable by this one)",
                path.display()
            ))
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let record = Record::parse(&text)
        .map_err(|e| format!("{} is damaged ({e}); set it up again", path.display()))?;
    match account_sid() {
        Some(sid) if sid == record.sid => {}
        Some(_) => {
            return Err(format!(
                "the account {ACCOUNT} is not the one the setup made; set it up again"
            ))
        }
        None => {
            return Err(format!(
                "the account {ACCOUNT} no longer exists; set it up again"
            ))
        }
    }
    if filter_missing(MAIN_FILTER) {
        return Err("its firewall filters are gone; set it up again".to_string());
    }
    Ok(Ready {
        sid: record.sid,
        proxy_ports: record.proxy_ports,
        record: path,
    })
}

/// Whether WFP says, for certain, that `spec` is not installed. Any other
/// answer (this user may not open the engine or read the filter) is not
/// certainty, and the record, which setup writes only after the filters are
/// in, is trusted.
fn filter_missing(spec: &FilterSpec) -> bool {
    let Ok(engine) = Engine::open(1_000) else {
        return false;
    };
    let key = guid(spec.key);
    let mut filter: *mut FWPM_FILTER0 = std::ptr::null_mut();
    // SAFETY: an open engine, a key that lives across the call; the result,
    // if any, is freed with FwpmFreeMemory0.
    let err = unsafe { FwpmFilterGetByKey0(engine.0, &key, &mut filter) };
    if !filter.is_null() {
        // SAFETY: memory WFP allocated for this call.
        unsafe { FwpmFreeMemory0(std::ptr::from_mut(&mut filter).cast()) };
    }
    err == FWP_E_FILTER_NOT_FOUND as u32
}

/// The account name and its password, from the record at `path`.
///
/// # Errors
/// When the record cannot be read or decrypted.
pub(crate) fn password(path: &Path) -> Result<(String, Secret), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let record = Record::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut plain = unprotect(&record.password)
        .map_err(|e| format!("the offline account's password cannot be read: {e}"))?;
    let text = String::from_utf8(plain.clone());
    zero(&mut plain);
    let text = text.map_err(|_| "the offline account's password is damaged".to_string())?;
    Ok((record.account, Secret::from_string(text)))
}

// ---- Setup. ----

/// Run `sbx setup-windows` (or `--remove`): what it did, line by line.
///
/// # Errors
/// When not elevated, or a step fails (said which).
pub fn run(args: &SetupArgs) -> Result<Vec<String>, String> {
    if !is_elevated() {
        return Err(NEEDS_ELEVATION.to_string());
    }
    if args.remove {
        remove()
    } else {
        install(args)
    }
}

fn install(args: &SetupArgs) -> Result<Vec<String>, String> {
    let mut report = Vec::new();
    let owner = current_user_sid()?;
    let previous = std::fs::read_to_string(record_path())
        .ok()
        .and_then(|t| Record::parse(&t).ok());
    // The password is about to change: an old record would name the wrong
    // one if a step below failed. Without a record, runs say "not set up".
    match std::fs::remove_file(record_path()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", record_path().display())),
    }

    // 1. The account, with a fresh password nobody sees.
    let mut random = random_bytes()?;
    let password = Secret::from_string(password_from(&random));
    zero(&mut random);
    let created = ensure_account(&password)?;
    report.push(format!(
        "account {ACCOUNT}: {} (in Users, not Administrators; password random, not shown)",
        if created {
            "created"
        } else {
            "exists, password reset"
        }
    ));
    let account = lookup_sid(&qualified(ACCOUNT))?;
    check_logon(&password)?;
    match hide_from_sign_in(true) {
        Ok(()) => report.push("hidden from the sign-in screen".to_string()),
        Err(e) => report.push(format!("not hidden from the sign-in screen ({e})")),
    }

    // 2. Folders it may read (toolchains): the earlier ones kept.
    let profile = std::env::var("USERPROFILE").ok();
    let mut reads: Vec<PathBuf> = previous
        .as_ref()
        .filter(|p| p.sid == account)
        .map(|p| p.reads.clone())
        .unwrap_or_default();
    for dir in &args.reads {
        let dir = std::fs::canonicalize(dir)
            .map(|d| PathBuf::from(super::strip_verbatim(&d.to_string_lossy())))
            .map_err(|e| format!("--read {}: {e}", dir.display()))?;
        if !dir.is_dir() {
            return Err(format!("--read {}: not a folder", dir.display()));
        }
        refuse_broad(&dir.to_string_lossy(), profile.as_deref())
            .map_err(|e| format!("--read {e}"))?;
        acl::grant_read(&dir, &account)?;
        report.push(format!("{ACCOUNT} may read {}", dir.display()));
        if !reads.iter().any(|r| {
            super::path_key(&r.to_string_lossy()) == super::path_key(&dir.to_string_lossy())
        }) {
            reads.push(dir);
        }
    }

    // 3. The filters.
    let ports = args
        .proxy_ports
        .or(previous.as_ref().map(|p| p.proxy_ports))
        .unwrap_or(DEFAULT_PROXY_PORTS);
    install_filters(&account, ports)?;
    report.push(format!(
        "Windows Filtering Platform: {} filters block every connection {ACCOUNT} makes or \
         accepts, except TCP to 127.0.0.1 ports {}-{} (the proxy)",
        FILTERS.len(),
        ports.0,
        ports.1
    ));

    // 4. The record, last: its presence says everything above is in place.
    let mut plain = password.utf8();
    let sealed = protect(&plain);
    zero(&mut plain);
    let record = Record {
        account: ACCOUNT.to_string(),
        sid: account,
        owner,
        proxy_ports: ports,
        reads,
        password: sealed?,
    };
    write_record(&record)?;
    report.push(format!(
        "record {} (DPAPI, readable by SYSTEM, Administrators and {} only)",
        record_path().display(),
        account_name(&record.owner)
    ));
    Ok(report)
}

/// A SID's `DOMAIN\name`, or the SID itself.
fn account_name(sid_text: &str) -> String {
    let Ok(s) = sid(sid_text) else {
        return sid_text.to_string();
    };
    let mut name = vec![0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain = vec![0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut use_: SID_NAME_USE = 0;
    // SAFETY: buffers with their sizes; `s` lives across the call.
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            s.0,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    if ok == 0 {
        return sid_text.to_string();
    }
    format!(
        "{}\\{}",
        String::from_utf16_lossy(&domain[..domain_len as usize]),
        String::from_utf16_lossy(&name[..name_len as usize])
    )
}

/// Create the account, or reset an existing one's password; `Users` only.
/// Whether it was created.
fn ensure_account(password: &Secret) -> Result<bool, String> {
    let name = wide(ACCOUNT);
    let comment = wide(ACCOUNT_COMMENT);
    let flags = UF_SCRIPT | UF_NORMAL_ACCOUNT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE;
    let info = USER_INFO_1 {
        usri1_name: name.as_ptr().cast_mut(),
        usri1_password: password.as_ptr().cast_mut(),
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: std::ptr::null_mut(),
        usri1_comment: comment.as_ptr().cast_mut(),
        usri1_flags: flags,
        usri1_script_path: std::ptr::null_mut(),
    };
    let mut parm = 0u32;
    // SAFETY: a USER_INFO_1 whose strings outlive the call (level 1).
    let status = unsafe {
        NetUserAdd(
            std::ptr::null(),
            1,
            std::ptr::from_ref(&info).cast(),
            &mut parm,
        )
    };
    let created = match status {
        0 => true,
        NERR_USER_EXISTS => {
            let pw = USER_INFO_1003 {
                usri1003_password: password.as_ptr().cast_mut(),
            };
            // SAFETY: as above, level 1003 (the password).
            let status = unsafe {
                NetUserSetInfo(
                    std::ptr::null(),
                    name.as_ptr(),
                    1003,
                    std::ptr::from_ref(&pw).cast(),
                    &mut parm,
                )
            };
            if status != 0 {
                return Err(format!(
                    "resetting {ACCOUNT}'s password failed with error {status}"
                ));
            }
            // Enabled again if someone disabled it, and the flags as made.
            let f = USER_INFO_1008 {
                usri1008_flags: flags,
            };
            // SAFETY: as above, level 1008 (the flags).
            let status = unsafe {
                NetUserSetInfo(
                    std::ptr::null(),
                    name.as_ptr(),
                    1008,
                    std::ptr::from_ref(&f).cast(),
                    &mut parm,
                )
            };
            if status != 0 {
                return Err(format!(
                    "setting {ACCOUNT}'s flags failed with error {status}"
                ));
            }
            false
        }
        e => {
            return Err(format!(
                "creating the account {ACCOUNT} failed with error {e} (parameter {parm})"
            ))
        }
    };
    // `Users` (needed to log on and to read the system folders), and never
    // `Administrators`, even if someone added it there.
    let account = sid(&lookup_sid(&qualified(ACCOUNT))?)?;
    let member = LOCALGROUP_MEMBERS_INFO_0 {
        lgrmi0_sid: account.0,
    };
    for (group, add) in [(USERS, true), (ADMINISTRATORS, false)] {
        let group_w = group_name(group)?;
        // SAFETY: one LOCALGROUP_MEMBERS_INFO_0 whose SID outlives the call.
        let status = unsafe {
            if add {
                NetLocalGroupAddMembers(
                    std::ptr::null(),
                    group_w.as_ptr(),
                    0,
                    std::ptr::from_ref(&member).cast(),
                    1,
                )
            } else {
                NetLocalGroupDelMembers(
                    std::ptr::null(),
                    group_w.as_ptr(),
                    0,
                    std::ptr::from_ref(&member).cast(),
                    1,
                )
            }
        };
        let fine = status == 0
            || (add && status == ERROR_MEMBER_IN_ALIAS)
            || (!add && status == ERROR_MEMBER_NOT_IN_ALIAS);
        if !fine {
            return Err(format!(
                "{} {ACCOUNT} {} {} failed with error {status}",
                if add { "adding" } else { "removing" },
                if add { "to" } else { "from" },
                String::from_utf16_lossy(&group_w[..group_w.len() - 1])
            ));
        }
    }
    Ok(created)
}

/// Log the account on once, as a run will: a policy that denies it local
/// logon would otherwise only show up at the first run.
fn check_logon(password: &Secret) -> Result<(), String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: NUL-terminated strings; the token, if any, is owned and closed.
    let ok = unsafe {
        LogonUserW(
            wide(ACCOUNT).as_ptr(),
            wide(".").as_ptr(),
            password.as_ptr(),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if ok == 0 {
        return Err(format!(
            "{ACCOUNT} cannot log on ({}); a group policy may deny it \"Allow log on locally\"",
            last_error("LogonUserW")
        ));
    }
    drop(OwnedHandle(token));
    Ok(())
}

/// Hide (or show again) the account on the sign-in screen.
fn hide_from_sign_in(hide: bool) -> Result<(), String> {
    let name = wide(ACCOUNT);
    // SAFETY: registry calls with NUL-terminated strings; the key is closed.
    unsafe {
        if !hide {
            let status = RegDeleteKeyValueW(
                HKEY_LOCAL_MACHINE,
                wide(USERLIST_KEY).as_ptr(),
                name.as_ptr(),
            );
            return match status {
                0 => Ok(()),
                e if e == ERROR_FILE_NOT_FOUND => Ok(()),
                e => Err(format!("registry error {e}")),
            };
        }
        let mut key: HKEY = std::ptr::null_mut();
        let status = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            wide(USERLIST_KEY).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        );
        if status != 0 {
            return Err(format!("registry error {status}"));
        }
        let zero_dword = 0u32.to_ne_bytes();
        let status = RegSetValueExW(
            key,
            name.as_ptr(),
            0,
            REG_DWORD,
            zero_dword.as_ptr(),
            zero_dword.len() as u32,
        );
        RegCloseKey(key);
        if status != 0 {
            return Err(format!("registry error {status}"));
        }
    }
    Ok(())
}

/// `%ProgramData%\sbx` with the record's ACL, then the record in it,
/// replacing any old one.
fn write_record(record: &Record) -> Result<(), String> {
    let dir = state_dir();
    let sddl = offline::state_dir_sddl(&record.owner);
    let sd = security_descriptor(&sddl)?;
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let dir_w = wide(&dir);
    // SAFETY: a NUL-terminated path and attributes whose descriptor lives.
    if unsafe { CreateDirectoryW(dir_w.as_ptr(), &attrs) } == 0 {
        // SAFETY: reads the calling thread's error value.
        let err = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        if err != ERROR_ALREADY_EXISTS {
            return Err(format!(
                "creating {} failed with error {err}",
                dir.display()
            ));
        }
        // It was there: `%ProgramData%` lets any user create a folder, so
        // trust it only if an administrator (or SYSTEM) owns it and it is a
        // real folder, then put the ACL back.
        let meta =
            std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if !meta.is_dir() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!(
                "{} is not a plain folder (a link?); remove it and run setup again",
                dir.display()
            ));
        }
        let owner = acl::owner_of(&dir)?;
        if owner != ADMINISTRATORS && owner != SYSTEM {
            return Err(format!(
                "{} is owned by {}, not by Administrators; remove it and run setup again",
                dir.display(),
                account_name(&owner)
            ));
        }
        // SAFETY: the DACL lives inside `sd`, alive across the call.
        let err = unsafe {
            SetNamedSecurityInfoW(
                dir_w.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl_of(&sd)?,
                std::ptr::null(),
            )
        };
        if err != 0 {
            return Err(format!(
                "setting the ACL of {} failed with error {err}",
                dir.display()
            ));
        }
    }
    let file = record_path();
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", file.display())),
    }
    // A new file (never written through whatever was there), which takes
    // the folder's ACL.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
        .and_then(|mut f| f.write_all(record.to_text().as_bytes()))
        .map_err(|e| format!("{}: {e}", file.display()))
}

fn remove() -> Result<Vec<String>, String> {
    let mut report = Vec::new();
    let mut failed = Vec::new();
    let record = std::fs::read_to_string(record_path())
        .ok()
        .and_then(|t| Record::parse(&t).ok());
    let account = account_sid();
    if let (Some(record), Some(account)) = (&record, &account) {
        for dir in record.reads.iter().filter(|d| d.exists()) {
            match acl::revoke(dir, account) {
                Ok(()) => report.push(format!("{ACCOUNT} may no longer read {}", dir.display())),
                Err(e) => failed.push(format!("{}: {e}", dir.display())),
            }
        }
    }
    match remove_filters() {
        Ok(()) => report.push("Windows Filtering Platform filters removed".to_string()),
        Err(e) => failed.push(format!("filters: {e}")),
    }
    // SAFETY: a NUL-terminated name.
    match unsafe { NetUserDel(std::ptr::null(), wide(ACCOUNT).as_ptr()) } {
        0 => report.push(format!("account {ACCOUNT} deleted")),
        NERR_USER_NOT_FOUND => report.push(format!("account {ACCOUNT}: not there")),
        e => failed.push(format!("deleting {ACCOUNT} failed with error {e}")),
    }
    if let Err(e) = hide_from_sign_in(false) {
        failed.push(format!("sign-in screen entry: {e}"));
    }
    let dir = state_dir();
    let plain_dir = std::fs::symlink_metadata(&dir)
        .is_ok_and(|m| m.is_dir() && m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0);
    if plain_dir {
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => report.push(format!("{} removed", dir.display())),
            Err(e) => failed.push(format!("{}: {e}", dir.display())),
        }
    }
    if failed.is_empty() {
        Ok(report)
    } else {
        Err(format!(
            "{}\nsome of it could not be undone:\n  {}",
            report.join("\n"),
            failed.join("\n  ")
        ))
    }
}

// ---- WFP. ----

/// An open WFP engine, closed on drop.
struct Engine(HANDLE);

impl Engine {
    fn open(wait_ms: u32) -> Result<Self, String> {
        // SAFETY: a zeroed session is the documented default; a
        // NUL-terminated name that lives across the call.
        unsafe {
            let name = wide("sbx setup-windows");
            let mut session: FWPM_SESSION0 = std::mem::zeroed();
            session.displayData.name = name.as_ptr().cast_mut();
            session.txnWaitTimeoutInMSec = wait_ms;
            let mut handle: HANDLE = std::ptr::null_mut();
            let err = FwpmEngineOpen0(
                std::ptr::null(),
                RPC_C_AUTHN_DEFAULT,
                std::ptr::null(),
                &session,
                &mut handle,
            );
            if err != 0 {
                return Err(format!("FwpmEngineOpen0 failed with error {err:#x}"));
            }
            Ok(Self(handle))
        }
    }

    /// Run `body` in one transaction: all of it, or (on error) none.
    fn transaction(&self, body: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
        // SAFETY: an open engine; the transaction is committed or aborted.
        unsafe {
            let err = FwpmTransactionBegin0(self.0, 0);
            if err != 0 {
                return Err(format!("FwpmTransactionBegin0 failed with error {err:#x}"));
            }
            if let Err(e) = body() {
                FwpmTransactionAbort0(self.0);
                return Err(e);
            }
            let err = FwpmTransactionCommit0(self.0);
            if err != 0 {
                FwpmTransactionAbort0(self.0);
                return Err(format!("FwpmTransactionCommit0 failed with error {err:#x}"));
            }
        }
        Ok(())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // SAFETY: owned, closed once.
        unsafe { FwpmEngineClose0(self.0) };
    }
}

fn guid(value: u128) -> Guid {
    Guid::from_u128(value)
}

fn fwp_ok(err: u32, call: &str, tolerated: &[i32]) -> Result<(), String> {
    if err == 0 || tolerated.iter().any(|t| *t as u32 == err) {
        Ok(())
    } else {
        Err(format!("{call} failed with error {err:#x}"))
    }
}

fn layer_key(layer: Layer) -> Guid {
    match layer {
        Layer::ConnectV4 => FWPM_LAYER_ALE_AUTH_CONNECT_V4,
        Layer::ConnectV6 => FWPM_LAYER_ALE_AUTH_CONNECT_V6,
        Layer::AcceptV4 => FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4,
        Layer::AcceptV6 => FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6,
        Layer::AssignV4 => FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4,
        Layer::AssignV6 => FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V6,
    }
}

fn install_filters(account_sid: &str, ports: (u16, u16)) -> Result<(), String> {
    let engine = Engine::open(30_000)?;
    let user_sd = security_descriptor(&offline::user_condition_sddl(account_sid))?;
    // SAFETY: a valid self-relative descriptor.
    let user_sd_len = unsafe { GetSecurityDescriptorLength(user_sd.0) };
    let provider_key = guid(PROVIDER_KEY);
    let sublayer_key = guid(SUBLAYER_KEY);
    let name = wide("sbx offline account");
    let description = wide("sbx setup-windows: the sbx-offline account has no network");
    engine.transaction(|| {
        // SAFETY: every structure passed points at locals that outlive the
        // calls (names, keys, the descriptor blob, the range); WFP copies
        // what it keeps.
        unsafe {
            let mut provider: FWPM_PROVIDER0 = std::mem::zeroed();
            provider.providerKey = provider_key;
            provider.displayData = FWPM_DISPLAY_DATA0 {
                name: name.as_ptr().cast_mut(),
                description: description.as_ptr().cast_mut(),
            };
            provider.flags = FWPM_PROVIDER_FLAG_PERSISTENT;
            fwp_ok(
                FwpmProviderAdd0(engine.0, &provider, std::ptr::null_mut()),
                "FwpmProviderAdd0",
                &[FWP_E_ALREADY_EXISTS],
            )?;
            let mut sublayer: FWPM_SUBLAYER0 = std::mem::zeroed();
            sublayer.subLayerKey = sublayer_key;
            sublayer.displayData = provider.displayData;
            sublayer.flags = FWPM_SUBLAYER_FLAG_PERSISTENT;
            sublayer.providerKey = std::ptr::from_ref(&provider_key).cast_mut();
            sublayer.weight = SUBLAYER_WEIGHT;
            fwp_ok(
                FwpmSubLayerAdd0(engine.0, &sublayer, std::ptr::null_mut()),
                "FwpmSubLayerAdd0",
                &[FWP_E_ALREADY_EXISTS],
            )?;
            let mut user_blob = FWP_BYTE_BLOB {
                size: user_sd_len,
                data: user_sd.0.cast(),
            };
            let mut range = FWP_RANGE0 {
                valueLow: FWP_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_VALUE0_0 { uint16: ports.0 },
                },
                valueHigh: FWP_VALUE0 {
                    r#type: FWP_UINT16,
                    Anonymous: FWP_VALUE0_0 { uint16: ports.1 },
                },
            };
            for spec in FILTERS {
                let key = guid(spec.key);
                fwp_ok(
                    FwpmFilterDeleteByKey0(engine.0, &key),
                    "FwpmFilterDeleteByKey0",
                    &[FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND],
                )?;
                let mut conditions: Vec<FWPM_FILTER_CONDITION0> = spec
                    .conditions
                    .iter()
                    .map(|c| condition(*c, &mut user_blob, &mut range))
                    .collect();
                let filter_name = wide(spec.name);
                let mut filter: FWPM_FILTER0 = std::mem::zeroed();
                filter.filterKey = key;
                filter.displayData = FWPM_DISPLAY_DATA0 {
                    name: filter_name.as_ptr().cast_mut(),
                    description: description.as_ptr().cast_mut(),
                };
                filter.flags = FWPM_FILTER_FLAG_PERSISTENT;
                filter.providerKey = std::ptr::from_ref(&provider_key).cast_mut();
                filter.layerKey = layer_key(spec.layer);
                filter.subLayerKey = sublayer_key;
                filter.weight = FWP_VALUE0 {
                    r#type: FWP_UINT8,
                    Anonymous: FWP_VALUE0_0 { uint8: spec.weight },
                };
                filter.numFilterConditions = conditions.len() as u32;
                filter.filterCondition = conditions.as_mut_ptr();
                filter.action = FWPM_ACTION0 {
                    r#type: if spec.permit {
                        FWP_ACTION_PERMIT
                    } else {
                        FWP_ACTION_BLOCK
                    },
                    Anonymous: FWPM_ACTION0_0 {
                        filterType: guid(0),
                    },
                };
                filter.Anonymous = FWPM_FILTER0_0 { rawContext: 0 };
                let mut id = 0u64;
                fwp_ok(
                    FwpmFilterAdd0(engine.0, &filter, std::ptr::null_mut(), &mut id),
                    &format!("FwpmFilterAdd0({})", spec.name),
                    &[],
                )?;
            }
        }
        Ok(())
    })
}

/// One filter condition. `user` and `range` must outlive the filter's add.
fn condition(c: Cond, user: &mut FWP_BYTE_BLOB, range: &mut FWP_RANGE0) -> FWPM_FILTER_CONDITION0 {
    let (field, match_type, value) = match c {
        Cond::User => (
            FWPM_CONDITION_ALE_USER_ID,
            FWP_MATCH_EQUAL,
            FWP_CONDITION_VALUE0 {
                r#type: FWP_SECURITY_DESCRIPTOR_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    sd: std::ptr::from_mut(user),
                },
            },
        ),
        Cond::Protocol(p) => (
            FWPM_CONDITION_IP_PROTOCOL,
            FWP_MATCH_EQUAL,
            FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT8,
                Anonymous: FWP_CONDITION_VALUE0_0 { uint8: p },
            },
        ),
        // WFP compares IPv4 addresses in host byte order.
        Cond::RemoteLoopbackV4 => (
            FWPM_CONDITION_IP_REMOTE_ADDRESS,
            FWP_MATCH_EQUAL,
            FWP_CONDITION_VALUE0 {
                r#type: FWP_UINT32,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    uint32: u32::from(std::net::Ipv4Addr::LOCALHOST),
                },
            },
        ),
        Cond::ProxyPorts => (
            FWPM_CONDITION_IP_REMOTE_PORT,
            FWP_MATCH_RANGE,
            FWP_CONDITION_VALUE0 {
                r#type: FWP_RANGE_TYPE,
                Anonymous: FWP_CONDITION_VALUE0_0 {
                    rangeValue: std::ptr::from_mut(range),
                },
            },
        ),
    };
    FWPM_FILTER_CONDITION0 {
        fieldKey: field,
        matchType: match_type,
        conditionValue: value,
    }
}

fn remove_filters() -> Result<(), String> {
    let engine = Engine::open(30_000)?;
    engine.transaction(|| {
        // SAFETY: an open engine; keys that live across each call.
        unsafe {
            for spec in FILTERS {
                fwp_ok(
                    FwpmFilterDeleteByKey0(engine.0, &guid(spec.key)),
                    "FwpmFilterDeleteByKey0",
                    &[FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND],
                )?;
            }
            fwp_ok(
                FwpmSubLayerDeleteByKey0(engine.0, &guid(SUBLAYER_KEY)),
                "FwpmSubLayerDeleteByKey0",
                &[FWP_E_SUBLAYER_NOT_FOUND, FWP_E_NOT_FOUND],
            )?;
            fwp_ok(
                FwpmProviderDeleteByKey0(engine.0, &guid(PROVIDER_KEY)),
                "FwpmProviderDeleteByKey0",
                &[FWP_E_PROVIDER_NOT_FOUND, FWP_E_NOT_FOUND],
            )?;
        }
        Ok(())
    })
}
