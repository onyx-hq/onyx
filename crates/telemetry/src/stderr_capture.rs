//! Make every line on a server pod's stderr a structured JSON line.
//!
//! `tracing` is not the only thing that writes to fd 2. Dependencies call
//! `eprintln!` (airlayer's semantic-model validator prints its `Warning: […]`
//! set on **every** engine build), C and C++ libraries write straight to the
//! descriptor, and panics print plain text. The cluster's log shipper tails the
//! container stream, so each of those lines arrived in HyperDX with no level,
//! no target, no service and no trace id — and repeated once per build.
//! Measured on prod on 2026-09-10: **46%** of `oxy-worker`'s lines and 33% of
//! `oxy-ide`'s were airlayer's warnings alone.
//!
//! The fix lives at the descriptor, not at each writer, because the writers are
//! not ours:
//!
//! 1. fd 2 is replaced by a pipe; a dedicated thread reads it line by line.
//! 2. Oxy's own JSON layer writes to a duplicate of the **original** stderr
//!    ([`StderrCapture::writer`]), so its lines never wait on the pipe and are
//!    never lost to it.
//! 3. Everything else that reaches the pipe is rewritten into the same JSON
//!    shape `OxyJson` emits — `timestamp`, `level`, `message`, `target`,
//!    `service` — with `captured: "stderr"` so its origin stays visible.
//! 4. An identical non-`ERROR` stray line repeated within [`REPEAT_WINDOW`] is
//!    suppressed and counted. The count is emitted as `repeats_suppressed` on
//!    the line's next emission, or on a `repeat_summary` line once the window
//!    has passed with no new occurrence, or at shutdown — so a line seen twice
//!    is never reported as seen once. `ERROR` lines are never suppressed.
//! 5. [`report_panic`] writes a panic as **one** JSON `ERROR` line — message,
//!    location, thread, backtrace — **directly** to the original stderr, so it
//!    survives a process that dies before the reader thread drains the pipe.
//!    The process's real panic hook is installed later by `oxy_app::cli` and
//!    *replaces* whatever is there, so that hook calls [`report_panic`]; the
//!    hook [`install`] sets only covers the window before `cli()` runs.
//! 6. If the reader ever fails, fd 2 is pointed back at the original stderr
//!    before the thread exits. The degraded state is "stray lines are
//!    unstructured again", never "fd 2 is a pipe nobody reads" — which would
//!    turn the next `eprintln!` into `EPIPE`, a panic, and an abort.
//!
//! Installed by `oxy-server`'s `logging::init` for `serve` / `start` / `worker`
//! in the JSON (cloud) log format only; a terminal keeps its plain stderr.
//! `OXY_STDERR_CAPTURE=off` disables it. [`finish`] restores fd 2 and drains
//! the pipe at shutdown.

use std::collections::HashMap;
use std::io::{self, BufRead, Read};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Identical stray lines inside this window are collapsed into one.
pub const REPEAT_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How often the reader looks for suppressed counts whose window has passed.
const REPEAT_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// A stray line longer than this is emitted in pieces rather than buffered
/// without bound — a C library can write megabytes without a newline.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// Distinct stray lines remembered for repeat suppression. Past this, entries
/// whose window has elapsed are dropped, and if that frees nothing the map is
/// cleared — worst case a repeat is emitted once more, never memory growth.
const MAX_TRACKED_LINES: usize = 1024;

/// Environment switch: `off` / `false` / `0` disables the capture.
pub const DISABLE_ENV: &str = "OXY_STDERR_CAPTURE";

/// Whether the environment allows the capture.
pub fn enabled_by_env() -> bool {
    !std::env::var(DISABLE_ENV).is_ok_and(|v| {
        let v = v.trim();
        v.eq_ignore_ascii_case("off") || v.eq_ignore_ascii_case("false") || v == "0"
    })
}

struct Seen {
    last_emitted: Instant,
    suppressed: u64,
}

/// Where the reader is inside the default panic hook's plain-text block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PanicBlock {
    Outside,
    /// After `thread '…' panicked at …:` — the message lines follow.
    Message(usize),
    /// After `stack backtrace:` — indented frames follow.
    Backtrace(usize),
}

/// A default-hook block longer than this is not a block we recognise; stop
/// dropping lines rather than risk swallowing unrelated output.
const MAX_PANIC_BLOCK_LINES: usize = 400;

/// The pure half: turns one raw stderr line into the JSON line to write, or
/// `None` when it is suppressed. Kept free of I/O so it is unit-testable.
pub struct Normalizer {
    service: Option<String>,
    seen: HashMap<String, Seen>,
    panic_block: PanicBlock,
    last_sweep: Option<Instant>,
    /// Summary lines for entries that left `seen` still holding a count.
    pending: Vec<String>,
}

