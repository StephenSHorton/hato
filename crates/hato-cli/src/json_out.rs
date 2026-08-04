//! Machine-readable NDJSON output for host embedding (suzuri, scripts).
//!
//! When enabled, **only** NDJSON lines go to stdout. Human text and progress
//! bars stay on stderr or are suppressed. Protocol version is in every event's
//! `v` field.

use std::io::{self, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::json;

/// Protocol version for host parsers.
pub const PROTOCOL_V: u32 = 1;

/// Progress throttle: emit at most this often (unless forced).
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Shared flag: when true, helpers write NDJSON to stdout.
static JSON_MODE: Mutex<bool> = Mutex::new(false);

/// Enable or disable JSON mode for this process.
pub fn set_enabled(on: bool) {
    if let Ok(mut g) = JSON_MODE.lock() {
        *g = on;
    }
}

/// Whether NDJSON mode is active.
pub fn enabled() -> bool {
    JSON_MODE.lock().map(|g| *g).unwrap_or(false)
}

/// Emit one NDJSON event object on stdout (only if JSON mode is on).
pub fn emit(event: &str, fields: serde_json::Value) {
    if !enabled() {
        return;
    }
    let mut obj = match fields {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert("v".into(), json!(PROTOCOL_V));
    obj.insert("event".into(), json!(event));
    let line = serde_json::Value::Object(obj).to_string();
    let mut out = io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Throttled progress reporter for JSON mode (and no-op setup for human mode).
pub struct ProgressEmitter {
    last: Mutex<LastProgress>,
}

struct LastProgress {
    at: Instant,
    done: u64,
    total: u64,
}

impl ProgressEmitter {
    pub fn new() -> Self {
        Self {
            last: Mutex::new(LastProgress {
                at: Instant::now()
                    .checked_sub(PROGRESS_MIN_INTERVAL)
                    .unwrap_or_else(Instant::now),
                done: 0,
                total: 0,
            }),
        }
    }

    /// Emit progress if enough time passed or transfer finished (`done == total` and total > 0).
    pub fn on_progress(&self, done: u64, total: u64) {
        if !enabled() {
            return;
        }
        let force = total > 0 && done >= total;
        let mut g = match self.last.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let now = Instant::now();
        if !force
            && now.duration_since(g.at) < PROGRESS_MIN_INTERVAL
            && done == g.done
            && total == g.total
        {
            return;
        }
        // Also skip if nothing moved and not forced / not interval
        if !force && now.duration_since(g.at) < PROGRESS_MIN_INTERVAL && done == g.done {
            return;
        }
        g.at = now;
        g.done = done;
        g.total = total;
        drop(g);
        emit(
            "progress",
            json!({
                "done": done,
                "total": total,
            }),
        );
    }
}

/// Stable process exit codes for host embedding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Generic = 1,
    Usage = 2,
    VerifierMismatch = 3,
    Peer = 4,
    OfferRejected = 5,
    Interrupted = 130,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Error that carries a stable exit code.
#[derive(Debug)]
pub struct CodedError {
    pub code: ExitCode,
    pub err: anyhow::Error,
}

impl CodedError {
    pub fn new(code: ExitCode, err: impl Into<anyhow::Error>) -> Self {
        Self {
            code,
            err: err.into(),
        }
    }
}

impl std::fmt::Display for CodedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.err)
    }
}

impl std::error::Error for CodedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.err.source()
    }
}

/// Map common failures onto exit codes; emit JSON error when enabled.
pub fn fail(code: ExitCode, err: impl Into<anyhow::Error>) -> CodedError {
    let err = err.into();
    let message = format!("{err:#}");
    let code_str = match code {
        ExitCode::Ok => "ok",
        ExitCode::Generic => "generic",
        ExitCode::Usage => "usage",
        ExitCode::VerifierMismatch => "verifier_mismatch",
        ExitCode::Peer => "peer",
        ExitCode::OfferRejected => "offer_rejected",
        ExitCode::Interrupted => "interrupted",
    };
    emit("error", json!({"code": code_str, "message": message}));
    CodedError::new(code, err)
}

/// Convert an anyhow error into a coded exit (default generic).
pub fn from_anyhow(err: anyhow::Error) -> CodedError {
    let msg = format!("{err:#}");
    let lower = msg.to_ascii_lowercase();
    let code = if lower.contains("wrong code") || lower.contains("verifier") {
        ExitCode::VerifierMismatch
    } else if lower.contains("usage")
        || lower.contains("required")
        || lower.contains("no such file")
        || lower.contains("not found")
    {
        ExitCode::Usage
    } else if lower.contains("offer") && lower.contains("reject") {
        ExitCode::OfferRejected
    } else if lower.contains("contact") || lower.contains("unreachable") {
        ExitCode::Peer
    } else {
        ExitCode::Generic
    };
    fail(code, err)
}
