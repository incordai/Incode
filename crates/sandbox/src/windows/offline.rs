//! Layer 2: network `deny` (and `proxy`) on native Windows, by running the
//! command as a local account the firewall cuts off.
//!
//! No token restricts the network on Windows (see [`super`]), but the Windows
//! Filtering Platform can match a connection on the *user* that makes it
//! (`FWPM_CONDITION_ALE_USER_ID`). So, as OpenAI Codex's Windows sandbox does
//! (`codex-rs/windows-sandbox-rs`, Apache-2.0: `setup_provisioning/
//! sandbox_users.rs`, `setup_provisioning/firewall.rs`, `wfp.rs`, `dpapi.rs`,
//! `elevated/runner_client.rs`), a one-time elevated setup
//! (`sbx setup-windows`):
//!
//! 1. creates a local account, [`ACCOUNT`], member of `Users` only (it needs
//!    `Users` to log on locally and to read `C:\Windows` and `Program Files`;
//!    Codex adds its accounts to `Users` too), with a random password nobody
//!    is shown, hidden from the sign-in screen;
//! 2. stores that password DPAPI-protected (machine scope) in a record under
//!    `%ProgramData%\sbx`, readable by SYSTEM, Administrators and the user who
//!    ran the setup only ([`state_dir_sddl`]);
//! 3. adds persistent WFP filters ([`FILTERS`]) that block every connection
//!    the account makes or accepts, TCP, UDP and ICMP, loopback included, and
//!    permit one thing: TCP to `127.0.0.1` on a small fixed range of ports
//!    ([`DEFAULT_PROXY_PORTS`]), where a `proxy` run's allowlisting proxy
//!    listens.
//!
//! At run time, unelevated, the helper starts a second helper as that account
//! with `CreateProcessWithLogonW` (the one way an unprivileged process can
//! start a process as another user: `CreateProcessAsUserW` with a logged-on
//! token needs `SeAssignPrimaryTokenPrivilege`, and `CreateProcessWithTokenW`
//! needs `SeImpersonatePrivilege`, neither of which a standard user holds;
//! Codex starts its command runner the same way). That second helper derives
//! the write-restricted token from its own token, exactly as layer 1 does,
//! and starts the command; the job object, made by the first helper, holds
//! them both.
//!
//! What differs from layer 1, and why:
//!
//! - **Reads.** The command reads what the offline account may read, not
//!   what the user may: system folders, and what each run grants it (the
//!   writable roots, the current folder, the helper program, the private
//!   temp directory). A toolchain under the user's profile must be granted
//!   once with `sbx setup-windows --read <DIR>`. Narrower, never wider.
//! - **Writes** need both the account (the token's normal check) and the
//!   run's SID (its restricted check): each root gets an ACE for each. The
//!   account's ACE is shared by concurrent runs, so it is only revoked when
//!   no other live run still needs it ([`still_needed`]).
//! - **No profile is loaded** (`dwLogonFlags = 0`, as Codex's runner does):
//!   loading it would create `C:\Users\sbx-offline` on first use, a home every
//!   run shares and could leave things in for the next. The command gets the
//!   run's computed environment instead.
//!
//! Not stopped by the filters (said in the README): name lookups through the
//! DNS Client service, which resolves on the command's behalf as
//! `NetworkService` (as on Linux with `systemd-resolved`), and SMB/WebDAV
//! paths (`\\host\share`), which the kernel redirector opens as SYSTEM.

// The pure parts are compiled everywhere so their tests run on Linux; only
// the Windows build uses them.
#![cfg_attr(not(windows), allow(dead_code))]

use std::path::{Path, PathBuf};

use super::path_key;
use crate::policy::NetMode;
use crate::Status;

/// The local account commands run as when the network is denied.
pub const ACCOUNT: &str = "sbx-offline";

/// The account's description, shown in Computer Management.
pub const ACCOUNT_COMMENT: &str =
    "sbx: runs sandboxed commands with no network (made by sbx setup-windows)";

/// The loopback ports a `proxy` run's proxy may listen on, one per
/// concurrent run. Below Windows' dynamic range (49152-65535), so the system
/// never hands one out as an ephemeral port.
pub const DEFAULT_PROXY_PORTS: (u16, u16) = (47_600, 47_615);