impl Normalizer {
    pub fn new(service: Option<String>) -> Self {
        Self {
            service,
            seen: HashMap::new(),
            panic_block: PanicBlock::Outside,
            last_sweep: None,
            pending: Vec::new(),
        }
    }

    /// One raw line, with or without its newline. The returned string is one
    /// JSON object ending in `\n`.
    pub fn process(&mut self, line: &str, now: Instant) -> Option<String> {
        let text = line.trim_end_matches(['\r', '\n']);
        let trimmed = text.trim();
        // Already structured (a JSON writer that still targets fd 2): pass it
        // through untouched rather than wrapping JSON in JSON.
        if trimmed.starts_with('{') && trimmed.ends_with('}') {
            self.panic_block = PanicBlock::Outside;
            return Some(format!("{text}\n"));
        }
        if self.inside_panic_block(text) {
            return None;
        }
        if trimmed.is_empty() {
            return None;
        }
        let class = classify(text);
        // An error is never collapsed: the second occurrence of a failure is
        // information, and a count that only surfaces ten minutes later is not
        // where anyone scanning for errors will look.
        let repeats = if class.level == "ERROR" {
            0
        } else {
            self.admit(text, now)?
        };
        Some(self.render(text, &class, repeats, false))
    }

    /// Summary lines for suppressed repeats whose window has passed without a
    /// new occurrence — otherwise a line seen exactly twice would report only
    /// once, forever. Cheap to call on every read: it scans at most every
    /// [`REPEAT_SWEEP_INTERVAL`].
    pub fn take_elapsed_repeats(&mut self, now: Instant) -> Vec<String> {
        let throttled = self
            .last_sweep
            .is_some_and(|t| now.duration_since(t) < REPEAT_SWEEP_INTERVAL);
        if !throttled {
            self.last_sweep = Some(now);
            let due = self.texts_where(|seen| {
                seen.suppressed > 0 && now.duration_since(seen.last_emitted) >= REPEAT_WINDOW
            });
            self.evict(due);
        }
        // Counts of entries evicted at the tracking ceiling are never throttled.
        std::mem::take(&mut self.pending)
    }

    /// Every outstanding suppressed count, window or not — for shutdown.
    pub fn flush_repeats(&mut self) -> Vec<String> {
        let due = self.texts_where(|seen| seen.suppressed > 0);
        self.evict(due);
        std::mem::take(&mut self.pending)
    }

    fn texts_where(&self, pick: impl Fn(&Seen) -> bool) -> Vec<String> {
        self.seen
            .iter()
            .filter(|(_, seen)| pick(seen))
            .map(|(text, _)| text.clone())
            .collect()
    }

    /// Remove entries from `seen`. One that still holds a count leaves a
    /// summary line in `pending` — this is the only way an entry leaves the
    /// map, so no count is ever dropped, the tracking ceiling included.
    fn evict(&mut self, texts: Vec<String>) {
        for text in texts {
            if let Some(seen) = self.seen.remove(&text)
                && seen.suppressed > 0
            {
                let class = classify(&text);
                let line = self.render(&text, &class, seen.suppressed, true);
                self.pending.push(line);
            }
        }
    }

    /// The default panic hook prints `thread '…' panicked at loc:`, the
    /// message, then either a `note: run with RUST_BACKTRACE=1` line or a
    /// `stack backtrace:` and its frames. Only reachable before `cli()`
    /// replaces the process's hook (see the module doc): there, the capture's
    /// own hook has already written the panic as one structured line and then
    /// runs the previous hook, whose plain-text copy is dropped here.
    fn inside_panic_block(&mut self, text: &str) -> bool {
        let trimmed = text.trim_start();
        // Current Rust puts the thread id between the name and `panicked at`:
        // `thread 'main' (1234) panicked at src/main.rs:1:1:`.
        if trimmed.starts_with("thread '") && trimmed.contains(" panicked at ") {
            self.panic_block = PanicBlock::Message(0);
            return true;
        }
        match self.panic_block {
            PanicBlock::Outside => false,
            PanicBlock::Message(n) | PanicBlock::Backtrace(n) if n >= MAX_PANIC_BLOCK_LINES => {
                self.panic_block = PanicBlock::Outside;
                false
            }
            PanicBlock::Message(n) => {
                if trimmed.starts_with("note: run with `RUST_BACKTRACE") {
                    self.panic_block = PanicBlock::Outside;
                } else if trimmed == "stack backtrace:" {
                    self.panic_block = PanicBlock::Backtrace(n + 1);
                } else {
                    self.panic_block = PanicBlock::Message(n + 1);
                }
                true
            }
            PanicBlock::Backtrace(n) => {
                let frame = text.starts_with(' ') || text.starts_with('\t');
                if frame || trimmed.starts_with("note: Some details are omitted") {
                    self.panic_block = PanicBlock::Backtrace(n + 1);
                    true
                } else {
                    self.panic_block = PanicBlock::Outside;
                    false
                }
            }
        }
    }

