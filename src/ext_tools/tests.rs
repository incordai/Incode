//! External tools: the entry, the archive guards, and an install end to end
//! against a release served on loopback.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;

/// The program a test release carries, as the archive names it.
fn program_file() -> &'static str {
    if cfg!(windows) {
        "tool.exe"
    } else {
        "tool"
    }
}

/// A zip of `entries`, stored.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o755);
    for (name, body) in entries {
        writer.start_file(*name, options).expect("entry");
        writer.write_all(body).expect("body");
    }
    writer.finish().expect("finish").into_inner()
}

/// A tar.gz of `entries`, each name written into the header as given — so a
/// test can write a name `tar`'s own builder would refuse.
fn tgz_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    for (name, body) in entries {
        let mut header = tar::Header::new_gnu();
        let raw = &mut header.as_old_mut().name;
        raw[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, *body).expect("append");
    }
    builder.into_inner().expect("tar").finish().expect("gzip")
}

/// The archive this platform's releases ship, holding `entries`.
fn archive_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    match Target::current().map(Target::ext) {
        Some("zip") => zip_of(entries),
        _ => tgz_of(entries),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn entry(name: &str) -> ToolEntry {
    ToolEntry {
        name: name.to_string(),
        release: Some("owner/repo".to_string()),
        asset: Some("tool-{arch}-{os}.{ext}".to_string()),
        run: vec!["tool".to_string(), "mcp".to_string()],
        provides: Some("browser".to_string()),
        ..ToolEntry::default()
    }
}

/// **AN ASSET IS NAMED FOR THIS MACHINE**: `{os}`, `{arch}`, `{ext}` and
/// `{version}` filled in; an `assets` entry for the target wins.
#[test]
fn an_asset_name_is_expanded_for_the_target() {
    let windows = Target {
        os: "windows",
        arch: "x86_64",
    };
    let mac = Target {
        os: "macos",
        arch: "aarch64",
    };
    let mut tool = entry("browser");
    tool.asset = Some("obscura-{arch}-{os}.{ext}".to_string());
    assert_eq!(
        tool.asset_for(windows, "v1").expect("named"),
        "obscura-x86_64-windows.zip"
    );
    assert_eq!(
        tool.asset_for(mac, "v1").expect("named"),
        "obscura-aarch64-macos.tar.gz"
    );
    tool.assets.insert(
        "aarch64-macos".to_string(),
        "Obscura-{version}-mac-arm.tgz".to_string(),
    );
    assert_eq!(
        tool.asset_for(mac, "v2").expect("named"),
        "Obscura-v2-mac-arm.tgz"
    );
    assert_eq!(
        tool.asset_for(windows, "v2").expect("named"),
        "obscura-x86_64-windows.zip"
    );
}

/// **AN ENTRY THAT COULD RUN SOMETHING ELSE IS REFUSED**: a `run[0]` that
/// leaves the folder, a name that is a path, a malformed source.
#[test]
fn an_entry_that_could_escape_or_is_incomplete_is_refused() {
    assert!(entry("browser").check().is_ok());
    for program in ["../tool", "/usr/bin/tool", "a/../../tool", ""] {
        let mut bad = entry("browser");
        bad.run = vec![program.to_string()];
        assert!(bad.check().is_err(), "run[0] `{program}` was accepted");
    }
    if cfg!(windows) {
        let mut bad = entry("browser");
        bad.run = vec![r"C:\Windows\System32\cmd.exe".to_string()];
        assert!(bad.check().is_err(), "a drive path was accepted");
    }
    for name in ["", "a/b", "..", "x".repeat(40).as_str()] {
        assert!(entry(name).check().is_err(), "name `{name}` was accepted");
    }
    let mut bad = entry("browser");
    bad.release = Some("owner/repo/extra".to_string());
    assert!(bad.check().is_err());
    bad.release = Some("../repo".to_string());
    assert!(bad.check().is_err());
    let mut both = entry("browser");
    both.url = Some("https://example.test/x.zip".to_string());
    assert!(both.check().is_err());
    let mut neither = entry("browser");
    neither.release = None;
    assert!(neither.check().is_err());
    let mut empty_run = entry("browser");
    empty_run.run.clear();
    assert!(empty_run.check().is_err());
}

/// The admin file's shape: the one in the docs parses, and a pinned sha may
/// be one digest or one per target.
#[test]
fn the_documented_entry_parses() {
    let admin: crate::admin::AdminConfig = serde_json::from_value(json!({
        "tools": [
            { "name": "browser", "release": "incordai/incord-browser", "version": "latest",
              "asset": "obscura-{arch}-{os}.{ext}", "run": ["obscura", "mcp"],
              "provides": "browser" },
            { "name": "other", "url": "https://example.test/{version}/o-{os}.{ext}",
              "version": "1.0", "run": ["o"],
              "sha256": { "x86_64-windows": "AB" } }
        ]
    }))
    .expect("parses");
    assert_eq!(admin.tools.len(), 2);
    assert_eq!(admin.tools[0].provides.as_deref(), Some("browser"));
    let windows = Target {
        os: "windows",
        arch: "x86_64",
    };
    assert_eq!(admin.tools[1].pinned_for(windows).as_deref(), Some("ab"));
    assert_eq!(
        admin.tools[1].asset_for(windows, "1.0").expect("named"),
        "o-windows.zip"
    );
}

/// **A ZIP THAT REACHES OUTSIDE ITS FOLDER IS REFUSED**, and nothing is
/// written outside it — the zip-slip guard.
#[test]
fn a_zip_that_reaches_outside_is_refused_and_writes_nothing_outside() {
    let dir = tempfile::tempdir().expect("dir");
    let into = dir.path().join("into");
    for name in ["../evil.txt", "ok/../../evil.txt", "/evil.txt"] {
        let archive = dir.path().join("a.zip");
        std::fs::write(&archive, zip_of(&[("fine.txt", b"x"), (name, b"evil")])).expect("seed");
        let refused = unpack::unpack(&archive, "a.zip", &into);
        assert!(refused.is_err(), "`{name}` was unpacked");
        assert!(!dir.path().join("evil.txt").exists(), "`{name}` escaped");
    }
}

/// **A TAR.GZ THAT REACHES OUTSIDE, OR HOLDS A LINK, IS REFUSED.**
#[test]
fn a_tar_gz_with_dotdot_or_a_link_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let into = dir.path().join("into");
    let archive = dir.path().join("a.tar.gz");
    std::fs::write(&archive, tgz_of(&[("../evil.txt", b"evil")])).expect("seed");
    assert!(unpack::unpack(&archive, "a.tar.gz", &into).is_err());
    assert!(!dir.path().join("evil.txt").exists());

    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    builder
        .append_link(&mut header, "link", "/etc/passwd")
        .expect("link");
    let bytes = builder.into_inner().expect("tar").finish().expect("gzip");
    std::fs::write(&archive, bytes).expect("seed");
    let said = unpack::unpack(&archive, "a.tar.gz", &into).expect_err("a link");
    assert!(said.contains("link"), "{said}");
}

