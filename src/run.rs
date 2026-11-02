//! Running things: sources, peeks and actions. Each gets its own thread and posts
//! what happens to the main loop, so nothing a device does can hold up the screen.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering::SeqCst};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_EVERY: u64 = 30;
/// After a command exits, how long to wait for the rest of its output. Not until
/// the pipes close: an ssh that left a shared connection behind holds them open.
const DRAIN: Duration = Duration::from_millis(300);
const OUTPUT_LIMIT: usize = 64 * 1024;
/// How long an unfinished escape sequence is held back waiting for its end.
const HOLD_LIMIT: usize = 4096;
const SHORT_LINE: usize = 80;

pub enum Msg {
    Line { source: String, line: Value },
    /// A source that is still running has said something wrong.
    Trouble { source: String, why: String },
    Exited { source: String, success: bool, why: String },
    Peek { generation: u64, key: String, label: String, run: Vec<String>, text: String },
    JobOutput { job: usize, text: String },
    JobDone { job: usize, outcome: Result<i32, String> },
}

/// What a device says goes on screen, so it must not be able to steer the
/// terminal: colour codes go, and so does every other control character.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Keep the end of `text` within the limit, on a character boundary.
pub fn keep_tail(text: &mut String) {
    if text.len() > OUTPUT_LIMIT {
        let mut from = text.len() - OUTPUT_LIMIT;
        while !text.is_char_boundary(from) {
            from += 1;
        }
        text.drain(..from);
    }
}

/// How much of `bytes` can be cleaned now: all but a character or an escape sequence
/// that the end of a read has cut in two.
fn complete_prefix(bytes: &[u8]) -> usize {
    let tail = bytes.len().saturating_sub(HOLD_LIMIT);
    if let Some(escape) = bytes[tail..].iter().rposition(|&b| b == 0x1b).map(|at| at + tail) {
        if !escape_is_finished(&bytes[escape..]) {
            return escape;
        }
    }
    for back in 1..=3.min(bytes.len()) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xc0 == 0x80 {
            continue;
        }
        let wanted = match byte {
            0xf0.. => 4,
            0xe0.. => 3,
            0xc0.. => 2,
            _ => 1,
        };
        return if back < wanted { bytes.len() - back } else { bytes.len() };
    }
    bytes.len()
}

fn escape_is_finished(sequence: &[u8]) -> bool {
    match sequence.get(1) {
        None => false,
        Some(b'[') => sequence[2..].iter().any(|b| (b'@'..=b'~').contains(b)),
        Some(b']') => sequence[2..].contains(&0x07) || sequence[2..].contains(&0x1b),
        Some(_) => true,
    }
}

/// Cleans output that arrives in pieces, one stream at a time.
#[derive(Default)]
pub struct Cleaner {
    held: Vec<u8>,
}

impl Cleaner {
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        self.held.extend_from_slice(bytes);
        let rest = self.held.split_off(complete_prefix(&self.held));
        let text = clean(&String::from_utf8_lossy(&self.held));
        self.held = rest;
        text
    }

    pub fn finish(&mut self) -> String {
        let text = clean(&String::from_utf8_lossy(&self.held));
        self.held.clear();
        text
    }
}

/// How to stop a command running on another thread, from anywhere, at any moment.
#[derive(Clone, Default)]
pub struct Stop(Arc<(AtomicBool, AtomicI32)>);

