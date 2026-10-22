//! Opt-in OpenTelemetry export: usage as OTLP/HTTP JSON.
//!
//! # When it is on
//!
//! Only when the operator turns it on — `INCODE_TELEMETRY=1`, or
//! `"telemetry": true` in admin.json — AND names a collector with
//! `OTEL_EXPORTER_OTLP_ENDPOINT` (or the per-signal
//! `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` / `..._LOGS_ENDPOINT`). Otherwise
//! nothing is recorded and nothing is sent.
//!
//! - **Protocol:** `http/json` only (`OTEL_EXPORTER_OTLP_PROTOCOL` unset or
//!   that), posted by hand with the client incode already has: no
//!   OpenTelemetry crates, no gRPC stack. Another protocol turns it off and
//!   says so, rather than sending what the collector cannot read.
//! - **Where:** `https`, or `http` to this machine — the rule every key in
//!   incode is held to (`agent::catalogue::key_may_travel_to`), since
//!   `OTEL_EXPORTER_OTLP_HEADERS` usually carries a token.
//! - **What is never sent:** the contents of a prompt, a file, a tool's
//!   input or output. A prompt's text is added to its event only with
//!   `OTEL_LOG_USER_PROMPTS=1`; its length always.
//!
//! # What is sent
//!
//! Metrics, as delta sums since the last export:
//!
//! | name | unit | attributes |
//! |---|---|---|
//! | `incode.session.count` | 1 | `mode` (`repl`, `acp`, `task`) |
//! | `incode.token.usage` | tokens | `type` (`input`, `output`, `cache_read`, `cache_creation`), `model` |
//! | `incode.cost.usage` | USD | `model` (priced models only) |
//! | `incode.tool.calls` | 1 | `tool`, `outcome` (`allowed`, `denied`) |
//! | `incode.turn.count` · `incode.turn.duration` | 1 · s | `model` |
//!
//! Events, as log records: `incode.permission_prompt` (`tool`, `answer`),
//! `incode.compaction` (`trigger`, `tokens_before`, `tokens_after`),
//! `incode.user_prompt` (`prompt_length`, and `prompt` only as above).
//!
//! # When it is sent
//!
//! After every turn, on a thread of its own so nobody waits for it; at the
//! end of a `-t` run, before it exits. Each export is bounded at
//! [`EXPORT_BOUND`]; one that fails is traced (`telemetry`) and dropped.
//!
//! # Wired to
//!
//! - [`session_started`] — the REPL's and `--acp`'s callers of
//!   `cli::prepare_session`, and `task_run::run_task_hooked` (`-t`).
//! - [`turn_finished`] and [`export_in_background`] —
//!   `cli::agent_turn::AgentSession::turn_observed`, where a turn's usage is
//!   final.
//! - [`tool_call`] — `toolbox::Toolbox::run_call`, around every call.
//! - [`permission_prompt`] — `toolbox::Toolbox::confirm_with_person` and
//!   `ask_protected`, the two places a person is asked.
//! - [`compaction`] — `agent::api::ApiBackend` (`compact`,
//!   `compact_if_near_window`).
//! - [`user_prompt`] — `cli::plan_turn`, and `task_run` for `-t`.
//! - [`export_now`] — `task_run`, at the end of a run.
//! - [`describe`] — `admin::AdminConfig::report`.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

/// The most one export may take.
pub const EXPORT_BOUND: Duration = Duration::from_secs(5);

/// Where and how to export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    metrics_url: String,
    logs_url: String,
    headers: Vec<(String, String)>,
    log_prompts: bool,
}