/// **THE BOUNDS ARE COUNTED AS THE BYTES COME OUT**: too many entries, or
/// more bytes than allowed, stop the unpack.
#[test]
fn an_archive_past_its_bounds_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let archive = dir.path().join("a.zip");
    std::fs::write(&archive, zip_of(&[("a", b"12345"), ("b", b"12345")])).expect("seed");
    let many = unpack::unpack_within(&archive, "a.zip", &dir.path().join("1"), 1, 1 << 20);
    assert!(many.expect_err("too many").contains("entries"));
    let big = unpack::unpack_within(&archive, "a.zip", &dir.path().join("2"), 10, 6);
    assert!(big.expect_err("too big").contains("bytes"));
    assert!(unpack::unpack_within(&archive, "a.zip", &dir.path().join("3"), 10, 10).is_ok());
}

/// A plain archive unpacks, folders and all; a tar.gz keeps an executable
/// bit on Unix.
#[test]
fn a_plain_archive_unpacks() {
    let dir = tempfile::tempdir().expect("dir");
    for (asset, bytes) in [
        ("a.zip", zip_of(&[("bin/tool", b"#!x"), ("README", b"r")])),
        (
            "a.tar.gz",
            tgz_of(&[("bin/tool", b"#!x"), ("README", b"r")]),
        ),
    ] {
        let archive = dir.path().join(asset);
        std::fs::write(&archive, bytes).expect("seed");
        let into = dir.path().join(format!("out-{asset}"));
        unpack::unpack(&archive, asset, &into).expect("unpacks");
        assert_eq!(std::fs::read(into.join("bin/tool")).expect("tool"), b"#!x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(into.join("bin/tool"))
                .expect("meta")
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "{asset}: {mode:o}");
        }
    }
    let other = dir.path().join("a.rar");
    std::fs::write(&other, b"x").expect("seed");
    assert!(unpack::unpack(&other, "a.rar", &dir.path().join("rar")).is_err());
}