    /// `Some(suppressed_since_last_emit)` when the line should be written.
    fn admit(&mut self, text: &str, now: Instant) -> Option<u64> {
        if let Some(entry) = self.seen.get_mut(text) {
            if now.duration_since(entry.last_emitted) < REPEAT_WINDOW {
                entry.suppressed += 1;
                return None;
            }
            let repeats = entry.suppressed;
            entry.last_emitted = now;
            entry.suppressed = 0;
            return Some(repeats);
        }
        if self.seen.len() >= MAX_TRACKED_LINES {
            let elapsed =
                self.texts_where(|seen| now.duration_since(seen.last_emitted) >= REPEAT_WINDOW);
            self.evict(elapsed);
            if self.seen.len() >= MAX_TRACKED_LINES {
                let all = self.texts_where(|_| true);
                self.evict(all);
            }
        }
        self.seen.insert(
            text.to_string(),
            Seen {
                last_emitted: now,
                suppressed: 0,
            },
        );
        Some(0)
    }

    fn render(&self, text: &str, class: &Class<'_>, repeats: u64, summary: bool) -> String {
        let mut line = JsonLine::new(class.level, text, class.target, self.service.as_deref());
        line.field("captured", "stderr");
        if let Some(view) = class.semantic_view {
            line.field("semantic_view", view);
        }
        if repeats > 0 {
            line.raw("repeats_suppressed", &repeats.to_string());
        }
        if summary {
            line.raw("repeat_summary", "true");
        }
        line.finish()
    }
}

/// How [`pump`] stopped reading.
#[derive(Debug)]
pub enum PumpEnd {
    /// Every write end is closed — the normal end, after `finish`.
    Eof,
    /// A read failed for a reason retrying will not fix.
    Failed(io::Error),
}

/// The reader loop, generic over the source so its failure handling is
/// testable without a real pipe. Reads line by line (bounded by
/// [`MAX_LINE_BYTES`]), hands each rendered line to `write`, and emits due
/// repeat summaries as it goes. `Interrupted` and `WouldBlock` are retried; any
/// other error ends the loop with [`PumpEnd::Failed`] so the caller can restore
/// fd 2. Outstanding repeat counts are flushed on either ending.
pub fn pump<R: Read>(
    reader: R,
    normalizer: &mut Normalizer,
    mut write: impl FnMut(&[u8]),
) -> PumpEnd {
    let mut reader = io::BufReader::new(reader);
    // `buf` is cleared only once a line has been emitted, so a retried read
    // keeps whatever `read_until` had already appended: a retry never drops
    // part of a line.
    let mut buf = Vec::with_capacity(1024);
    let end = loop {
        // Never zero: a full buffer is emitted below before the next read.
        let budget = (MAX_LINE_BYTES - buf.len()) as u64;
        match (&mut reader).take(budget).read_until(b'\n', &mut buf) {
            Ok(0) => {
                if !buf.is_empty() {
                    emit_line(&buf, normalizer, &mut write);
                }
                break PumpEnd::Eof;
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Not expected on a blocking pipe; never spin if it happens.
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => {
                if !buf.is_empty() {
                    emit_line(&buf, normalizer, &mut write);
                }
                break PumpEnd::Failed(e);
            }
        }
        if buf.last() == Some(&b'\n') || buf.len() >= MAX_LINE_BYTES {
            emit_line(&buf, normalizer, &mut write);
            buf.clear();
        }
    };
    for summary in normalizer.flush_repeats() {
        write(summary.as_bytes());
    }
    end
}

/// Render one raw line (plus any repeat summaries now due) through `write`.
fn emit_line(bytes: &[u8], normalizer: &mut Normalizer, write: &mut impl FnMut(&[u8])) {
    let line = String::from_utf8_lossy(bytes);
    let now = Instant::now();
    // A bug in the normalizer must never cost the line itself.
    let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        normalizer.process(&line, now)
    }))
    .unwrap_or_else(|_| Some(format!("{}\n", line.trim_end_matches('\n'))));
    if let Some(out) = rendered {
        write(out.as_bytes());
    }
    for summary in normalizer.take_elapsed_repeats(now) {
        write(summary.as_bytes());
    }
}

/// A JSON object written key by key, so the keys come out in the order
/// `OxyJson` uses — `timestamp`, `level`, `message`, `target`, `service` —
/// and a line read with `kubectl logs` scans the same way as Oxy's own.
/// (`serde_json::Map` sorts its keys.)
struct JsonLine(String);