/// The export as the environment (`get`) and admin.json's `telemetry`
/// (`admin`) set it: `Ok(None)` when it is off, `Err` with why when it was
/// asked for and cannot be done.
///
/// # Errors
///
/// Turned on with no collector named, a protocol other than `http/json`,
/// a collector that is neither `https` nor this machine, or headers that do
/// not parse.
pub fn config_from(
    get: &dyn Fn(&str) -> Option<String>,
    admin: Option<bool>,
) -> Result<Option<Config>, String> {
    let set = |name: &str| {
        get(name)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let on =
        matches!(set("INCODE_TELEMETRY").as_deref(), Some("1" | "true")) || admin == Some(true);
    if !on {
        return Ok(None);
    }
    if let Some(protocol) = set("OTEL_EXPORTER_OTLP_PROTOCOL") {
        if protocol != "http/json" {
            return Err(format!(
                "OTEL_EXPORTER_OTLP_PROTOCOL={protocol}: incode exports only http/json, so \
                 telemetry is off"
            ));
        }
    }
    let base = set("OTEL_EXPORTER_OTLP_ENDPOINT").map(|b| b.trim_end_matches('/').to_string());
    let signal =
        |own: &str, path: &str| set(own).or_else(|| base.as_ref().map(|b| format!("{b}{path}")));
    let (Some(metrics_url), Some(logs_url)) = (
        signal("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT", "/v1/metrics"),
        signal("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", "/v1/logs"),
    ) else {
        return Err(
            "telemetry is on but no OTEL_EXPORTER_OTLP_ENDPOINT is set, so nothing is exported"
                .to_string(),
        );
    };
    for url in [&metrics_url, &logs_url] {
        crate::agent::catalogue::key_may_travel_to(url)
            .map_err(|why| format!("telemetry is off: {url}: {why}"))?;
    }
    let headers = match set("OTEL_EXPORTER_OTLP_HEADERS") {
        Some(raw) => parse_headers(&raw)?,
        None => Vec::new(),
    };
    Ok(Some(Config {
        metrics_url,
        logs_url,
        headers,
        log_prompts: matches!(set("OTEL_LOG_USER_PROMPTS").as_deref(), Some("1" | "true")),
    }))
}

/// `key=value,key2=value2`, values percent-decoded, as the OpenTelemetry
/// specification writes `OTEL_EXPORTER_OTLP_HEADERS`.
fn parse_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').ok_or_else(|| {
                "OTEL_EXPORTER_OTLP_HEADERS has an entry with no `=`, so telemetry is off"
                    .to_string()
            })?;
            Ok((name.trim().to_string(), percent_decode(value.trim())))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A metric's attributes, sorted, as its identity.
type Attributes = Vec<(String, String)>;