/// A loopback server standing in for GitHub: the API's release document and
/// the asset itself, and every path it was asked for.
struct FakeRelease {
    api: String,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl FakeRelease {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let routes: Arc<Mutex<HashMap<String, Vec<u8>>>> = Arc::default();
        let asked: Arc<Mutex<Vec<String>>> = Arc::default();
        let (served, log) = (Arc::clone(&routes), Arc::clone(&asked));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (served, log) = (Arc::clone(&served), Arc::clone(&log));
                std::thread::spawn(move || {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&request).into_owned();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                    log.lock().expect("log").push(path.clone());
                    let body = served.lock().expect("routes").get(&path).cloned();
                    let reply = match body {
                        Some(body) => {
                            let mut out = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            out.extend_from_slice(&body);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec(),
                    };
                    let _ = stream.write_all(&reply);
                });
            }
        });
        Self {
            api: format!("http://127.0.0.1:{port}"),
            routes,
            asked,
        }
    }

    /// Publish release `tag` of `owner/repo` with one asset: `served` is what
    /// is downloaded, `digest` what the release says its sha256 is.
    fn publish(&self, tag: &str, asset: &str, served: Vec<u8>, digest: Option<&str>) {
        let download = format!("/download/{tag}/{asset}");
        let release = json!({
            "tag_name": tag,
            "assets": [{
                "name": asset,
                "size": served.len(),
                "digest": digest.map(|d| format!("sha256:{d}")),
                "browser_download_url": format!("{}{download}", self.api),
            }]
        });
        let mut routes = self.routes.lock().expect("routes");
        routes.insert(
            "/repos/owner/repo/releases/latest".to_string(),
            release.to_string().into_bytes(),
        );
        routes.insert(
            format!("/repos/owner/repo/releases/tags/{tag}"),
            release.to_string().into_bytes(),
        );
        routes.insert(download, served);
    }

    fn downloads(&self) -> usize {
        self.asked
            .lock()
            .expect("log")
            .iter()
            .filter(|p| p.starts_with("/download/"))
            .count()
    }
}

/// The asset name a test release uses on this machine.
fn asset_here() -> String {
    let target = Target::current().expect("a supported platform");
    target.expand("tool-{arch}-{os}.{ext}", "")
}