impl JsonLine {
    fn new(level: &str, message: &str, target: &str, service: Option<&str>) -> Self {
        let mut line = JsonLine(String::from("{"));
        let ts = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        line.field("timestamp", &ts);
        line.field("level", level);
        line.field("message", message);
        line.field("target", target);
        if let Some(service) = service {
            line.field("service", service);
        }
        line
    }

    fn field(&mut self, key: &str, value: &str) {
        self.raw(key, &Value::String(value.to_string()).to_string());
    }

    fn raw(&mut self, key: &str, json: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        self.0.push_str(&Value::String(key.to_string()).to_string());
        self.0.push(':');
        self.0.push_str(json);
    }

    fn finish(mut self) -> String {
        self.0.push_str("}\n");
        self.0
    }
}

/// What a stray line is, as far as its text says.
#[derive(Debug, PartialEq, Eq)]
pub struct Class<'a> {
    pub level: &'static str,
    pub target: &'static str,
    /// airlayer's validator names the view in brackets: `Warning: [orders] …`.
    pub semantic_view: Option<&'a str>,
}

/// Level and target from the text of a line nobody structured.
///
/// airlayer's model-validation warnings are `INFO`, not `WARN`: they describe
/// a workspace's semantic model (a shadowed or ambiguous measure), not a fault
/// in the platform, and at `WARN` they would outnumber every real warning the
/// fleet emits. They get their own target so `target:airlayer` finds them.
pub fn classify(text: &str) -> Class<'_> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix("Warning: [") {
        return Class {
            level: "INFO",
            target: "airlayer",
            semantic_view: rest.split_once(']').map(|(view, _)| view),
        };
    }
    let lower = trimmed
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let level = if trimmed.contains("panicked at")
        || lower.starts_with("error")
        || lower.starts_with("fatal")
    {
        "ERROR"
    } else if lower.starts_with("warn") {
        "WARN"
    } else {
        "INFO"
    };
    Class {
        level,
        target: "stderr",
        semantic_view: None,
    }
}

/// What a panic hook knows about one panic.
pub struct PanicReport<'a> {
    pub thread: &'a str,
    pub location: &'a str,
    pub message: &'a str,
    pub backtrace: Option<&'a str>,
}

/// The JSON line a panic becomes. Pure, for the test.
pub fn panic_line(service: Option<&str>, report: &PanicReport<'_>) -> String {
    let message = format!(
        "panic in thread '{}' at {}: {}",
        report.thread, report.location, report.message
    );
    let mut line = JsonLine::new("ERROR", &message, "panic", service);
    line.field("thread", report.thread);
    line.field("panic.location", report.location);
    line.field("panic.message", report.message);
    if let Some(bt) = report.backtrace {
        line.field("panic.backtrace", bt);
    }
    line.finish()
}