/// The folder under `%ProgramData%` that holds the record.
pub const STATE_DIR: &str = "sbx";
/// The record's file name.
pub const RECORD_FILE: &str = "offline.conf";

/// DPAPI's optional entropy: the blob is useless to anything that does not
/// also know this, which keeps a generic "decrypt every machine blob" tool
/// from reading it in passing. The file's ACL is the protection.
pub const DPAPI_ENTROPY: &[u8] = b"sbx-offline-v1";

const RECORD_MAGIC: &str = "sbx-offline-record";
const RECORD_VERSION: u32 = 1;

/// What `sbx setup-windows` wrote down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The account name ([`ACCOUNT`]).
    pub account: String,
    /// The account's SID, to check it is still the same account.
    pub sid: String,
    /// The SID of the user who ran the setup: the one user granted read on
    /// the record (beside SYSTEM and Administrators).
    pub owner: String,
    /// The loopback port range the proxy filter permits.
    pub proxy_ports: (u16, u16),
    /// Folders granted to the account for reading at setup (`--read`).
    pub reads: Vec<PathBuf>,
    /// The password, DPAPI-protected (machine scope, [`DPAPI_ENTROPY`]).
    pub password: Vec<u8>,
}

impl Record {
    /// The record as text: one `key value` per line.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = format!("{RECORD_MAGIC} {RECORD_VERSION}\n");
        text.push_str(&format!("account {}\n", self.account));
        text.push_str(&format!("sid {}\n", self.sid));
        text.push_str(&format!("owner {}\n", self.owner));
        text.push_str(&format!(
            "proxy-ports {}-{}\n",
            self.proxy_ports.0, self.proxy_ports.1
        ));
        for r in &self.reads {
            text.push_str(&format!("read {}\n", r.display()));
        }
        text.push_str(&format!("password {}\n", hex(&self.password)));
        text
    }

    /// A record read back.
    ///
    /// # Errors
    /// When the first line is not this format's (or another version's), or a
    /// field is missing or malformed.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let head = lines.next().unwrap_or("");
        match head.split_once(' ') {
            Some((RECORD_MAGIC, v)) if v.trim() == RECORD_VERSION.to_string() => {}
            Some((RECORD_MAGIC, v)) => {
                return Err(format!(
                    "the record is version {v}; this sbx reads version {RECORD_VERSION}"
                ))
            }
            _ => return Err("not an sbx offline record".to_string()),
        }
        let (mut account, mut sid, mut owner, mut ports, mut password) =
            (None, None, None, None, None);
        let mut reads = Vec::new();
        for line in lines {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "account" => account = Some(value.to_string()),
                "sid" => sid = Some(value.to_string()),
                "owner" => owner = Some(value.to_string()),
                "proxy-ports" => ports = Some(parse_port_range(value)?),
                "read" => reads.push(PathBuf::from(value)),
                "password" => {
                    password = Some(unhex(value).ok_or("the record's password is not hex")?);
                }
                _ => {}
            }
        }
        let sid = sid.filter(|s| is_sid(s)).ok_or("the record has no SID")?;
        let owner = owner
            .filter(|s| is_sid(s))
            .ok_or("the record has no owner")?;
        Ok(Self {
            account: account
                .filter(|a| !a.is_empty())
                .ok_or("the record has no account")?,
            sid,
            owner,
            proxy_ports: ports.ok_or("the record has no proxy ports")?,
            reads,
            password: password
                .filter(|p| !p.is_empty())
                .ok_or("the record has no password")?,
        })
    }
}

