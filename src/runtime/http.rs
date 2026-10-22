//! Shared HTTP client and async execution runtime.
//!
//! Reuses a single multi-threaded Tokio runtime and `reqwest::Client` across
//! Incord service requests (memory, brief, guards, owner, auditor, etc.)
//! rather than building a new runtime, thread, and connection pool on every request.
//!
//! # Proxies and certificate authorities
//!
//! Every outbound client is built from [`builder`]:
//!
//! - **Proxy.** reqwest's own system proxy stands: `HTTPS_PROXY`,
//!   `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY`, then the Windows or macOS
//!   proxy settings. Its matcher sends a loopback address through the proxy
//!   too unless `NO_PROXY` names it, which would put the local memory engine,
//!   a local model server or a local MCP server behind a corporate proxy that
//!   cannot reach them. So a client for THIS machine ([`is_loopback`]) is
//!   built with no proxy at all, and every other keeps the system's.
//! - **CA.** The PEM bundle named by `INCODE_CA_CERTS` (else `SSL_CERT_FILE`)
//!   is trusted beside the system's store. One that cannot be read or holds
//!   no certificate is said on stderr and not added, so a TLS connection that
//!   needed it fails rather than quietly trusting something else.
//!
//! Built here: the shared clients below, and the model API's
//! (`agent::api`) and web fetch/search's (`web`) own clients.

use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

/// Whether `url` names this machine: `localhost`, a `.localhost` name, or a
/// loopback address. Such a request never goes through a proxy.
#[must_use]
pub fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => {
            let name = host.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
    }
}

/// Whether the address `url` names this machine; see [`is_loopback`]. A URL
/// that does not parse is not.
#[must_use]
pub fn names_loopback(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| is_loopback(&url))
}

/// The base every outbound client is built from: the operator's extra CA
/// certificates added, and, for a client that only talks to this machine
/// (`loopback`), no proxy. See the module docs.
///
/// A client with no proxy follows a redirect only while it STAYS on this
/// machine: one that led elsewhere would be fetched without the proxy, past
/// whatever the proxy is there to enforce. A caller that follows none sets
/// its own policy over this one.
pub fn builder(loopback: bool) -> reqwest::ClientBuilder {
    let mut builder = reqwest::Client::builder();
    for certificate in EXTRA_ROOTS.iter() {
        builder = builder.add_root_certificate(certificate.clone());
    }
    if loopback {
        builder = builder
            .no_proxy()
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if !is_loopback(attempt.url()) {
                    attempt.error("a redirect from this machine to another is not followed")
                } else if attempt.previous().len() >= MAX_LOCAL_REDIRECTS {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }));
    }
    builder
}

/// How many redirects a client for this machine follows, as reqwest's own
/// default does.
const MAX_LOCAL_REDIRECTS: usize = 10;

/// The operator's extra certificate authorities, read once.
static EXTRA_ROOTS: LazyLock<Vec<reqwest::Certificate>> = LazyLock::new(|| {
    let (roots, warning) = roots_from(&|name| std::env::var_os(name));
    if let Some(warning) = warning {
        eprintln!("incode: {warning}");
    }
    roots
});

/// The certificates in the PEM bundle `INCODE_CA_CERTS` (else
/// `SSL_CERT_FILE`) names, as `get` reads the environment, and what to say
/// when it names one that cannot be used.
fn roots_from(
    get: &dyn Fn(&str) -> Option<std::ffi::OsString>,
) -> (Vec<reqwest::Certificate>, Option<String>) {
    let Some((name, path)) = ["INCODE_CA_CERTS", "SSL_CERT_FILE"]
        .into_iter()
        .find_map(|name| {
            get(name)
                .filter(|value| !value.is_empty())
                .map(|value| (name, std::path::PathBuf::from(value)))
        })
    else {
        return (Vec::new(), None);
    };
    let unused = |why: String| {
        (
            Vec::new(),
            Some(format!(
                "{name}={}: {why}; no extra certificate authority is trusted",
                path.display()
            )),
        )
    };
    let pem = match std::fs::read(&path) {
        Ok(pem) => pem,
        Err(e) => return unused(format!("could not be read ({e})")),
    };
    match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(roots) if !roots.is_empty() => (roots, None),
        Ok(_) => unused("holds no PEM certificate".to_string()),
        Err(e) => unused(format!("is not a PEM certificate bundle ({e})")),
    }
}

