//! Live "entries scanned" line on stderr while a scan runs.

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cleanmcp_core::ScanControl;

use crate::output::human;

/// Prints progress until dropped. Does nothing when stderr is not a terminal.
pub struct Reporter {
    stop: Option<mpsc::Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl Reporter {
    pub fn start(control: Arc<ScanControl>) -> Self {
        if !std::io::stderr().is_terminal() {
            return Reporter {
                stop: None,
                handle: None,
            };
        }
        let (tx, rx) = mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            let started = Instant::now();
            let mut shown = false;
            // Wake every 200 ms until the sender is dropped.
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(Duration::from_millis(200)) {
                let n = control.entries.load(Relaxed);
                let secs = started.elapsed().as_secs_f64().max(0.001);
                let line = format!(
                    "Scanning... {n} entries, {} ({:.0}/s)",
                    human(control.bytes.load(Relaxed)),
                    n as f64 / secs
                );
                // Pad instead of an ANSI "clear line" so old Windows consoles work too.
                eprint!("\r{line:<79}");
                let _ = std::io::stderr().flush();
                shown = true;
            }
            if shown {
                eprint!("\r{:79}\r", "");
                let _ = std::io::stderr().flush();
            }
        });
        Reporter {
            stop: Some(tx),
            handle: Some(handle),
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