/// Whether `s` looks like a SID string (`S-1-` and numbers).
#[must_use]
pub fn is_sid(s: &str) -> bool {
    s.strip_prefix("S-1-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('-')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `low-high` (or one port), both non-zero, low <= high.
///
/// # Errors
/// When it is not that.
pub fn parse_port_range(text: &str) -> Result<(u16, u16), String> {
    let text = text.trim();
    let (lo, hi) = text.split_once('-').unwrap_or((text, text));
    let port = |p: &str| {
        p.trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or_else(|| format!("`{text}` is not a port range (like 47600-47615)"))
    };
    let (lo, hi) = (port(lo)?, port(hi)?);
    if lo > hi {
        return Err(format!("`{text}`: the range is backwards"));
    }
    Ok((lo, hi))
}

// ---- The password. ----

/// The account's password from 32 random bytes: a fixed prefix that meets
/// any complexity policy (upper, lower, digit, symbol), then the bytes in
/// URL-safe base64 (43 characters, 256 bits). Never printed or logged.
#[must_use]
pub fn password_from(random: &[u8; 32]) -> String {
    let mut pw = String::from("Sx9!");
    pw.push_str(&base64url(random));
    pw
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// Whether a password has all four kinds of character a Windows complexity
/// policy counts, and is long enough for any length policy.
#[must_use]
pub fn meets_complexity(pw: &str) -> bool {
    pw.len() >= 32
        && pw.bytes().any(|b| b.is_ascii_uppercase())
        && pw.bytes().any(|b| b.is_ascii_lowercase())
        && pw.bytes().any(|b| b.is_ascii_digit())
        && pw.bytes().any(|b| !b.is_ascii_alphanumeric())
}

// ---- Security descriptors. ----

/// The WFP user condition: a security descriptor granting
/// `FWP_ACTRL_MATCH_FILTER` (`CC`, 0x1) to the account, so a filter matches
/// every socket a process of that account opens (Codex's
/// `LocalUserAuthorizedList` uses the same SDDL).
#[must_use]
pub fn user_condition_sddl(account_sid: &str) -> String {
    format!("O:LSD:(A;;CC;;;{account_sid})")
}

/// The state folder and the record in it: SYSTEM and Administrators full
/// control, the user who ran the setup read, nobody else (in particular not
/// `Users`, which the offline account is in). Protected, so nothing is
/// inherited from `%ProgramData%`, whose default lets `Users` create files.
#[must_use]
pub fn state_dir_sddl(owner_sid: &str) -> String {
    format!("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FR;;;{owner_sid})")
}

/// The DACL the second helper (running as the account, unrestricted) puts
/// on its own process: SYSTEM everything, and the owner, that is the
/// account and so the confined command, only `SYNCHRONIZE` and
/// `PROCESS_QUERY_LIMITED_INFORMATION` (`OW` removes the owner's implicit
/// `WRITE_DAC`). The command cannot open its unrestricted parent to write
/// to it.
pub const HELPER_PROCESS_SDDL: &str = "D:P(A;;GA;;;SY)(A;;0x101000;;;OW)";

// ---- The filters. ----

/// Where a filter sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `FWPM_LAYER_ALE_AUTH_CONNECT_V4`: an outbound TCP connect, and the
    /// first packet a UDP or raw socket sends to each address.
    ConnectV4,
    ConnectV6,
    /// `FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4`: an inbound connection accepted,
    /// or the first packet received from an address.
    AcceptV4,
    AcceptV6,
    /// `FWPM_LAYER_ALE_RESOURCE_ASSIGNMENT_V4`: a socket bound.
    AssignV4,
    AssignV6,
}

/// One condition of a filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cond {
    /// The socket's user is the offline account ([`user_condition_sddl`]).
    User,
    /// `FWPM_CONDITION_IP_PROTOCOL`.
    Protocol(u8),
    /// The remote address is `127.0.0.1`.
    RemoteLoopbackV4,
    /// The remote port is within the record's proxy ports.
    ProxyPorts,
}

/// One persistent WFP filter.
#[derive(Debug, Clone, Copy)]
pub struct FilterSpec {
    /// Its fixed key: setup deletes and re-adds by key, so running it twice
    /// leaves one copy.
    pub key: u128,
    pub name: &'static str,
    pub layer: Layer,
    /// Permit (else block).
    pub permit: bool,
    /// Its weight in the sublayer (0-15): the highest matching filter
    /// decides, so the proxy permit (15) outranks the blocks (1).
    pub weight: u8,
    pub conditions: &'static [Cond],
}

/// The provider every filter belongs to.
pub const PROVIDER_KEY: u128 = 0x61c2_aed0_e653_4e33_a551_74e6_700c_227e;
/// The sublayer they sit in.
pub const SUBLAYER_KEY: u128 = 0x9de6_5a21_0676_4855_b78a_8f1e_11fe_faa7;
/// The sublayer's weight: above the default sublayers', so nothing a
/// lower-priority sublayer permits overrides these blocks.
pub const SUBLAYER_WEIGHT: u16 = 0xfff0;

const IPPROTO_ICMP: u8 = 1;
const IPPROTO_TCP: u8 = 6;
const IPPROTO_ICMPV6: u8 = 58;

/// Every filter setup adds. The first is the one whose presence says the
/// setup is in place ([`MAIN_FILTER`]).
pub const FILTERS: &[FilterSpec] = &[
    FilterSpec {
        key: 0x41ae_b76d_b1c6_46e2_a8ec_2773_5b76_7fb3,
        name: "sbx_offline_block_connect_v4",
        layer: Layer::ConnectV4,
        permit: false,
        weight: 1,
        conditions: &[Cond::User],
    },
    FilterSpec {
        key: 0x1933_8d8b_ba7f_4fbd_aa2d_ac85_c850_0a44,
        name: "sbx_offline_block_connect_v6",
        layer: Layer::ConnectV6,
        permit: false,
        weight: 1,
        conditions: &[Cond::User],
    },
    FilterSpec {
        key: 0x992a_71cd_6ae4_49d0_a019_245c_7fe3_cc39,
        name: "sbx_offline_block_accept_v4",
        layer: Layer::AcceptV4,
        permit: false,
        weight: 1,
        conditions: &[Cond::User],
    },
    FilterSpec {
        key: 0xf187_fa38_db37_4fd8_874d_6df8_c8ef_6b78,
        name: "sbx_offline_block_accept_v6",
        layer: Layer::AcceptV6,
        permit: false,
        weight: 1,
        conditions: &[Cond::User],
    },
    // Raw ICMP sockets refused at bind (as Codex does); their sends are
    // blocked at connect anyway.
    FilterSpec {
        key: 0x9124_0db3_5eea_4a0d_be98_364b_aa57_e626,
        name: "sbx_offline_block_icmp_v4",
        layer: Layer::AssignV4,
        permit: false,
        weight: 1,
        conditions: &[Cond::User, Cond::Protocol(IPPROTO_ICMP)],
    },
    FilterSpec {
        key: 0xb436_7d8e_cbdc_4ded_8b82_d6e4_350b_daee,
        name: "sbx_offline_block_icmp_v6",
        layer: Layer::AssignV6,
        permit: false,
        weight: 1,
        conditions: &[Cond::User, Cond::Protocol(IPPROTO_ICMPV6)],
    },
    FilterSpec {
        key: 0x4deb_7adf_bcc1_442f_aad6_b1b7_d307_a302,
        name: "sbx_offline_permit_proxy_v4",
        layer: Layer::ConnectV4,
        permit: true,
        weight: 15,
        conditions: &[
            Cond::User,
            Cond::Protocol(IPPROTO_TCP),
            Cond::RemoteLoopbackV4,
            Cond::ProxyPorts,
        ],
    },
];

/// The filter whose presence says the setup is in place.
pub const MAIN_FILTER: &FilterSpec = &FILTERS[0];

// ---- `sbx setup-windows`'s arguments. ----

/// What `sbx setup-windows` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupArgs {
    /// `--remove`: undo everything.
    pub remove: bool,
    /// `--read <DIR>`: folders the account may read (toolchains).
    pub reads: Vec<PathBuf>,
    /// `--proxy-ports <LO-HI>`.
    pub proxy_ports: Option<(u16, u16)>,
}