impl Stop {
    pub fn now(&self) {
        self.0 .0.store(true, SeqCst);
        let group = self.0 .1.load(SeqCst);
        if group != 0 {
            // SAFETY: the process group of a command still running: it is cleared
            // as soon as that command has been reaped.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        }
    }
}

/// Run a command to its end, passing its cleaned output along as it comes. Stdin is
/// empty, and it runs in its own process group so that stopping it takes all of it down.
pub fn run_collecting(argv: &[String], timeout: Duration, stop: &Stop, mut on_output: impl FnMut(&str)) -> Result<i32, String> {
    let (program, args) = argv.split_first().ok_or("nothing to run")?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{program}: {e}"))?;
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<Box<dyn Read + Send>> = vec![
        Box::new(child.stdout.take().expect("piped")),
        Box::new(child.stderr.take().expect("piped")),
    ];
    for mut reader in readers {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let mut cleaner = Cleaner::default();
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 || tx.send(cleaner.feed(&buffer[..n])).is_err() {
                    break;
                }
            }
            let _ = tx.send(cleaner.finish());
        });
    }
    drop(tx);
    let started = Instant::now();
    let pid = child.id() as i32;
    stop.0 .1.store(pid, SeqCst);
    let kill = |child: &mut std::process::Child| {
        // SAFETY: the process group we created for this child, which we have not reaped.
        unsafe { libc::killpg(pid, libc::SIGKILL) };
        let _ = child.wait();
    };
    let ended = loop {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            on_output(&chunk);
        }
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status.code().unwrap_or(-1)),
            Err(e) => break Err(e.to_string()),
            Ok(None) => {}
        }
        if stop.0 .0.load(SeqCst) {
            kill(&mut child);
            break Err("stopped".to_string());
        }
        if started.elapsed() > timeout {
            kill(&mut child);
            break Err(format!("no answer after {} s", timeout.as_secs()));
        }
    };
    stop.0 .1.store(0, SeqCst);
    let code = ended?;
    let deadline = Instant::now() + DRAIN;
    while let Ok(chunk) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        on_output(&chunk);
    }
    Ok(code)
}

/// Run a peek. Its answer is posted under `generation`, so a late one can be told apart.
pub fn spawn_peek(generation: u64, key: String, label: String, run: Vec<String>, tx: SyncSender<Msg>) -> Stop {
    let stop = Stop::default();
    let handle = stop.clone();
    thread::spawn(move || {
        let mut text = String::new();
        let outcome = run_collecting(&run, Duration::from_secs(15), &handle, |chunk| {
            text.push_str(chunk);
            keep_tail(&mut text);
        });
        match outcome {
            Ok(0) => {}
            Ok(code) => text.push_str(&format!("\n(exit {code})")),
            Err(why) => text.push_str(&format!("\n({why})")),
        }
        let _ = tx.send(Msg::Peek { generation, key, label, run, text });
    });
    stop
}

pub fn spawn_job(job: usize, argv: Vec<String>, tx: SyncSender<Msg>) {
    thread::spawn(move || {
        let progress = tx.clone();
        let outcome = run_collecting(&argv, Duration::from_secs(120), &Stop::default(), move |chunk| {
            let _ = progress.send(Msg::JobOutput { job, text: chunk.to_string() });
        });
        let _ = tx.send(Msg::JobDone { job, outcome });
    });
}

/// Every executable file in the directory is a source.
pub fn find_sources(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .collect();
    found.sort();
    found
}

/// What's running, so the program can take it all down when it leaves.
#[derive(Clone, Default)]
pub struct Running(Arc<Mutex<HashMap<String, i32>>>);

impl Running {
    pub fn stop_all(&self) {
        for pid in self.0.lock().unwrap().values() {
            // SAFETY: each is the process group of a source we started.
            unsafe { libc::killpg(*pid, libc::SIGTERM) };
        }
    }
}

