//! 本地限量日志 + panic 钩子。日志只记录错误类别和必要上下文。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_BYTES: u64 = 512 * 1024;

struct Logger {
    file: Mutex<Option<File>>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(dir: &std::path::Path) {
    let path = dir.join("log.txt");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let _ = std::fs::rename(&path, dir.join("log.old.txt"));
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(&path).ok();
    let _ = LOGGER.set(Logger { file: Mutex::new(file) });
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
    }));
}

pub fn log(msg: &str) {
    let Some(logger) = LOGGER.get() else { return };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("{ts} {msg}\n");
    if let Ok(mut guard) = logger.file.lock() {
        if let Some(f) = guard.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