/// Parse `sbx setup-windows`'s arguments.
///
/// # Errors
/// For an unknown option, a missing value, or `--remove` with anything else.
pub fn parse_setup_args(args: &[String]) -> Result<SetupArgs, String> {
    let mut out = SetupArgs::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let value = |i: usize| {
            args.get(i + 1)
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        match flag {
            "--remove" => out.remove = true,
            "--read" => {
                out.reads.push(PathBuf::from(value(i)?));
                i += 1;
            }
            "--proxy-ports" => {
                out.proxy_ports = Some(parse_port_range(&value(i)?)?);
                i += 1;
            }
            other => {
                return Err(format!(
                    "sbx setup-windows: unknown option `{other}` (--remove, --read <DIR>, \
                     --proxy-ports <LO-HI>)"
                ))
            }
        }
        i += 1;
    }
    if out.remove && (!out.reads.is_empty() || out.proxy_ports.is_some()) {
        return Err("sbx setup-windows --remove takes no other option".to_string());
    }
    Ok(out)
}

// ---- What the run can count on. ----

/// The setup, found in place and usable by this user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The account's SID.
    pub sid: String,
    /// The loopback ports the proxy may listen on.
    pub proxy_ports: (u16, u16),
    /// Where the record is, for the helper to read the password from.
    pub record: PathBuf,
}