/// Every line a stream gives, until it ends or `on_line` says to stop. Bytes that aren't
/// text don't end the reading: a source nobody reads from blocks for good.
fn each_line(stream: impl Read, mut on_line: impl FnMut(&str) -> bool) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                if !on_line(String::from_utf8_lossy(&raw).trim()) {
                    return Ok(());
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Run a source for as long as this program runs. A source prints one JSON object
/// per line. It can keep going, or it can exit and be run again `every` seconds
/// later. Its stdin is a pipe that closes when this program ends, however it ends,
/// so a long-running source can notice. The returned sender asks for a run now.
pub fn spawn_source(path: PathBuf, tx: SyncSender<Msg>, running: Running) -> Sender<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let (wake, woken) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut every = DEFAULT_EVERY;
        loop {
            let spawned = Command::new(&path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn();
            let (success, why) = match spawned {
                Err(e) => (false, e.to_string()),
                Ok(mut child) => {
                    running.0.lock().unwrap().insert(name.clone(), child.id() as i32);
                    let _stdin = child.stdin.take();
                    let last_error = Arc::new(Mutex::new(String::new()));
                    if let Some(stderr) = child.stderr.take() {
                        let last_error = last_error.clone();
                        thread::spawn(move || {
                            let _ = each_line(stderr, |line| {
                                if !line.is_empty() {
                                    *last_error.lock().unwrap() = clean(line);
                                }
                                true
                            });
                        });
                    }
                    let mut wrong = None;
                    let mut nobody_listening = false;
                    if let Some(stdout) = child.stdout.take() {
                        let read = each_line(stdout, |line| {
                            if line.is_empty() {
                                return true;
                            }
                            let sent = match serde_json::from_str::<Value>(line) {
                                Err(_) if wrong.is_some() => Ok(()),
                                Err(_) => {
                                    let short: String = clean(line).chars().take(SHORT_LINE).collect();
                                    let why = format!("printed something that isn't JSON: {short}");
                                    wrong = Some(why.clone());
                                    tx.send(Msg::Trouble { source: name.clone(), why }).map_err(|_| ())
                                }
                                Ok(value) => match (value["every"].as_u64(), value["id"].is_null()) {
                                    (Some(n), true) => {
                                        every = n.max(1);
                                        Ok(())
                                    }
                                    _ => tx.send(Msg::Line { source: name.clone(), line: value }).map_err(|_| ()),
                                },
                            };
                            nobody_listening = sent.is_err();
                            !nobody_listening
                        });
                        if let Err(e) = read {
                            wrong.get_or_insert(format!("couldn't read its output: {e}"));
                        }
                    }
                    if nobody_listening {
                        return;
                    }
                    let status = child.wait();
                    running.0.lock().unwrap().remove(&name);
                    let why = last_error.lock().unwrap().clone();
                    match (status, wrong) {
                        (Ok(_), Some(wrong)) => (false, wrong),
                        (Ok(s), None) if s.success() => (true, why),
                        (Ok(s), None) => (false, if why.is_empty() { format!("exit {}", s.code().unwrap_or(-1)) } else { why }),
                        (Err(e), _) => (false, e.to_string()),
                    }
                }
            };
            if tx.send(Msg::Exited { source: name.clone(), success, why }).is_err() {
                return;
            }
            let _ = woken.recv_timeout(Duration::from_secs(every));
            while woken.try_recv().is_ok() {}
        }
    });
    wake
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into()]
    }

    #[test]
    fn terminal_control_from_a_device_is_removed() {
        assert_eq!(clean("\u{1b}[31mred\u{1b}[0m\r\n"), "red\n");
        assert_eq!(clean("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(clean("a\u{1b}]0;title\u{1b}\\b"), "ab");
        assert_eq!(clean("a\u{8}\u{7}\u{0}b\tc"), "ab    c");
        assert_eq!(clean("trailing\u{1b}"), "trailing");
        assert_eq!(clean("é ✓"), "é ✓");
    }

    #[test]
    fn output_is_kept_from_the_end() {
        let mut text = "é".repeat(OUTPUT_LIMIT);
        keep_tail(&mut text);
        assert!(text.len() <= OUTPUT_LIMIT);
        assert!(text.ends_with('é'));
    }

    #[test]
    fn a_command_reports_its_output_and_exit_code() {
        let mut out = String::new();
        let code = run_collecting(&argv("echo hi; echo err >&2; exit 3"), Duration::from_secs(5), &Stop::default(), |c| out.push_str(c));
        assert_eq!(code, Ok(3));
        assert!(out.contains("hi") && out.contains("err"));
    }

    #[test]
    fn a_command_that_will_not_finish_is_killed_with_its_children() {
        let t = Instant::now();
        let outcome = run_collecting(&argv("sleep 30 & sleep 30"), Duration::from_millis(300), &Stop::default(), |_| {});
        assert!(outcome.is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        assert!(run_collecting(&["/nonexistent/x".to_string()], Duration::from_secs(1), &Stop::default(), |_| {}).is_err());
        assert!(run_collecting(&[], Duration::from_secs(1), &Stop::default(), |_| {}).is_err());
    }

    #[test]
    fn a_child_holding_the_pipes_open_does_not_hold_up_the_result() {
        let t = Instant::now();
        let code = run_collecting(&argv("sleep 20 & exit 0"), Duration::from_secs(10), &Stop::default(), |_| {});
        assert_eq!(code, Ok(0));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stopping_a_command_takes_all_of_it_down_at_once() {
        let stop = Stop::default();
        let later = stop.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            later.now();
        });
        let t = Instant::now();
        let outcome = run_collecting(&argv("sleep 30 & sleep 30"), Duration::from_secs(25), &stop, |_| {});
        assert_eq!(outcome, Err("stopped".to_string()));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stopping_before_it_starts_still_stops_it() {
        let stop = Stop::default();
        stop.now();
        let t = Instant::now();
        assert!(run_collecting(&argv("sleep 30"), Duration::from_secs(25), &stop, |_| {}).is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn what_a_read_cuts_in_two_waits_for_the_rest() {
        let mut cleaner = Cleaner::default();
        assert_eq!(cleaner.feed(b"a\x1b[3"), "a");
        assert_eq!(cleaner.feed(b"1mb\xc3"), "b");
        assert_eq!(cleaner.feed(b"\xa9\x1b]0;tit"), "é");
        assert_eq!(cleaner.feed(b"le\x07c"), "c");
        assert_eq!(cleaner.feed(b"\xe2\x9c"), "");
        assert_eq!(cleaner.feed(b"\x93"), "✓");
    }

    #[test]
    fn what_never_finishes_is_not_held_for_ever() {
        let mut cleaner = Cleaner::default();
        assert_eq!(cleaner.feed(b"a\x1b[3"), "a");
        assert_eq!(cleaner.finish(), "");
        let mut long = b"\x1b]0;".to_vec();
        long.extend(vec![b'x'; HOLD_LIMIT + 10]);
        assert!(!cleaner.feed(&long).contains('\u{1b}'));
        assert!(cleaner.held.len() <= HOLD_LIMIT);
    }

    #[test]
    fn a_peek_keeps_only_its_tail_while_it_reads() {
        let mut most = 0;
        let mut text = String::new();
        run_collecting(&argv("head -c 400000 /dev/zero | tr '\\0' x"), Duration::from_secs(10), &Stop::default(), |chunk| {
            text.push_str(chunk);
            keep_tail(&mut text);
            most = most.max(text.len());
        })
        .unwrap();
        assert!(most <= OUTPUT_LIMIT);
    }

    #[test]
    fn bytes_that_are_not_text_do_not_end_the_reading() {
        let mut seen = Vec::new();
        each_line(&b"one\n\xff\xfe\ntwo"[..], |line| {
            seen.push(line.to_string());
            true
        })
        .unwrap();
        assert_eq!(seen, ["one", "\u{fffd}\u{fffd}", "two"]);
    }

    #[test]
    fn sources_are_the_executables_in_the_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("hypertop-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("subdir")).unwrap();
        std::fs::write(dir.join("yes"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir.join("yes"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("notes.txt"), "not executable").unwrap();
        let found = find_sources(&dir);
        assert_eq!(found, vec![dir.join("yes")]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