#[cfg(unix)]
pub use unix::{StderrCapture, finish, install, report_panic};

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io::{self, Write};
    use std::os::fd::FromRawFd;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    use tracing_subscriber::fmt::MakeWriter;

    use super::{Normalizer, PanicReport, PumpEnd, panic_line, pump};

    /// The original stderr, shared by the JSON layer, the reader thread and
    /// the panic line, so their writes never interleave mid-line.
    #[derive(Clone)]
    pub struct StderrCapture {
        original: Arc<Mutex<File>>,
        /// The same descriptor, for lock-free fallbacks and restoring fd 2.
        original_raw: i32,
    }

    pub struct Guard<'a>(MutexGuard<'a, File>);

    impl Write for Guard<'_> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    impl<'a> MakeWriter<'a> for StderrCapture {
        type Writer = Guard<'a>;
        fn make_writer(&'a self) -> Self::Writer {
            Guard(self.original.lock().unwrap_or_else(|e| e.into_inner()))
        }
    }

    impl StderrCapture {
        /// The writer Oxy's own JSON layer should use: the real stderr.
        pub fn writer(&self) -> Self {
            self.clone()
        }

        /// Write straight to the original descriptor. `try_lock`, because a
        /// panic can happen while this thread holds the writer; the fallback
        /// is an unsynchronised write rather than a deadlock.
        fn write_direct(&self, bytes: &[u8]) {
            match self.original.try_lock() {
                Ok(mut f) => {
                    let _ = f.write_all(bytes);
                }
                Err(_) => {
                    let mut rest = bytes;
                    while !rest.is_empty() {
                        let n = unsafe {
                            libc::write(self.original_raw, rest.as_ptr().cast(), rest.len())
                        };
                        if n <= 0 {
                            break;
                        }
                        rest = &rest[n as usize..];
                    }
                }
            }
        }
    }

    struct Installed {
        original_fd: i32,
        capture: StderrCapture,
        service: Option<String>,
        done: Mutex<Option<mpsc::Receiver<()>>>,
    }

    static INSTALLED: OnceLock<Installed> = OnceLock::new();

    /// Redirect fd 2 into the normalizer. Returns the writer for Oxy's own
    /// JSON layer. Idempotent: a second call returns an error and changes
    /// nothing.
    pub fn install(service: Option<String>) -> io::Result<StderrCapture> {
        if INSTALLED.get().is_some() {
            return Err(io::Error::other("stderr capture already installed"));
        }
        // SAFETY: plain descriptor syscalls; every returned fd is checked and
        // owned by exactly one `File` (or kept as the raw original below).
        let original_fd = unsafe { libc::dup(2) };
        if original_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(original_fd) };
            return Err(err);
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Neither pipe end nor the saved original should leak into children
        // we exec (git, embedded postgres): they get fd 2, which is the pipe,
        // and that is enough.
        for fd in [read_fd, write_fd, original_fd] {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }

        let original_for_writer = unsafe { libc::dup(original_fd) };
        if original_for_writer < 0 {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
                libc::close(original_fd);
            }
            return Err(err);
        }
        unsafe { libc::fcntl(original_for_writer, libc::F_SETFD, libc::FD_CLOEXEC) };
        let capture = StderrCapture {
            original: Arc::new(Mutex::new(unsafe {
                File::from_raw_fd(original_for_writer)
            })),
            original_raw: original_for_writer,
        };

        let (done_tx, done_rx) = mpsc::channel();
        let reader = unsafe { File::from_raw_fd(read_fd) };
        let sink = capture.clone();
        let reader_service = service.clone();
        std::thread::Builder::new()
            .name("oxy-stderr-capture".into())
            .spawn(move || {
                drain(reader, &sink, reader_service);
                let _ = done_tx.send(());
            })?;

        // Point fd 2 at the pipe only once the reader is running.
        if unsafe { libc::dup2(write_fd, 2) } < 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::close(write_fd) };
            return Err(err);
        }
        unsafe { libc::close(write_fd) };

        let _ = INSTALLED.set(Installed {
            original_fd,
            capture,
            service,
            done: Mutex::new(Some(done_rx)),
        });
        install_early_panic_hook();
        Ok(INSTALLED
            .get()
            .map(|i| i.capture.clone())
            .expect("just installed"))
    }

    /// Restore the original stderr and wait up to `timeout` for the reader to
    /// write out what was still in the pipe. Call it last, after the OTLP
    /// shutdown. A no-op when the capture was never installed.
    pub fn finish(timeout: Duration) {
        let Some(installed) = INSTALLED.get() else {
            return;
        };
        // Once fd 2 is the original again, this process holds no write end of
        // the pipe; the reader sees EOF when any child that inherited it exits.
        unsafe { libc::dup2(installed.original_fd, 2) };
        let rx = installed
            .done
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(rx) = rx {
            let _ = rx.recv_timeout(timeout);
        }
    }

    /// Write `info` as one structured `panic` line to the original stderr, if
    /// the capture is installed. Returns whether it wrote — a hook that gets
    /// `false` should print the panic itself, because nothing else will.
    ///
    /// This is for the process's real panic hook (`oxy_app::cli`), which
    /// replaces any hook installed before it rather than chaining — chaining
    /// would re-run Sentry's panic integration beside its own
    /// `capture_message` and report every panic twice. The backtrace is always
    /// captured, as that hook always did.
    pub fn report_panic(info: &std::panic::PanicHookInfo<'_>) -> bool {
        let Some(installed) = INSTALLED.get() else {
            return false;
        };
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("<unnamed>").to_string();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".into());
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        let line = panic_line(
            installed.service.as_deref(),
            &PanicReport {
                thread: &thread,
                location: &location,
                message: &message,
                backtrace: Some(&backtrace),
            },
        );
        installed.capture.write_direct(line.as_bytes());
        true
    }

    /// Covers panics between `install` and the process hook `cli()` sets.
    /// Chains to the previous hook (Sentry's integration, `human_panic`, or
    /// the default), whose plain-text copy the normalizer drops.
    fn install_early_panic_hook() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            report_panic(info);
            previous(info);
        }));
    }

    fn drain(reader: File, sink: &StderrCapture, service: Option<String>) {
        let mut normalizer = Normalizer::new(service);
        let end = pump(reader, &mut normalizer, |bytes| {
            let mut w = sink.make_writer();
            let _ = w.write_all(bytes);
        });
        if let PumpEnd::Failed(e) = end {
            // Returning drops the pipe's read end. If fd 2 still pointed at the
            // write end, every later `eprintln!` would hit EPIPE and panic —
            // and the panic hook's own output would make it an abort. Put the
            // real stderr back first, then say what happened.
            unsafe { libc::dup2(sink.original_raw, 2) };
            let line = super::JsonLine::new(
                "ERROR",
                &format!(
                    "stderr capture stopped; stray stderr lines are unstructured from here: {e}"
                ),
                "oxy_telemetry::stderr_capture",
                None,
            )
            .finish();
            sink.write_direct(line.as_bytes());
        }
    }
}