/// How to get layer 2, for messages.
pub const SETUP_HINT: &str = "run `sbx setup-windows` as administrator once (it creates the \
     offline account sbx-offline and its firewall filters), or use WSL2 (SBX_BACKEND=wsl, or auto)";

/// What native Windows enforces of the network policy, given whether the
/// setup is in place (`Err` says why not).
#[must_use]
pub fn net_status(mode: NetMode, setup: &Result<Ready, String>) -> Status {
    let reads = "the command runs as the local account sbx-offline and so reads only what that \
                 account may: system folders, the run's roots, current folder and temp \
                 directory, and folders granted with `sbx setup-windows --read`";
    match (mode, setup) {
        (NetMode::Allow, _) => Status::NotRequested,
        (NetMode::Deny, Ok(_)) => Status::Enforced(format!(
            "Windows Filtering Platform: every connection the offline account makes or accepts \
             is blocked (TCP, UDP, ICMP, loopback included); {reads}"
        )),
        (NetMode::Proxy, Ok(r)) => Status::Enforced(format!(
            "Windows Filtering Platform: the offline account may only open TCP to 127.0.0.1 \
             ports {}-{}, where the run's allowlisting proxy listens; {reads}",
            r.proxy_ports.0, r.proxy_ports.1
        )),
        (_, Err(why)) => Status::NotEnforced(format!(
            "network {} is not enforced natively on Windows: {why}; {SETUP_HINT}",
            mode.as_str()
        )),
    }
}

// ---- The account's share of a run's grants. ----

/// How much access the account still needs on a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Need {
    None,
    Read,
    Modify,
}