/// The client for incode's own services, with a 60 s bound on each request;
/// [`SHARED_DIRECT`] is its twin for this machine. Use [`client_for`].
pub static SHARED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| shared(false));

/// [`SHARED_CLIENT`] with no proxy, for a service on this machine.
static SHARED_DIRECT: LazyLock<reqwest::Client> = LazyLock::new(|| shared(true));

fn shared(loopback: bool) -> reqwest::Client {
    builder(loopback)
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

/// The shared client for a request to `url`: no proxy when it names this
/// machine.
#[must_use]
pub fn client_for(url: &str) -> &'static reqwest::Client {
    if names_loopback(url) {
        &SHARED_DIRECT
    } else {
        &SHARED_CLIENT
    }
}

/// How long a connection to a server a client named may take to open.
pub const CONNECT_BOUND: Duration = Duration::from_secs(10);

/// For a server someone ELSE named (an ACP client's HTTP MCP server): follows
/// no redirect, since a redirect could carry the headers it was handed to a
/// host its address was never checked against, and bounds the connect. It has
/// no overall timeout of its own: each request sets one.
///
/// `Err` when the client could not be built, which is said rather than
/// replaced by a default client that WOULD follow redirects.
static NO_REDIRECT_CLIENT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| no_redirect(false));

/// [`NO_REDIRECT_CLIENT`] with no proxy, for a server on this machine.
static NO_REDIRECT_DIRECT: LazyLock<Result<reqwest::Client, String>> =
    LazyLock::new(|| no_redirect(true));

fn no_redirect(loopback: bool) -> Result<reqwest::Client, String> {
    builder(loopback)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_BOUND)
        .build()
        .map_err(|e| format!("no HTTP client: {e}"))
}

/// The shared client that follows no redirect, for a request to `url`; see
/// [`NO_REDIRECT_CLIENT`]. No proxy when `url` names this machine.
pub fn no_redirect_client(url: &str) -> Result<&'static reqwest::Client, String> {
    if names_loopback(url) {
        NO_REDIRECT_DIRECT.as_ref().map_err(Clone::clone)
    } else {
        NO_REDIRECT_CLIENT.as_ref().map_err(Clone::clone)
    }
}

pub static SHARED_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("incode-http")
        .build()
        .expect("incord http runtime")
});

/// Run an async future to completion synchronously.
///
/// If called from outside Tokio, uses `SHARED_RUNTIME.block_on`.
/// If called from within a multi-threaded Tokio runtime, uses `block_in_place`.
/// If called from within a single-threaded Tokio runtime, dispatches to `SHARED_RUNTIME` on a separate thread.
pub fn run_async<F, R>(future: F) -> Result<R, String>
where
    F: Future<Output = Result<R, String>> + Send + 'static,
    R: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => match handle.runtime_flavor() {
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(future))
            }
            _ => std::thread::spawn(move || SHARED_RUNTIME.block_on(future))
                .join()
                .map_err(|_| "the request thread panicked".to_string())?,
        },
        Err(_) => SHARED_RUNTIME
            .block_on(future)
            .map_err(|e| format!("runtime error: {e}")),
    }
}

/// Send the request `build` makes with the shared client its address calls
/// for ([`client_for`]): built first, so the address it names is known.
async fn send_shared<F>(build: F) -> Result<reqwest::Response, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
{
    let request = build(&SHARED_CLIENT)
        .build()
        .map_err(|e| format!("request failed: {}", error_chain(&e)))?;
    client_for(request.url().as_str())
        .execute(request)
        .await
        .map_err(|e| format!("request failed: {}", error_chain(&e)))
}

