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

pub static SHARED_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("incord-http")
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
            .map_err(|e| format!("request failed: {e}"))?;
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
            .map_err(|e| format!("request failed: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("the {service_name} answered {status}"));
        }
        Ok(())
    })
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

    #[tokio::test(flavor = "current_thread")]
    async fn run_async_executes_from_current_thread_tokio_context() {
        let result = run_async(async { Ok::<_, String>(99) });
        assert_eq!(result, Ok(99));
    }
}
