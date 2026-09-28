//! Shared HTTP client and async execution runtime.
//!
//! Reuses a single multi-threaded Tokio runtime and `reqwest::Client` across
//! Incord service requests (memory, brief, guards, owner, auditor, etc.)
//! rather than building a new runtime, thread, and connection pool on every request.

use std::future::Future;
use std::sync::LazyLock;
use std::time::Duration;

pub static SHARED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
});

/// How long a connection to a server a client named may take to open.
pub const CONNECT_BOUND: Duration = Duration::from_secs(10);

/// For a server someone ELSE named (an ACP client's HTTP MCP server): follows
/// no redirect, since a redirect could carry the headers it was handed to a
/// host its address was never checked against, and bounds the connect. It has
/// no overall timeout of its own: each request sets one.
///
/// `Err` when the client could not be built, which is said rather than
/// replaced by a default client that WOULD follow redirects.
static NO_REDIRECT_CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_BOUND)
        .build()
        .map_err(|e| format!("no HTTP client: {e}"))
});

/// The shared client that follows no redirect; see [`NO_REDIRECT_CLIENT`].
pub fn no_redirect_client() -> Result<&'static reqwest::Client, String> {
    NO_REDIRECT_CLIENT.as_ref().map_err(Clone::clone)
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

/// Execute an HTTP request with `SHARED_CLIENT` and return the response text if successful.
pub fn request_text<F>(build: F, service_name: &'static str) -> Result<String, String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    run_async(async move {
        let response = build(&SHARED_CLIENT)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", error_chain(&e)))?;
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

/// Execute an HTTP request with `SHARED_CLIENT` and return `()` if successful.
pub fn request_empty<F>(build: F, service_name: &'static str) -> Result<(), String>
where
    F: FnOnce(&reqwest::Client) -> reqwest::RequestBuilder + Send + 'static,
{
    run_async(async move {
        let response = build(&SHARED_CLIENT)
            .send()
            .await
            .map_err(|e| format!("request failed: {}", error_chain(&e)))?;
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