/// Execute an HTTP request with the shared client and return the response text if successful.
pub fn request_text<F>(build: F, service_name: &'static str) -> Result<String, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    run_async(async move {
        let response = send_shared(build).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the {service_name} answered {status}"));
        }
        response
            .text()
            .await
            .map_err(|e| format!("reading the response failed: {e}"))
    })
}

/// Execute an HTTP request with the shared client and return `()` if successful.
pub fn request_empty<F>(build: F, service_name: &'static str) -> Result<(), String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    run_async(async move {
        let response = send_shared(build).await?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the {service_name} answered {status}"));
        }
        Ok(())
    })
}

/// An error and every cause under it, on one line.
///
/// A transport error's own text names nothing: the guards request that failed
/// at session start was traced only as "error sending request for url (…)",
/// which cannot tell a refused connection from a timeout. Its causes can.
pub(crate) fn error_chain(error: &dyn std::error::Error) -> String {
    let mut out = error.to_string();
    let mut cause = error.source();
    while let Some(inner) = cause {
        out.push_str(": ");
        out.push_str(&inner.to_string());
        cause = inner.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn this_machine_is_named_by_localhost_and_loopback_addresses_only() {
        for url in [
            "http://localhost:9180/x",
            "http://LOCALHOST./",
            "http://app.localhost/",
            "http://127.0.0.1:8080/",
            "http://127.4.5.6/",
            "http://[::1]:11434/v1",
        ] {
            assert!(names_loopback(url), "{url}");
        }
        for url in [
            "https://api.example.com/v1",
            "http://localhost.example.com/",
            "http://10.0.0.1/",
            "http://[::2]/",
            "not a url",
        ] {
            assert!(!names_loopback(url), "{url}");
        }
    }

    /// **THE CLIENT IS CHOSEN FOR EACH ADDRESS**: one on this machine gets
    /// the direct client, anything else the proxied one — so a caller that
    /// reaches two hosts must ask for each (`mcp_oauth::fetch_json`).
    #[test]
    fn the_client_is_chosen_by_the_address_it_is_for() {
        let local = "http://127.0.0.1:9180/x";
        let remote = "https://auth.example.com/.well-known/oauth-authorization-server";
        assert!(std::ptr::eq(client_for(local), &*SHARED_DIRECT));
        assert!(std::ptr::eq(client_for(remote), &*SHARED_CLIENT));
        let (Ok(direct), Ok(proxied)) = (no_redirect_client(local), no_redirect_client(remote))
        else {
            panic!("no client");
        };
        assert!(!std::ptr::eq(direct, proxied));
        assert!(std::ptr::eq(
            direct,
            no_redirect_client("http://localhost/").expect("client")
        ));
    }

    /// **A CLIENT FOR THIS MACHINE DOES NOT FOLLOW A REDIRECT OFF IT**: it
    /// has no proxy, so following would reach the other host past the proxy.
    #[test]
    fn a_redirect_from_this_machine_to_another_is_not_followed() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Some(Ok(mut stream)) = listener.incoming().next() {
                read_head(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 302 Found\r\nlocation: http://incode-redirect-probe.invalid/\r\n\
                     content-length: 0\r\nconnection: close\r\n\r\n"
                );
            }
        });
        let url = format!("http://127.0.0.1:{port}/");
        let said = request_text(move |c| c.get(&url), "test service").expect_err("redirected");
        assert!(said.contains("is not followed"), "{said}");
    }

    /// A private certificate authority and a server certificate for
    /// `localhost` it signed: the CA's PEM, the server's certificate and its
    /// key (both DER). Made at test time; nothing secret is committed.
    fn private_ca() -> (String, Vec<u8>, Vec<u8>) {
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("ca params");
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "incode test CA");
        let ca_cert = ca.self_signed(&ca_key).expect("ca");
        let key = rcgen::KeyPair::generate().expect("server key");
        let mut server =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("server params");
        server
            .distinguished_name
            .push(rcgen::DnType::CommonName, "localhost");
        server.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let cert = server.signed_by(&key, &ca_cert, &ca_key).expect("server");
        (ca_cert.pem(), cert.der().to_vec(), key.serialize_der())
    }

    fn env_of(
        pairs: Vec<(&'static str, std::path::PathBuf)>,
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.clone().into_os_string())
        }
    }

    /// **THE OPERATOR'S CA BUNDLE IS READ, AND ONE THAT CANNOT BE USED IS
    /// SAID**, naming the variable and the file; `INCODE_CA_CERTS` comes
    /// before `SSL_CERT_FILE`.
    #[test]
    fn a_ca_bundle_is_read_from_the_environment_and_a_bad_one_is_said() {
        let dir = tempfile::tempdir().expect("dir");
        let (first, ..) = private_ca();
        let (second, ..) = private_ca();
        let bundle = dir.path().join("bundle.pem");
        std::fs::write(&bundle, format!("{first}\n{second}")).expect("write");
        let garbage = dir.path().join("garbage.pem");
        std::fs::write(&garbage, "not a certificate").expect("write");
        let missing = dir.path().join("missing.pem");

        let (roots, warning) = roots_from(&env_of(Vec::new()));
        assert!(roots.is_empty() && warning.is_none());

        let (roots, warning) = roots_from(&env_of(vec![
            ("INCODE_CA_CERTS", bundle.clone()),
            ("SSL_CERT_FILE", missing.clone()),
        ]));
        assert_eq!(roots.len(), 2, "{warning:?}");
        assert!(warning.is_none(), "{warning:?}");

        let (roots, warning) = roots_from(&env_of(vec![("SSL_CERT_FILE", bundle)]));
        assert_eq!((roots.len(), warning), (2, None));

        for (path, why) in [
            (missing, "could not be read"),
            (garbage, "holds no PEM certificate"),
        ] {
            let (roots, warning) = roots_from(&env_of(vec![("INCODE_CA_CERTS", path.clone())]));
            assert!(roots.is_empty());
            let warning = warning.unwrap_or_else(|| panic!("{} was not said", path.display()));
            assert!(
                warning.starts_with("INCODE_CA_CERTS=") && warning.contains(why),
                "{warning}"
            );
        }
    }

    /// Set only in the child the network tests start: what to ask for.
    const NET_CHILD: &str = "INCODE_NET_CHILD_URL";

    /// The child half of the network tests: asks for the URL it was given
    /// through the shared client, then — so a proxy can be seen being used —
    /// for a name that exists nowhere, and prints the first answer. Returns
    /// at once, passing, unless started as the child.
    #[test]
    fn net_child() {
        let Ok(url) = std::env::var(NET_CHILD) else {
            return;
        };
        let said = request_text(move |c| c.get(&url), "test service");
        let _ = request_text(|c| c.get("http://incode-proxy-probe.invalid/"), "probe");
        println!("NET-CHILD: {said:?}");
        std::process::exit(0);
    }

    /// Run [`net_child`] asking for `url`, with `env` set and no proxy or CA
    /// setting inherited: what it printed.
    fn ask_from_a_child(url: &str, env: &[(String, String)]) -> String {
        let mut child = std::process::Command::new(std::env::current_exe().expect("exe"));
        child.args([
            "--exact",
            "runtime::http::tests::net_child",
            "--nocapture",
            "--test-threads=1",
            "-q",
        ]);
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "INCODE_CA_CERTS",
            "SSL_CERT_FILE",
        ] {
            child.env_remove(name);
        }
        child
            .env(NET_CHILD, url)
            .envs(env.iter().map(|(k, v)| (k, v)));
        let out = child.output().expect("the child runs");
        assert!(out.status.success(), "{out:?}");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A plain HTTP server on 127.0.0.1 answering `body` to each of `times`
    /// requests.
    fn http_server(body: &'static str, times: usize) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(mut stream) = stream else { continue };
                read_head(&mut stream);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        format!("http://127.0.0.1:{port}/")
    }

    /// Read a request up to the end of its headers.
    fn read_head(stream: &mut impl std::io::Read) {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// **THIS MACHINE IS REACHED DIRECTLY; ANYWHERE ELSE THROUGH THE PROXY.**
    /// With `HTTP_PROXY` naming a proxy and no `NO_PROXY`, a request to a
    /// loopback service gets its answer straight from it and never reaches
    /// the proxy, while a remote name is asked of the proxy.
    #[test]
    fn a_loopback_request_skips_the_proxy_and_a_remote_one_takes_it() {
        let proxy = incord_sandbox::proxy::Proxy::start(
            incord_sandbox::proxy::Allowlist::new(&[], false).expect("allowlist"),
            None,
        )
        .expect("proxy");
        let direct = http_server("direct", 1);
        let said = ask_from_a_child(&direct, &proxy.client_env());
        assert!(said.contains(r#"NET-CHILD: Ok("direct")"#), "{said}");

        // The proxy notes a decision once it has answered; give it a moment.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut asked: Vec<String>;
        loop {
            asked = proxy.decisions().into_iter().map(|d| d.target).collect();
            if !asked.is_empty() || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            asked
                .iter()
                .any(|t| t.contains("incode-proxy-probe.invalid")),
            "a remote request did not take the proxy: {asked:?}"
        );
        assert!(
            !asked.iter().any(|t| t.contains("127.0.0.1")),
            "a loopback request went through the proxy: {asked:?}"
        );
    }

    /// A TLS server for `localhost` on 127.0.0.1, answering `ok` to each of
    /// `times` connections: its URL.
    fn tls_server(cert: Vec<u8>, key: Vec<u8>, times: usize) -> String {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        let config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .expect("server config");
        let config = std::sync::Arc::new(config);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(tcp) = stream else { continue };
                let Ok(conn) = rustls::ServerConnection::new(std::sync::Arc::clone(&config)) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                read_head(&mut tls);
                let _ = tls.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                );
                tls.conn.send_close_notify();
                let _ = tls.flush();
            }
        });
        format!("https://localhost:{port}/")
    }

    /// **A SERVER SIGNED BY THE OPERATOR'S OWN CA IS TRUSTED ONCE
    /// `INCODE_CA_CERTS` NAMES IT**, and not without it: the same request,
    /// through the same shared client, refused and then answered.
    #[test]
    fn a_server_signed_by_the_operators_ca_is_trusted_through_incode_ca_certs() {
        let (ca_pem, cert, key) = private_ca();
        let dir = tempfile::tempdir().expect("dir");
        let bundle = dir.path().join("corporate-ca.pem");
        std::fs::write(&bundle, ca_pem).expect("write");
        let url = tls_server(cert, key, 2);

        let untrusted = ask_from_a_child(&url, &[]);
        assert!(untrusted.contains("NET-CHILD: Err("), "{untrusted}");

        let trusted = ask_from_a_child(
            &url,
            &[("INCODE_CA_CERTS".to_string(), bundle.display().to_string())],
        );
        assert!(trusted.contains(r#"NET-CHILD: Ok("ok")"#), "{trusted}");
    }

    #[test]
    fn run_async_executes_from_synchronous_context() {
        let result = run_async(async { Ok::<_, String>(42) });
        assert_eq!(result, Ok(42));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_async_executes_from_multi_threaded_tokio_context() {
        let result = run_async(async { Ok::<_, String>("hello".to_string()) });
        assert_eq!(result, Ok("hello".to_string()));
    }

    /// **A FAILED REQUEST NAMES WHY**: the transport error's causes are in
    /// the message, not just "error sending request".
    #[test]
    fn a_failed_request_says_its_cause() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let url = format!("http://127.0.0.1:{port}/");
        let said = request_text(move |c| c.get(&url), "test service").expect_err("closed port");
        assert!(said.contains("error sending request"), "{said}");
        assert!(said.to_lowercase().contains("connect"), "{said}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn run_async_executes_from_current_thread_tokio_context() {
        let result = run_async(async { Ok::<_, String>(99) });
        assert_eq!(result, Ok(99));
    }
}