/// What other live runs still need of `path`: the account's ACE on it is
/// one ACE shared by every run that granted it (a grant merges into it), so a
/// run that ends may revoke it only when no other live run still lists the
/// path, and must leave read access when another only reads it. `others` is
/// each other live run's (modify roots, read paths).
#[must_use]
pub fn still_needed(path: &Path, others: &[(Vec<PathBuf>, Vec<PathBuf>)]) -> Need {
    let key = path_key(&path.to_string_lossy());
    let same = |p: &PathBuf| path_key(&p.to_string_lossy()) == key;
    let mut need = Need::None;
    for (modify, read) in others {
        if modify.iter().any(same) {
            return Need::Modify;
        }
        if read.iter().any(same) {
            need = Need::Read;
        }
    }
    need
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> Record {
        Record {
            account: ACCOUNT.into(),
            sid: "S-1-5-21-1-2-3-1009".into(),
            owner: "S-1-5-21-1-2-3-1001".into(),
            proxy_ports: DEFAULT_PROXY_PORTS,
            reads: vec![
                PathBuf::from(r"C:\Users\me\.cargo"),
                PathBuf::from(r"D:\tools x"),
            ],
            password: vec![0, 1, 0xab, 0xff],
        }
    }

    #[test]
    fn a_record_round_trips() {
        let r = record();
        let text = r.to_text();
        assert!(text.starts_with("sbx-offline-record 1\n"), "{text}");
        assert!(text.contains("password 0001abff\n"), "{text}");
        assert!(text.contains("proxy-ports 47600-47615\n"), "{text}");
        assert_eq!(Record::parse(&text), Ok(r));
        // CRLF, as a file edited on Windows may come back.
        assert_eq!(
            Record::parse(&record().to_text().replace('\n', "\r\n")),
            Ok(record())
        );
    }

    #[test]
    fn a_bad_record_is_refused_and_says_why() {
        assert!(Record::parse("").is_err());
        assert!(Record::parse("something else\n").is_err());
        let v2 = record().to_text().replace("record 1", "record 2");
        assert!(Record::parse(&v2).is_err_and(|e| e.contains("version 2")));
        for (from, to) in [
            ("sid S-1-5-21-1-2-3-1009", "sid nobody"),
            ("owner S-1-5-21-1-2-3-1001", "owner "),
            ("password 0001abff", "password xyz"),
            ("password 0001abff", "password abc"),
            ("proxy-ports 47600-47615", "proxy-ports 9-1"),
            ("account sbx-offline", "account "),
        ] {
            let bad = record().to_text().replace(from, to);
            assert!(Record::parse(&bad).is_err(), "{to}");
        }
    }

    #[test]
    fn sids_are_recognised() {
        assert!(is_sid("S-1-5-21-1-2-3-1001"));
        assert!(is_sid("S-1-1-0"));
        for bad in ["", "S-1-", "S-1-5-", "S-1-5-x", "s-1-5", "S-1-5--1"] {
            assert!(!is_sid(bad), "{bad}");
        }
    }

    #[test]
    fn port_ranges_parse() {
        assert_eq!(parse_port_range("47600-47615"), Ok((47_600, 47_615)));
        assert_eq!(parse_port_range(" 5000 "), Ok((5000, 5000)));
        for bad in ["", "0-5", "5-4", "a-b", "70000", "1-"] {
            assert!(parse_port_range(bad).is_err(), "{bad}");
        }
        // Below the dynamic range, so never an ephemeral port.
        assert!(DEFAULT_PROXY_PORTS.1 < 49_152);
    }

    #[test]
    fn the_password_is_long_complex_and_carries_every_byte() {
        let a = password_from(&[0u8; 32]);
        let mut bytes = [0u8; 32];
        bytes[31] = 1;
        let b = password_from(&bytes);
        assert_ne!(a, b);
        for pw in [&a, &b, &password_from(&[0xff; 32])] {
            assert!(meets_complexity(pw), "{pw}");
            assert_eq!(pw.len(), 4 + 43, "{pw}");
            assert!(pw.bytes().all(|c| c.is_ascii_graphic()), "{pw}");
        }
        assert_eq!(base64url(b"\xfb\xff"), "-_8");
        assert_eq!(base64url(b"Man"), "TWFu");
        assert!(!meets_complexity("short1A!"));
        assert!(!meets_complexity(&"a".repeat(40)));
    }

    #[test]
    fn security_descriptors_name_who_they_should() {
        assert_eq!(
            user_condition_sddl("S-1-5-21-9-1009"),
            "O:LSD:(A;;CC;;;S-1-5-21-9-1009)"
        );
        let dir = state_dir_sddl("S-1-5-21-9-1001");
        assert!(dir.starts_with("O:BAD:P("), "protected: {dir}");
        assert!(dir.contains("(A;OICI;FR;;;S-1-5-21-9-1001)"), "{dir}");
        assert!(!dir.contains(";BU)") && !dir.contains(";WD)") && !dir.contains(";AU)"));
        assert!(HELPER_PROCESS_SDDL.contains(";OW)"));
    }

    #[test]
    fn filters_are_unique_named_and_all_scoped_to_the_account() {
        let mut keys: Vec<u128> = FILTERS.iter().map(|f| f.key).collect();
        keys.extend([PROVIDER_KEY, SUBLAYER_KEY]);
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "keys are unique");
        let mut names: Vec<&str> = FILTERS.iter().map(|f| f.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), FILTERS.len());
        for f in FILTERS {
            assert!(f.name.starts_with("sbx_offline_"), "{}", f.name);
            assert!(f.conditions.contains(&Cond::User), "{} is per-user", f.name);
            assert!(f.weight <= 15);
        }
        // Every direction is blocked, both IP versions.
        for layer in [
            Layer::ConnectV4,
            Layer::ConnectV6,
            Layer::AcceptV4,
            Layer::AcceptV6,
        ] {
            assert!(
                FILTERS
                    .iter()
                    .any(|f| f.layer == layer && !f.permit && f.conditions == [Cond::User]),
                "{layer:?}"
            );
        }
        // The one permit is TCP to the loopback proxy ports, and outranks
        // the blocks.
        let permits: Vec<_> = FILTERS.iter().filter(|f| f.permit).collect();
        assert_eq!(permits.len(), 1);
        assert!(permits[0].conditions.contains(&Cond::RemoteLoopbackV4));
        assert!(permits[0].conditions.contains(&Cond::ProxyPorts));
        assert!(permits[0].conditions.contains(&Cond::Protocol(IPPROTO_TCP)));
        assert!(FILTERS
            .iter()
            .all(|f| f.permit || f.weight < permits[0].weight));
        assert!(!MAIN_FILTER.permit);
    }

    #[test]
    fn setup_arguments_parse() {
        let s =
            |v: &[&str]| parse_setup_args(&v.iter().map(|a| (*a).to_string()).collect::<Vec<_>>());
        assert_eq!(s(&[]), Ok(SetupArgs::default()));
        assert_eq!(
            s(&[
                "--read",
                r"C:\a",
                "--read",
                r"C:\b c",
                "--proxy-ports",
                "5000-5003"
            ]),
            Ok(SetupArgs {
                remove: false,
                reads: vec![PathBuf::from(r"C:\a"), PathBuf::from(r"C:\b c")],
                proxy_ports: Some((5000, 5003)),
            })
        );
        assert_eq!(s(&["--remove"]).map(|a| a.remove), Ok(true));
        assert!(s(&["--remove", "--read", "x"]).is_err());
        assert!(s(&["--read"]).is_err());
        assert!(s(&["--proxy-ports", "x"]).is_err());
        assert!(s(&["--force"]).is_err());
    }

    #[test]
    fn the_status_says_what_is_enforced_and_how_to_get_it() {
        let ready = Ok(Ready {
            sid: "S-1-5-21-1-1009".into(),
            proxy_ports: (5000, 5003),
            record: PathBuf::from(r"C:\ProgramData\sbx\offline.conf"),
        });
        assert_eq!(net_status(NetMode::Allow, &ready), Status::NotRequested);
        assert!(
            matches!(net_status(NetMode::Deny, &ready), Status::Enforced(s) if s.contains("sbx-offline") && s.contains("loopback"))
        );
        assert!(
            matches!(net_status(NetMode::Proxy, &ready), Status::Enforced(s) if s.contains("5000-5003"))
        );
        let missing = Err("the offline account is not set up".to_string());
        for mode in [NetMode::Deny, NetMode::Proxy] {
            let st = net_status(mode, &missing);
            assert!(
                matches!(&st, Status::NotEnforced(s) if s.contains("sbx setup-windows` as administrator") && s.contains("not set up")),
                "{st:?}"
            );
        }
        assert_eq!(net_status(NetMode::Allow, &missing), Status::NotRequested);
    }

    #[test]
    fn a_shared_ace_stays_while_another_run_needs_it() {
        let ws = PathBuf::from(r"C:\w\proj");
        let none: Vec<(Vec<PathBuf>, Vec<PathBuf>)> = vec![];
        assert_eq!(still_needed(&ws, &none), Need::None);
        let reader = (vec![], vec![PathBuf::from(r"c:\W\Proj\")]);
        assert_eq!(still_needed(&ws, std::slice::from_ref(&reader)), Need::Read);
        let writer = (vec![PathBuf::from(r"\\?\C:\w\proj")], vec![]);
        assert_eq!(still_needed(&ws, &[reader, writer]), Need::Modify);
        // A different path, even a parent or child, is a different ACE.
        let parent = (
            vec![PathBuf::from(r"C:\w")],
            vec![PathBuf::from(r"C:\w\proj\x")],
        );
        assert_eq!(still_needed(&ws, &[parent]), Need::None);
    }
}
