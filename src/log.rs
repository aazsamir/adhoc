//! A tiny logger: one line per event, on stderr.
//!
//! `HH:MM:SS LEVEL [service] message key=value key=value`

use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO ",
            Level::Warn => "WARN ",
            Level::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Logger {
    level: Level,
    service: Option<String>,
}

impl Logger {
    pub fn new(debug: bool) -> Logger {
        Logger {
            level: if debug { Level::Debug } else { Level::Info },
            service: None,
        }
    }

    /// A logger that prefixes every line with the service name.
    pub fn with_service(&self, name: &str) -> Logger {
        Logger {
            level: self.level,
            service: Some(name.to_string()),
        }
    }

    pub fn debug(&self, message: &str, fields: &[(&str, String)]) {
        self.log(Level::Debug, message, fields);
    }

    pub fn info(&self, message: &str, fields: &[(&str, String)]) {
        self.log(Level::Info, message, fields);
    }

    pub fn warn(&self, message: &str, fields: &[(&str, String)]) {
        self.log(Level::Warn, message, fields);
    }

    pub fn error(&self, message: &str, fields: &[(&str, String)]) {
        self.log(Level::Error, message, fields);
    }

    pub fn log(&self, level: Level, message: &str, fields: &[(&str, String)]) {
        if level < self.level {
            return;
        }

        let mut line = format!("{} {} ", crate::sys::clock::local_hms(), level.label());
        if let Some(service) = &self.service {
            line.push('[');
            line.push_str(service);
            line.push_str("] ");
        }
        line.push_str(message);
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(value);
        }
        emit(&line);
    }
}

/// Serialises whole lines so concurrent services do not interleave mid-line.
fn emit(line: &str) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    eprintln!("{line}");
}

/// Logging macros, so call sites read as one line:
///
/// ```ignore
/// info!(log, "listening", service = name, addr = addr);
/// ```
///
/// Field values must implement [`std::fmt::Display`]; the key is the identifier
/// you wrote, stringified.
#[macro_export]
macro_rules! debug {
    ($log:expr, $msg:expr $(, $key:ident = $value:expr)* $(,)?) => {
        $log.debug($msg, &[$((stringify!($key), $value.to_string())),*])
    };
}

#[macro_export]
macro_rules! info {
    ($log:expr, $msg:expr $(, $key:ident = $value:expr)* $(,)?) => {
        $log.info($msg, &[$((stringify!($key), $value.to_string())),*])
    };
}

#[macro_export]
macro_rules! warn {
    ($log:expr, $msg:expr $(, $key:ident = $value:expr)* $(,)?) => {
        $log.warn($msg, &[$((stringify!($key), $value.to_string())),*])
    };
}

#[macro_export]
macro_rules! log_error {
    ($log:expr, $msg:expr $(, $key:ident = $value:expr)* $(,)?) => {
        $log.error($msg, &[$((stringify!($key), $value.to_string())),*])
    };
}