/// **AN INSTALL IS CHECKED, UNPACKED, RECORDED AND RUN FROM ITS OWN
/// FOLDER.** The release's digest matches; `run[0]` resolves inside
/// `tools/<name>/<version>/`; the session starts it as an MCP server with
/// `run[1..]`; it provides the browser; a second install is a no-op.
#[test]
fn an_install_is_checked_unpacked_and_recorded() {
    let data = tempfile::tempdir().expect("data");
    let fake = FakeRelease::start();
    let bytes = archive_of(&[(program_file(), b"binary")]);
    fake.publish(
        "v1.2.3",
        &asset_here(),
        bytes.clone(),
        Some(&sha256_hex(&bytes)),
    );
    let tool = entry("browser");

    let done = install(&tool, data.path(), &fake.api).expect("installs");
    let Installed::Fresh(manifest) = done else {
        panic!("not a fresh install: {done:?}");
    };
    assert_eq!(manifest.version, "v1.2.3");
    assert_eq!(manifest.sha256, sha256_hex(&bytes));
    assert_eq!(super::manifest(data.path(), "browser"), Some(manifest));

    let program = executable(&tool, data.path()).expect("installed");
    let folder = tools_dir(data.path()).join("browser").join("v1.2.3");
    assert!(program.starts_with(&folder), "{}", program.display());
    assert_eq!(
        provider("browser", std::slice::from_ref(&tool), data.path()),
        Some(program.clone())
    );
    let (specs, said) = session_servers(std::slice::from_ref(&tool), data.path());
    assert!(said.is_empty(), "{said:?}");
    assert_eq!(specs.len(), 1);
    match &specs[0].transport {
        crate::toolbox::Transport::Stdio { command, args, .. } => {
            assert_eq!(command, &program);
            assert_eq!(args, &["mcp".to_string()]);
        }
        other => panic!("not a stdio server: {other:?}"),
    }
    let leftovers: Vec<String> = std::fs::read_dir(tools_dir(data.path()).join("browser"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    assert!(matches!(
        install(&tool, data.path(), &fake.api),
        Ok(Installed::Already(_))
    ));
    assert_eq!(fake.downloads(), 1, "a second install downloaded again");
}

/// **A DOWNLOAD THAT DOES NOT MATCH ITS DIGEST IS REFUSED**, deleted, and
/// nothing is unpacked or recorded — one byte flipped is enough.
#[test]
fn a_tampered_download_is_refused_and_nothing_is_installed() {
    let data = tempfile::tempdir().expect("data");
    let fake = FakeRelease::start();
    let bytes = archive_of(&[(program_file(), b"binary")]);
    let mut tampered = bytes.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x01;
    fake.publish("v1.2.3", &asset_here(), tampered, Some(&sha256_hex(&bytes)));
    let tool = entry("browser");

    let refused = install(&tool, data.path(), &fake.api).expect_err("tampered");
    assert!(refused.contains("refused"), "{refused}");
    assert!(refused.contains(&sha256_hex(&bytes)), "{refused}");
    assert_eq!(fake.downloads(), 1);
    assert!(super::manifest(data.path(), "browser").is_none());
    assert!(executable(&tool, data.path()).is_none());
    let left: Vec<String> = std::fs::read_dir(tools_dir(data.path()).join("browser"))
        .expect("dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// **WITH NO DIGEST AND NO PIN, NOTHING IS DOWNLOADED.** A pin that
/// disagrees with the release is refused too; a pin alone is enough.
#[test]
fn an_asset_with_nothing_to_check_it_against_is_not_downloaded() {
    let data = tempfile::tempdir().expect("data");
    let fake = FakeRelease::start();
    let bytes = archive_of(&[(program_file(), b"binary")]);
    fake.publish("v1.0.0", &asset_here(), bytes.clone(), None);
    let mut tool = entry("browser");
    let refused = install(&tool, data.path(), &fake.api).expect_err("unchecked");
    assert!(refused.contains("no sha256"), "{refused}");
    assert_eq!(fake.downloads(), 0, "it was downloaded anyway");

    tool.sha256 = Some(Pinned::One(sha256_hex(&bytes)));
    assert!(matches!(
        install(&tool, data.path(), &fake.api),
        Ok(Installed::Fresh(_))
    ));

    fake.publish(
        "v1.0.1",
        &asset_here(),
        bytes.clone(),
        Some(&sha256_hex(b"other")),
    );
    tool.version = Some("v1.0.1".to_string());
    let refused = install(&tool, data.path(), &fake.api).expect_err("disagree");
    assert!(refused.contains("pins"), "{refused}");
}

/// **PLAIN HTTP TO ANOTHER MACHINE IS NEVER FETCHED**, even with a pin.
#[test]
fn plain_http_to_another_machine_is_refused() {
    let data = tempfile::tempdir().expect("data");
    let mut tool = entry("browser");
    tool.release = None;
    tool.url = Some("http://example.test/tool-{arch}-{os}.{ext}".to_string());
    tool.sha256 = Some(Pinned::One("a".repeat(64)));
    let refused = install(&tool, data.path(), fetch::GITHUB_API).expect_err("http");
    assert!(refused.contains("refused to fetch"), "{refused}");
}

/// **A RELEASE WITHOUT `run[0]` IS NOT INSTALLED.**
#[test]
fn a_release_without_its_program_is_not_installed() {
    let data = tempfile::tempdir().expect("data");
    let fake = FakeRelease::start();
    let bytes = archive_of(&[("something-else", b"x")]);
    fake.publish(
        "v2",
        &asset_here(),
        bytes.clone(),
        Some(&sha256_hex(&bytes)),
    );
    let refused = install(&entry("browser"), data.path(), &fake.api).expect_err("no program");
    assert!(refused.contains("holds no `tool`"), "{refused}");
    assert!(super::manifest(data.path(), "browser").is_none());
}

/// A configured tool that is not installed is named with how to install it,
/// and nothing is started for it.
#[test]
fn a_tool_not_installed_is_named_not_started() {
    let data = tempfile::tempdir().expect("data");
    let (specs, said) = session_servers(&[entry("browser")], data.path());
    assert!(specs.is_empty());
    assert_eq!(said.len(), 1);
    assert!(said[0].contains("/tools add browser"), "{said:?}");
}

/// `/tools`: the list, an unknown name, usage, and remove.
#[test]
fn the_tools_command_lists_refuses_and_removes() {
    let data = tempfile::tempdir().expect("data");
    assert!(command("", data.path())[0].contains("no tools are configured"));
    let admin = crate::admin::AdminConfig {
        tools: vec![entry("browser")],
        ..crate::admin::AdminConfig::default()
    };
    admin.save(data.path()).expect("save");
    assert!(command("", data.path())[0].contains("not installed"));
    assert!(command("add nope", data.path())[0].contains("no tool `nope`"));
    assert!(command("add", data.path())[0].contains("usage"));
    assert!(command("frobnicate", data.path())[0].contains("unknown"));

    let folder = tools_dir(data.path()).join("browser").join("v1");
    std::fs::create_dir_all(&folder).expect("seed");
    let said = command("remove browser", data.path());
    assert!(said[0].contains("removed"), "{said:?}");
    assert!(!tools_dir(data.path()).join("browser").exists());
    assert!(command("remove ../x", data.path())[0].contains("not a tool name"));
}