/// What is waiting to be exported.
#[derive(Debug, Default)]
struct Pending {
    sums: BTreeMap<(&'static str, Attributes), f64>,
    events: Vec<(&'static str, u128, Attributes)>,
    since: u128,
}

/// Records metrics and events in memory and exports them.
#[derive(Debug)]
pub struct Recorder {
    config: Config,
    pending: Mutex<Pending>,
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn attributes(pairs: &[(&str, &str)]) -> Attributes {
    let mut out: Attributes = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    out.sort();
    out
}

/// The unit OTLP is told for a metric.
fn unit_of(name: &str) -> &'static str {
    match name {
        "incode.token.usage" => "tokens",
        "incode.cost.usage" => "USD",
        "incode.turn.duration" => "s",
        _ => "1",
    }
}

fn otlp_attributes(attributes: &[(String, String)]) -> Value {
    Value::Array(
        attributes
            .iter()
            .map(|(k, v)| json!({"key": k, "value": {"stringValue": v}}))
            .collect(),
    )
}

fn resource() -> Value {
    json!({"attributes": otlp_attributes(&[
        ("service.name".to_string(), "incode".to_string()),
        ("service.version".to_string(), env!("CARGO_PKG_VERSION").to_string()),
    ])})
}

fn scope() -> Value {
    json!({"name": "incode", "version": env!("CARGO_PKG_VERSION")})
}

impl Recorder {
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self {
            config,
            pending: Mutex::new(Pending {
                since: now_nanos(),
                ..Pending::default()
            }),
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Add `value` to the sum `name` with `pairs`.
    pub fn add(&self, name: &'static str, pairs: &[(&str, &str)], value: f64) {
        *self
            .locked()
            .sums
            .entry((name, attributes(pairs)))
            .or_insert(0.0) += value;
    }

    /// Record the event `name` with `pairs`, now.
    pub fn event(&self, name: &'static str, pairs: &[(&str, &str)]) {
        self.locked()
            .events
            .push((name, now_nanos(), attributes(pairs)));
    }

    /// Whether a prompt's text may go in its event.
    #[must_use]
    fn logs_prompts(&self) -> bool {
        self.config.log_prompts
    }

    /// What is pending, as the OTLP/JSON bodies for `/v1/metrics` and
    /// `/v1/logs` (`None` for a signal with nothing to say), emptied.
    fn drain(&self) -> (Option<Value>, Option<Value>) {
        let now = now_nanos();
        let mut pending = self.locked();
        let since = std::mem::replace(&mut pending.since, now);
        let sums = std::mem::take(&mut pending.sums);
        let events = std::mem::take(&mut pending.events);
        drop(pending);

        let metrics = (!sums.is_empty()).then(|| {
            let mut by_name: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
            for ((name, attributes), value) in sums {
                by_name.entry(name).or_default().push(json!({
                    "attributes": otlp_attributes(&attributes),
                    "startTimeUnixNano": since.to_string(),
                    "timeUnixNano": now.to_string(),
                    "asDouble": value,
                }));
            }
            let metrics: Vec<Value> = by_name
                .into_iter()
                .map(|(name, points)| {
                    json!({
                        "name": name,
                        "unit": unit_of(name),
                        // 1: AGGREGATION_TEMPORALITY_DELTA.
                        "sum": {"aggregationTemporality": 1, "isMonotonic": true, "dataPoints": points},
                    })
                })
                .collect();
            json!({"resourceMetrics": [{
                "resource": resource(),
                "scopeMetrics": [{"scope": scope(), "metrics": metrics}],
            }]})
        });
        let logs = (!events.is_empty()).then(|| {
            let records: Vec<Value> = events
                .into_iter()
                .map(|(name, at, attributes)| {
                    let mut attributes = attributes;
                    attributes.push(("event.name".to_string(), name.to_string()));
                    json!({
                        "timeUnixNano": at.to_string(),
                        "body": {"stringValue": name},
                        "attributes": otlp_attributes(&attributes),
                    })
                })
                .collect();
            json!({"resourceLogs": [{
                "resource": resource(),
                "scopeLogs": [{"scope": scope(), "logRecords": records}],
            }]})
        });
        (metrics, logs)
    }

    /// Send what is pending, and empty it.
    ///
    /// # Errors
    ///
    /// A signal the collector did not take: why. What it did not take is
    /// dropped, not kept for the next export.
    pub fn export(&self) -> Result<(), String> {
        let (metrics, logs) = self.drain();
        let mut failed = Vec::new();
        for (url, body) in [
            (&self.config.metrics_url, metrics),
            (&self.config.logs_url, logs),
        ] {
            let Some(body) = body else { continue };
            let (url, headers) = (url.clone(), self.config.headers.clone());
            let sent = crate::runtime::http::request_empty(
                move |client| {
                    let mut request = client
                        .post(&url)
                        .timeout(EXPORT_BOUND)
                        .header("content-type", "application/json")
                        .body(body.to_string());
                    for (name, value) in &headers {
                        request = request.header(name.as_str(), value.as_str());
                    }
                    request
                },
                "telemetry collector",
            );
            if let Err(why) = sent {
                failed.push(why);
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }
}

/// The process's recorder, when telemetry is on.
fn global() -> Option<&'static Recorder> {
    static GLOBAL: OnceLock<Option<Recorder>> = OnceLock::new();
    GLOBAL
        .get_or_init(|| {
            let admin = crate::admin::AdminConfig::load_default().telemetry;
            match config_from(&|name| std::env::var(name).ok(), admin) {
                Ok(config) => config.map(Recorder::new),
                Err(why) => {
                    eprintln!("incode: {why}");
                    None
                }
            }
        })
        .as_ref()
}

#[cfg(test)]
thread_local! {
    /// A test's own recorder for this thread, used instead of the process's.
    static CAPTURE: std::cell::RefCell<Option<std::sync::Arc<Recorder>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `record` on the recorder in use: a test's on its thread, else the
/// process's when telemetry is on.
fn with(record: impl FnOnce(&Recorder)) {
    #[cfg(test)]
    {
        if let Some(recorder) = CAPTURE.with(|c| c.borrow().clone()) {
            record(&recorder);
            return;
        }
    }
    if let Some(recorder) = global() {
        record(recorder);
    }
}

/// A session started; `mode` is `repl`, `acp` or `task`.
pub fn session_started(mode: &str) {
    with(|r| r.add("incode.session.count", &[("mode", mode)], 1.0));
}

/// A turn on `model` finished, using `usage`, after `seconds`.
pub fn turn_finished(model: &str, usage: &crate::runtime::usage::TokenUsage, seconds: f64) {
    with(|r| {
        for (kind, tokens) in [
            ("input", usage.input_tokens),
            ("output", usage.output_tokens),
            ("cache_read", usage.cache_read_input_tokens),
            ("cache_creation", usage.cache_creation_input_tokens),
        ] {
            if tokens > 0 {
                r.add(
                    "incode.token.usage",
                    &[("type", kind), ("model", model)],
                    f64::from(tokens),
                );
            }
        }
        if let Some(cost) = usage.estimate_cost_usd(Some(model)) {
            r.add("incode.cost.usage", &[("model", model)], cost);
        }
        r.add("incode.turn.count", &[("model", model)], 1.0);
        r.add("incode.turn.duration", &[("model", model)], seconds);
    });
}

/// A tool call was decided: `outcome` is `allowed` (it ran, whatever it
/// then returned) or `denied` (it never ran).
pub fn tool_call(tool: &str, outcome: &str) {
    with(|r| {
        r.add(
            "incode.tool.calls",
            &[("tool", tool), ("outcome", outcome)],
            1.0,
        );
    });
}

/// A person was asked about `tool` and answered `answer` (`allow`, `deny`,
/// or `unanswered` when there was nobody to ask).
pub fn permission_prompt(tool: &str, answer: &str) {
    with(|r| {
        r.event(
            "incode.permission_prompt",
            &[("tool", tool), ("answer", answer)],
        )
    });
}

/// The conversation was compacted (`trigger`: `manual` or `auto`).
pub fn compaction(trigger: &str, tokens_before: u64, tokens_after: u64) {
    let (before, after) = (tokens_before.to_string(), tokens_after.to_string());
    with(|r| {
        r.event(
            "incode.compaction",
            &[
                ("trigger", trigger),
                ("tokens_before", &before),
                ("tokens_after", &after),
            ],
        );
    });
}

/// A person's line went to the agent. Its length always; its text only
/// with `OTEL_LOG_USER_PROMPTS=1`.
pub fn user_prompt(text: &str) {
    let length = text.chars().count().to_string();
    with(|r| {
        if r.logs_prompts() {
            r.event(
                "incode.user_prompt",
                &[("prompt_length", &length), ("prompt", text)],
            );
        } else {
            r.event("incode.user_prompt", &[("prompt_length", &length)]);
        }
    });
}

/// Export what is pending on a thread of its own: nobody waits for it.
pub fn export_in_background() {
    if let Some(recorder) = global() {
        std::thread::spawn(move || {
            if let Err(why) = recorder.export() {
                crate::trace!("telemetry", "export failed: {why}");
            }
        });
    }
}

/// Export what is pending now, before the process ends (`-t`).
pub fn export_now() {
    if let Some(recorder) = global() {
        if let Err(why) = recorder.export() {
            crate::trace!("telemetry", "export failed: {why}");
        }
    }
}

/// admin.json's `telemetry` and the environment, as `--admin` shows them.
#[must_use]
pub fn describe(admin: Option<bool>) -> String {
    match config_from(&|name| std::env::var(name).ok(), admin) {
        Ok(Some(config)) => format!(
            "on: {} · {}{}",
            config.metrics_url,
            config.logs_url,
            if config.log_prompts {
                " · prompt text included"
            } else {
                ""
            }
        ),
        Ok(None) => "off".to_string(),
        Err(why) => format!("off: {why}"),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    /// Run `f` with a recorder of this thread's own, and return what was
    /// recorded as `(metrics, logs)` bodies. For tests of the wiring.
    pub(crate) fn captured<T>(f: impl FnOnce() -> T) -> (T, Option<Value>, Option<Value>) {
        let recorder = std::sync::Arc::new(Recorder::new(Config {
            metrics_url: "http://127.0.0.1:1/v1/metrics".to_string(),
            logs_url: "http://127.0.0.1:1/v1/logs".to_string(),
            headers: Vec::new(),
            log_prompts: false,
        }));
        CAPTURE.with(|c| *c.borrow_mut() = Some(std::sync::Arc::clone(&recorder)));
        let out = f();
        CAPTURE.with(|c| *c.borrow_mut() = None);
        let (metrics, logs) = recorder.drain();
        (out, metrics, logs)
    }

    /// The value of the sum `name` whose attributes include every one of
    /// `pairs`, in a metrics body.
    pub(crate) fn sum_of(metrics: &Value, name: &str, pairs: &[(&str, &str)]) -> f64 {
        let mut total = 0.0;
        for metric in metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["name"] == name)
        {
            for point in metric["sum"]["dataPoints"].as_array().into_iter().flatten() {
                let has = |k: &str, v: &str| {
                    point["attributes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|a| a["key"] == k && a["value"]["stringValue"] == v)
                };
                if pairs.iter().all(|(k, v)| has(k, v)) {
                    total += point["asDouble"].as_f64().unwrap_or(0.0);
                }
            }
        }
        total
    }

    /// **OFF UNLESS TURNED ON, AND THEN ONLY TO A COLLECTOR IT MAY REACH,
    /// OVER A PROTOCOL IT SPEAKS.**
    #[test]
    fn telemetry_is_off_unless_turned_on_and_refuses_what_it_cannot_do() {
        let endpoint = ("OTEL_EXPORTER_OTLP_ENDPOINT", "https://otel.example.com/");
        assert_eq!(
            config_from(&env(&[endpoint]), None),
            Ok(None),
            "not turned on"
        );
        assert_eq!(config_from(&env(&[endpoint]), Some(false)), Ok(None));

        let on = config_from(&env(&[("INCODE_TELEMETRY", "1"), endpoint]), None)
            .expect("valid")
            .expect("on");
        assert_eq!(on.metrics_url, "https://otel.example.com/v1/metrics");
        assert_eq!(on.logs_url, "https://otel.example.com/v1/logs");
        assert!(!on.log_prompts);
        assert!(
            config_from(&env(&[endpoint]), Some(true))
                .expect("valid")
                .is_some(),
            "admin.json turns it on too"
        );

        let refused =
            |pairs: &[(&str, &str)]| config_from(&env(pairs), Some(true)).expect_err("refused");
        assert!(refused(&[]).contains("no OTEL_EXPORTER_OTLP_ENDPOINT"));
        assert!(
            refused(&[endpoint, ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")])
                .contains("only http/json")
        );
        assert!(
            refused(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://otel.example.com")])
                .contains("telemetry is off"),
            "plain http off this machine"
        );
        assert!(
            refused(&[endpoint, ("OTEL_EXPORTER_OTLP_HEADERS", "nonsense")]).contains("no `=`")
        );

        let local = config_from(
            &env(&[
                (
                    "OTEL_EXPORTER_OTLP_METRICS_ENDPOINT",
                    "http://127.0.0.1:4318/m",
                ),
                (
                    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
                    "http://localhost:4318/l",
                ),
                (
                    "OTEL_EXPORTER_OTLP_HEADERS",
                    "authorization=Bearer%20abc, x-team = core",
                ),
                ("OTEL_LOG_USER_PROMPTS", "1"),
            ]),
            Some(true),
        )
        .expect("valid")
        .expect("on");
        assert_eq!(local.metrics_url, "http://127.0.0.1:4318/m");
        assert_eq!(
            local.headers,
            [
                ("authorization".to_string(), "Bearer abc".to_string()),
                ("x-team".to_string(), "core".to_string())
            ]
        );
        assert!(local.log_prompts);
    }

    /// A collector on 127.0.0.1 that keeps each request's path, headers and
    /// body: its base URL and what it received.
    fn collector(times: usize) -> (String, std::sync::mpsc::Receiver<(String, String, Value)>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(times) {
                let Ok(mut stream) = stream else { continue };
                let mut seen = Vec::new();
                let mut buf = [0u8; 16384];
                let (head, body) = loop {
                    let Ok(n) = stream.read(&mut buf) else { return };
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&seen).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + length {
                            break (
                                text[..end].to_string(),
                                text[end + 4..end + 4 + length].to_string(),
                            );
                        }
                    }
                };
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                );
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                let _ = tx.send((path, head, body));
            }
        });
        (format!("http://127.0.0.1:{port}"), rx)
    }

    /// **WHAT IS RECORDED REACHES THE COLLECTOR AS OTLP/JSON**: metrics as
    /// delta sums to `/v1/metrics`, events as log records to `/v1/logs`, the
    /// operator's headers on both — and no prompt text unless asked for.
    #[test]
    fn recorded_usage_reaches_a_collector_as_otlp_json() {
        let (base, received) = collector(2);
        let config = config_from(
            &env(&[
                ("OTEL_EXPORTER_OTLP_ENDPOINT", &base),
                ("OTEL_EXPORTER_OTLP_HEADERS", "x-api-key=k1"),
            ]),
            Some(true),
        )
        .expect("valid")
        .expect("on");
        let recorder = std::sync::Arc::new(Recorder::new(config));
        CAPTURE.with(|c| *c.borrow_mut() = Some(std::sync::Arc::clone(&recorder)));
        session_started("repl");
        turn_finished(
            "claude-sonnet-4",
            &crate::runtime::usage::TokenUsage {
                input_tokens: 1000,
                output_tokens: 200,
                ..Default::default()
            },
            2.5,
        );
        tool_call("Write", "denied");
        tool_call("Read", "allowed");
        tool_call("Read", "allowed");
        permission_prompt("Bash", "deny");
        compaction("auto", 9000, 3000);
        user_prompt("delete the secret plans");
        CAPTURE.with(|c| *c.borrow_mut() = None);
        recorder.export().expect("the collector took both");

        let mut got: BTreeMap<String, (String, Value)> = BTreeMap::new();
        for _ in 0..2 {
            let (path, head, body) = received
                .recv_timeout(Duration::from_secs(10))
                .expect("an export arrived");
            got.insert(path, (head, body));
        }
        let (head, metrics) = &got["/v1/metrics"];
        assert!(
            head.to_ascii_lowercase().contains("x-api-key: k1"),
            "{head}"
        );
        assert_eq!(
            sum_of(metrics, "incode.session.count", &[("mode", "repl")]),
            1.0
        );
        assert_eq!(
            sum_of(
                metrics,
                "incode.token.usage",
                &[("type", "input"), ("model", "claude-sonnet-4")]
            ),
            1000.0
        );
        assert_eq!(
            sum_of(metrics, "incode.token.usage", &[("type", "output")]),
            200.0
        );
        assert!(
            sum_of(metrics, "incode.cost.usage", &[]) > 0.0,
            "a priced model has a cost"
        );
        assert_eq!(
            sum_of(
                metrics,
                "incode.tool.calls",
                &[("tool", "Read"), ("outcome", "allowed")]
            ),
            2.0
        );
        assert_eq!(
            sum_of(metrics, "incode.tool.calls", &[("outcome", "denied")]),
            1.0
        );
        assert_eq!(sum_of(metrics, "incode.turn.duration", &[]), 2.5);
        let metric = metrics["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0].clone();
        assert_eq!(metric["sum"]["aggregationTemporality"], 1, "delta");

        let (_, logs) = &got["/v1/logs"];
        let text = logs.to_string();
        for name in [
            "incode.permission_prompt",
            "incode.compaction",
            "incode.user_prompt",
        ] {
            assert!(text.contains(name), "{name} missing: {text}");
        }
        assert!(text.contains("\"prompt_length\""), "{text}");
        assert!(
            !text.contains("secret plans"),
            "a prompt's text went out unasked: {text}"
        );

        // Exported means emptied: nothing goes twice.
        let (metrics, logs) = recorder.drain();
        assert!(metrics.is_none() && logs.is_none());
    }

    #[test]
    fn a_percent_encoded_header_value_is_decoded() {
        assert_eq!(percent_decode("Bearer%20a%2Cb"), "Bearer a,b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("é%2"), "é%2");
    }
}