#[cfg(not(unix))]
pub fn finish(_timeout: Duration) {}

#[cfg(not(unix))]
pub fn report_panic(_info: &std::panic::PanicHookInfo<'_>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Value {
        assert!(
            line.ends_with('\n'),
            "one line, newline-terminated: {line:?}"
        );
        serde_json::from_str(line.trim_end()).expect("valid JSON")
    }

    #[test]
    fn a_stray_line_becomes_the_same_json_shape_oxy_emits() {
        let mut n = Normalizer::new(Some("oxy-worker".into()));
        let out = n
            .process("something went sideways\n", Instant::now())
            .unwrap();
        let v = parse(&out);
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["message"], "something went sideways");
        assert_eq!(v["target"], "stderr");
        assert_eq!(v["service"], "oxy-worker");
        assert_eq!(v["captured"], "stderr");
        assert!(v["timestamp"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn airlayer_model_warnings_are_info_under_their_own_target_with_the_view() {
        let line = "Warning: [airhouse_orders] explicit measure '__oxy_row_count' shadows promoted measure(s) from airhouse_order_checks[order] — the induced measure is not exposed.";
        let v = parse(&Normalizer::new(None).process(line, Instant::now()).unwrap());
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["target"], "airlayer");
        assert_eq!(v["semantic_view"], "airhouse_orders");
        assert!(v.get("service").is_none());
    }

    #[test]
    fn levels_follow_the_text() {
        assert_eq!(
            classify("thread 'main' panicked at src/x.rs:1:1:").level,
            "ERROR"
        );
        assert_eq!(classify("Error: connection refused").level, "ERROR");
        assert_eq!(classify("FATAL: role does not exist").level, "ERROR");
        assert_eq!(classify("warning: deprecated").level, "WARN");
        assert_eq!(classify("note: run with RUST_BACKTRACE=1").level, "INFO");
    }

    #[test]
    fn a_repeat_inside_the_window_is_suppressed_and_counted_on_the_next_emit() {
        let mut n = Normalizer::new(None);
        let t0 = Instant::now();
        assert!(n.process("Warning: [v] same", t0).is_some());
        for i in 1..=9 {
            assert!(
                n.process("Warning: [v] same", t0 + Duration::from_secs(60 * i))
                    .is_none(),
                "minute {i} is inside the window"
            );
        }
        let again = n
            .process(
                "Warning: [v] same",
                t0 + REPEAT_WINDOW + Duration::from_secs(1),
            )
            .expect("emitted again once the window has elapsed");
        assert_eq!(parse(&again)["repeats_suppressed"], 9);
        // And the count resets.
        let later = n
            .process(
                "Warning: [v] same",
                t0 + REPEAT_WINDOW * 2 + Duration::from_secs(2),
            )
            .unwrap();
        assert!(parse(&later).get("repeats_suppressed").is_none());
    }

    #[test]
    fn errors_are_never_collapsed() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        for _ in 0..3 {
            let v = parse(&n.process("Error: connection refused", t).unwrap());
            assert!(v.get("repeats_suppressed").is_none());
        }
    }

    #[test]
    fn a_line_seen_twice_reports_its_second_occurrence_once_the_window_passes() {
        let mut n = Normalizer::new(None);
        let t0 = Instant::now();
        assert!(n.process("warning: disk nearly full", t0).is_some());
        assert!(
            n.process("warning: disk nearly full", t0 + Duration::from_secs(60))
                .is_none()
        );
        assert!(
            n.take_elapsed_repeats(t0 + Duration::from_secs(120))
                .is_empty(),
            "inside the window: nothing due yet"
        );
        let due = n.take_elapsed_repeats(t0 + REPEAT_WINDOW + Duration::from_secs(60));
        assert_eq!(due.len(), 1);
        let v = parse(&due[0]);
        assert_eq!(v["repeats_suppressed"], 1);
        assert_eq!(v["repeat_summary"], true);
        assert_eq!(v["level"], "WARN");
        assert!(
            n.flush_repeats().is_empty(),
            "a reported count is not reported again"
        );
    }

    #[test]
    fn shutdown_flushes_outstanding_counts() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        n.process("Warning: [v] x", t);
        n.process("Warning: [v] x", t);
        n.process("Warning: [v] x", t);
        let flushed = n.flush_repeats();
        assert_eq!(flushed.len(), 1);
        assert_eq!(parse(&flushed[0])["repeats_suppressed"], 2);
    }

    #[test]
    fn different_lines_are_not_collapsed() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        assert!(n.process("Warning: [a] x", t).is_some());
        assert!(n.process("Warning: [b] x", t).is_some());
    }

    #[test]
    fn json_passes_through_untouched_and_blank_lines_vanish() {
        let mut n = Normalizer::new(Some("oxy-serve".into()));
        let json = r#"{"level":"INFO","message":"already structured"}"#;
        assert_eq!(
            n.process(json, Instant::now()).unwrap(),
            format!("{json}\n")
        );
        assert_eq!(
            n.process(json, Instant::now()).unwrap(),
            format!("{json}\n"),
            "structured lines are never deduplicated"
        );
        assert!(n.process("   \n", Instant::now()).is_none());
    }

    #[test]
    fn counts_evicted_at_the_tracking_ceiling_are_reported_not_dropped() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        for i in 0..MAX_TRACKED_LINES {
            n.process(&format!("warning: {i}"), t);
            n.process(&format!("warning: {i}"), t); // suppressed once
        }
        // Nothing has elapsed, so the next new line clears the whole set.
        n.process("warning: one too many", t);
        let reported = n.take_elapsed_repeats(t);
        assert_eq!(reported.len(), MAX_TRACKED_LINES);
        assert!(reported.iter().all(|l| parse(l)["repeats_suppressed"] == 1));
    }

    #[test]
    fn the_tracked_set_is_bounded() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        for i in 0..(MAX_TRACKED_LINES * 3) {
            n.process(&format!("line {i}"), t);
        }
        assert!(n.seen.len() <= MAX_TRACKED_LINES);
    }

    /// A reader that yields its chunks in order, then the given error.
    struct Scripted {
        chunks: Vec<io::Result<&'static [u8]>>,
    }

    impl Read for Scripted {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.chunks.is_empty() {
                return Ok(0);
            }
            match self.chunks.remove(0) {
                Ok(bytes) => {
                    buf[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                }
                Err(e) => Err(e),
            }
        }
    }

    #[test]
    fn a_fatal_read_error_ends_the_pump_as_failed_after_writing_what_it_had() {
        let reader = Scripted {
            chunks: vec![
                Ok(b"first\n"),
                Err(io::Error::from(io::ErrorKind::Interrupted)),
                Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Ok(b"second\n"),
                Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                Ok(b"never read\n"),
            ],
        };
        let mut n = Normalizer::new(None);
        let mut written = Vec::new();
        let end = pump(reader, &mut n, |b| {
            written.push(String::from_utf8_lossy(b).to_string())
        });
        assert!(
            matches!(end, PumpEnd::Failed(ref e) if e.kind() == io::ErrorKind::BrokenPipe),
            "{end:?}"
        );
        let messages: Vec<String> = written
            .iter()
            .map(|l| parse(l)["message"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            messages,
            ["first", "second"],
            "Interrupted and WouldBlock retry; the fatal error stops"
        );
    }

    #[test]
    fn a_retried_read_keeps_the_part_of_the_line_already_read() {
        let reader = Scripted {
            chunks: vec![
                Ok(b"hal"),
                Err(io::Error::from(io::ErrorKind::WouldBlock)),
                Ok(b"f a line\n"),
            ],
        };
        let mut n = Normalizer::new(None);
        let mut written = Vec::new();
        let end = pump(reader, &mut n, |b| {
            written.push(String::from_utf8_lossy(b).to_string())
        });
        assert!(matches!(end, PumpEnd::Eof));
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(parse(&written[0])["message"], "half a line");
    }

    #[test]
    fn eof_ends_the_pump_cleanly_and_flushes_counts() {
        let reader = Scripted {
            chunks: vec![Ok(b"warning: x\n"), Ok(b"warning: x\n")],
        };
        let mut n = Normalizer::new(None);
        let mut written = Vec::new();
        let end = pump(reader, &mut n, |b| {
            written.push(String::from_utf8_lossy(b).to_string())
        });
        assert!(matches!(end, PumpEnd::Eof));
        assert_eq!(written.len(), 2, "the line, then its flushed count");
        assert_eq!(parse(&written[1])["repeats_suppressed"], 1);
    }

    /// The child half of [`the_descriptor_redirect_works_in_a_real_process`]:
    /// a no-op unless that test launched this binary with the env var set.
    #[cfg(unix)]
    #[test]
    fn child_process_body() {
        if std::env::var("OXY_STDERR_CAPTURE_CHILD").is_err() {
            return;
        }
        let writer = install(Some("oxy-test".into())).expect("installed");
        for _ in 0..3 {
            eprintln!("Warning: [orders] explicit measure 'x' shadows promoted measure(s)");
        }
        eprintln!("plain stray line");
        {
            use std::io::Write as _;
            use tracing_subscriber::fmt::MakeWriter as _;
            let mut w = writer.make_writer();
            w.write_all(b"{\"level\":\"INFO\",\"message\":\"ours\"}\n")
                .unwrap();
        }
        let _ = std::thread::spawn(|| panic!("boom in a worker")).join();
        finish(Duration::from_secs(5));
    }

    /// Runs this test binary again as a child with the capture installed and
    /// reads what actually reached its stderr: every line JSON, the repeated
    /// warning collapsed to one plus its flushed count, our own line
    /// untouched, the panic structured. The process hook `oxy_app::cli` sets
    /// is covered by its own test there.
    #[cfg(unix)]
    #[test]
    fn the_descriptor_redirect_works_in_a_real_process() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stderr_capture::tests::child_process_body",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("OXY_STDERR_CAPTURE_CHILD", "1")
            .output()
            .expect("child ran");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let lines: Vec<Value> = stderr
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l)
                    .unwrap_or_else(|e| panic!("non-JSON line on stderr ({e}): {l:?}\n{stderr}"))
            })
            .collect();
        let warnings: Vec<&Value> = lines
            .iter()
            .filter(|v| v["target"] == "airlayer" && v.get("repeat_summary").is_none())
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "three identical warnings collapse to one: {stderr}"
        );
        assert_eq!(warnings[0]["semantic_view"], "orders");
        assert!(
            lines
                .iter()
                .any(|v| v["target"] == "airlayer" && v["repeats_suppressed"] == 2),
            "the two suppressed repeats are reported at shutdown: {stderr}"
        );
        assert!(
            lines
                .iter()
                .any(|v| v["message"] == "plain stray line" && v["service"] == "oxy-test")
        );
        assert!(
            lines
                .iter()
                .any(|v| v["message"] == "ours" && v.get("captured").is_none())
        );
        let panics: Vec<&Value> = lines.iter().filter(|v| v["target"] == "panic").collect();
        assert_eq!(panics.len(), 1, "{stderr}");
        assert_eq!(panics[0]["panic.message"], "boom in a worker");
        assert_eq!(panics[0]["level"], "ERROR");
        assert!(panics[0]["panic.backtrace"].as_str().is_some());
    }

    #[test]
    fn a_panic_is_one_error_line_with_its_location() {
        let v = parse(&panic_line(
            Some("oxy-serve"),
            &PanicReport {
                thread: "tokio-runtime-worker",
                location: "crates/app/src/x.rs:10:5",
                message: "index out of bounds",
                backtrace: Some("   0: std::panicking"),
            },
        ));
        assert_eq!(v["level"], "ERROR");
        assert_eq!(v["target"], "panic");
        assert_eq!(v["panic.location"], "crates/app/src/x.rs:10:5");
        assert_eq!(v["panic.message"], "index out of bounds");
        assert_eq!(v["panic.backtrace"], "   0: std::panicking");
        assert_eq!(v["thread"], "tokio-runtime-worker");
    }

    #[test]
    fn keys_come_out_in_oxy_json_order() {
        let out = Normalizer::new(Some("oxy-ide".into()))
            .process("Warning: [v] x", Instant::now())
            .unwrap();
        let order: Vec<usize> = [
            "\"timestamp\"",
            "\"level\"",
            "\"message\"",
            "\"target\"",
            "\"service\"",
        ]
        .iter()
        .map(|k| out.find(k).unwrap_or_else(|| panic!("{k} missing: {out}")))
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{out}");
    }

    #[test]
    fn the_default_hooks_plain_text_panic_block_is_dropped() {
        let mut n = Normalizer::new(None);
        let t = Instant::now();
        for line in [
            "thread 'tokio-runtime-worker' (123) panicked at crates/x.rs:1:1:",
            "called `Option::unwrap()` on a `None` value",
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace",
        ] {
            assert!(n.process(line, t).is_none(), "{line}");
        }
        assert!(n.process("an unrelated line after it", t).is_some());

        for line in [
            "thread 'main' panicked at crates/y.rs:2:2:",
            "boom",
            "stack backtrace:",
            "   0: rust_begin_unwind",
            "             at /rustc/library/std/src/panicking.rs:697:5",
            "note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.",
        ] {
            assert!(n.process(line, t).is_none(), "{line}");
        }
        assert!(n.process("next real line", t).is_some());
    }
}
