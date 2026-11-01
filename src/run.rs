//! Running things: sources, peeks and actions. Each gets its own thread and posts
//! what happens to the main loop, so nothing a device does can hold up the screen.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

const DEFAULT_EVERY: u64 = 30;
/// After a command exits, how long to wait for the rest of its output. Not until
/// the pipes close: an ssh that left a shared connection behind holds them open.
const DRAIN: Duration = Duration::from_millis(300);
const OUTPUT_LIMIT: usize = 64 * 1024;

pub enum Msg {
    Line { source: String, line: Value },
    Exited { source: String, success: bool, why: String },
    Peek { key: String, label: String, text: String },
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

/// Run a command to its end, passing its output along as it comes. Stdin is empty,
/// and it runs in its own process group so a timeout takes all of it down.
pub fn run_collecting(argv: &[String], timeout: Duration, mut on_output: impl FnMut(&str)) -> Result<i32, String> {
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
            while let Ok(n) = reader.read(&mut buffer) {
                if n == 0 || tx.send(String::from_utf8_lossy(&buffer[..n]).into_owned()).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    let started = Instant::now();
    let pid = child.id() as i32;
    let code = loop {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            on_output(&chunk);
        }
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status.code().unwrap_or(-1);
        }
        if started.elapsed() > timeout {
            // SAFETY: the process group we created for this child.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            let _ = child.wait();
            return Err(format!("no answer after {} s", timeout.as_secs()));
        }
    };
    let deadline = Instant::now() + DRAIN;
    while let Ok(chunk) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        on_output(&chunk);
    }
    Ok(code)
}

pub fn spawn_peek(key: String, label: String, argv: Vec<String>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let mut text = String::new();
        let outcome = run_collecting(&argv, Duration::from_secs(15), |chunk| text.push_str(chunk));
        match outcome {
            Ok(0) => {}
            Ok(code) => text.push_str(&format!("\n(exit {code})")),
            Err(why) => text.push_str(&format!("\n({why})")),
        }
        let mut text = clean(&text);
        keep_tail(&mut text);
        let _ = tx.send(Msg::Peek { key, label, text });
    });
}

pub fn spawn_job(job: usize, argv: Vec<String>, tx: Sender<Msg>) {
    thread::spawn(move || {
        let progress = tx.clone();
        let outcome = run_collecting(&argv, Duration::from_secs(120), move |chunk| {
            let _ = progress.send(Msg::JobOutput { job, text: clean(chunk) });
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

/// Run a source for as long as this program runs. A source prints one JSON object
/// per line. It can keep going, or it can exit and be run again `every` seconds
/// later. Its stdin is a pipe that closes when this program ends, however it ends,
/// so a long-running source can notice. The returned sender asks for a run now.
pub fn spawn_source(path: PathBuf, tx: Sender<Msg>, running: Running) -> Sender<()> {
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
                            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                                if !line.trim().is_empty() {
                                    *last_error.lock().unwrap() = clean(&line);
                                }
                            }
                        });
                    }
                    if let Some(stdout) = child.stdout.take() {
                        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                            let Ok(line) = serde_json::from_str::<Value>(&line) else { continue };
                            if let (Some(n), true) = (line["every"].as_u64(), line["id"].is_null()) {
                                every = n.max(1);
                                continue;
                            }
                            if tx.send(Msg::Line { source: name.clone(), line }).is_err() {
                                return;
                            }
                        }
                    }
                    let status = child.wait();
                    running.0.lock().unwrap().remove(&name);
                    let why = last_error.lock().unwrap().clone();
                    match status {
                        Ok(s) if s.success() => (true, why),
                        Ok(s) => (false, if why.is_empty() { format!("exit {}", s.code().unwrap_or(-1)) } else { why }),
                        Err(e) => (false, e.to_string()),
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
        let code = run_collecting(&argv("echo hi; echo err >&2; exit 3"), Duration::from_secs(5), |c| out.push_str(c));
        assert_eq!(code, Ok(3));
        assert!(out.contains("hi") && out.contains("err"));
    }

    #[test]
    fn a_command_that_will_not_finish_is_killed_with_its_children() {
        let t = Instant::now();
        let outcome = run_collecting(&argv("sleep 30 & sleep 30"), Duration::from_millis(300), |_| {});
        assert!(outcome.is_err());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_an_error_not_a_panic() {
        assert!(run_collecting(&["/nonexistent/x".to_string()], Duration::from_secs(1), |_| {}).is_err());
        assert!(run_collecting(&[], Duration::from_secs(1), |_| {}).is_err());
    }

    #[test]
    fn a_child_holding_the_pipes_open_does_not_hold_up_the_result() {
        let t = Instant::now();
        let code = run_collecting(&argv("sleep 20 & exit 0"), Duration::from_secs(10), |_| {});
        assert_eq!(code, Ok(0));
        assert!(t.elapsed() < Duration::from_secs(5));
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
