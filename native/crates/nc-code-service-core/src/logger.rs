//! Service logger helpers (port concept of `packages/services/src/logger/serviceLogger.ts`).
//!
//! The TS implementation writes formatted console lines with scope/pid/traceId. In Rust the
//! canonical sink is `tracing`/`log` in most crates; this module provides a lightweight,
//! testable structured logger that can also format the same prefix shape when needed.

use std::fmt::Debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

pub fn format_log_prefix(source: &str, pid: u32) -> String {
    format!("[{source} pid={pid}]")
}

pub trait ServiceLogSink: Send + Sync {
    fn log(&self, level: LogLevel, prefix: &str, message: &str);
}

#[derive(Default)]
pub struct StderrServiceLogSink;
impl ServiceLogSink for StderrServiceLogSink {
    fn log(&self, level: LogLevel, prefix: &str, message: &str) {
        let lvl = match level {
            LogLevel::Debug => "DEBUG",
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        };
        eprintln!("{prefix} {lvl} {message}");
    }
}

#[derive(Clone)]
pub struct ServiceLogger {
    scope: String,
    pid: u32,
    debug_enabled: bool,
    sink: std::sync::Arc<dyn ServiceLogSink>,
}

impl ServiceLogger {
    pub fn new(scope: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            pid: std::process::id(),
            debug_enabled: cfg!(debug_assertions),
            sink: std::sync::Arc::new(StderrServiceLogSink),
        }
    }

    pub fn with_debug_enabled(mut self, enabled: bool) -> Self {
        self.debug_enabled = enabled;
        self
    }
    pub fn with_sink(mut self, sink: std::sync::Arc<dyn ServiceLogSink>) -> Self {
        self.sink = sink;
        self
    }

    fn source(&self, trace_id: Option<&str>) -> String {
        match trace_id {
            Some(tid) if !tid.is_empty() => format!("{}][trace:{}", self.scope, tid),
            _ => self.scope.clone(),
        }
    }

    fn write(&self, level: LogLevel, trace_id: Option<&str>, message: impl AsRef<str>) {
        if level == LogLevel::Debug && !self.debug_enabled {
            return;
        }
        let prefix = format_log_prefix(&self.source(trace_id), self.pid);
        self.sink.log(level, &prefix, message.as_ref());
    }

    pub fn debug(&self, trace_id: Option<&str>, message: impl AsRef<str>) {
        self.write(LogLevel::Debug, trace_id, message);
    }
    pub fn info(&self, trace_id: Option<&str>, message: impl AsRef<str>) {
        self.write(LogLevel::Info, trace_id, message);
    }
    pub fn warn(&self, trace_id: Option<&str>, message: impl AsRef<str>) {
        self.write(LogLevel::Warn, trace_id, message);
    }
    pub fn error(&self, trace_id: Option<&str>, message: impl AsRef<str>) {
        self.write(LogLevel::Error, trace_id, message);
    }
}

#[derive(Default)]
pub struct CaptureLogSink {
    pub lines: std::sync::Mutex<Vec<(LogLevel, String, String)>>,
}
impl ServiceLogSink for CaptureLogSink {
    fn log(&self, level: LogLevel, prefix: &str, message: &str) {
        self.lines
            .lock()
            .unwrap()
            .push((level, prefix.to_string(), message.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn formats_trace_prefix() {
        let sink = Arc::new(CaptureLogSink::default());
        let logger = ServiceLogger::new("svc")
            .with_debug_enabled(true)
            .with_sink(sink.clone());
        logger.info(Some("t1"), "hello");
        let lines = sink.lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].1.contains("trace:t1"));
        assert_eq!(lines[0].2, "hello");
    }

    #[test]
    fn debug_suppressed_when_disabled() {
        let sink = Arc::new(CaptureLogSink::default());
        let logger = ServiceLogger::new("svc")
            .with_debug_enabled(false)
            .with_sink(sink.clone());
        logger.debug(None, "x");
        assert!(sink.lines.lock().unwrap().is_empty());
    }
}
