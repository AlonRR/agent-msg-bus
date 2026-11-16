//! `job run`: run a command and tell a session, as bus mail, when it ends and when it prints a line
//! the session asked about (Phase 11 piece D).
//!
//! A program, not a model, decides when to send: an exit code and a regular expression decide the
//! same way every time and can be tested. A model handed a log tail writes a fluent summary even when
//! the tail is empty or junk, and costs a call per look.
//!
//! **What it sends.** One message of kind `job` when the command ends: how it ended, the run time
//! and the last lines of output. With `--match` and `--every`, the first matching line is sent at
//! once and later ones at most once per interval, each message carrying the count and the newest
//! matched lines. A message can start a paid turn in the session it wakes, which is why the session
//! sets the interval itself. Matches still held when the command ends go with the end message,
//! which is never held back.
//!
//! **From where.** From `<machine>/job`, the machine of this client's config, never as the session
//! it reports to: the broker never delivers a sender its own mail, and under token binding a token
//! acts only for its own machine.
//!
//! **Surviving a broker restart.** The end message is retried with backoff for a bounded time, so
//! a job that ends while the broker is being swapped still reports. If it never gets through, the
//! message goes to stderr and `job run` exits [`EXIT_UNSENT`].

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

/// The longest line a message carries, in characters. A longer one is cut and marked.
pub const LINE_CAP: usize = 2000;
/// Bytes held while waiting for a newline before the held text is treated as a line anyway. A
/// progress bar redrawn with `\r` never ends its line.
const HOLD_CAP: usize = 64 * 1024;
/// How soon a match message that failed to send is tried again, when the interval is longer.
pub const MATCH_RETRY: Duration = Duration::from_secs(60);
/// Matched lines one message carries at most: the newest.
pub const MATCH_LINES: usize = 10;
/// How long the end message keeps trying to reach the broker.
pub const RETRY_FOR: Duration = Duration::from_secs(15 * 60);
/// After the command exits, how long to wait for output still in its pipes. A child it started
/// can hold them open long after it is gone.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
/// `job run`'s exit code when the command could not be started (as a shell's).
pub const EXIT_NOT_STARTED: i32 = 127;
/// `job run`'s exit code when the end message never reached the broker (`EX_TEMPFAIL`).
pub const EXIT_UNSENT: i32 = 75;

/// The address a job's mail comes from on `machine`.
pub fn default_from(machine: &str) -> String {
    format!("{machine}/job")
}

/// `30s`, `10m`, `2h`, `1d`: a whole number and one unit. Zero is refused, since it would send
/// every match as its own message, which is what the interval is there to prevent.
pub fn parse_interval(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("`{s}` is not an interval: write a whole number and a unit, like 30s, 10m, 2h or 1d"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86400,
        _ => return Err(format!("`{s}`: the unit must be s, m, h or d")),
    };
    if secs == 0 {
        return Err(format!("`{s}`: the interval must be more than zero"));
    }
    Ok(Duration::from_secs(secs))
}

/// `3h 2m 5s`, the two or three largest parts that are not zero; `0s` for nothing.
pub fn human(d: Duration) -> String {
    let mut s = d.as_secs();
    let days = s / 86400;
    s %= 86400;
    let parts = [(days, "d"), (s / 3600, "h"), (s % 3600 / 60, "m"), (s % 60, "s")];
    let out: Vec<String> = parts
        .iter()
        .skip_while(|(n, _)| *n == 0)
        .take(3)
        .filter(|(n, _)| *n > 0)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    if out.is_empty() {
        "0s".into()
    } else {
        out.join(" ")
    }
}

/// Splits a byte stream into lines as a terminal would leave them on screen.
///
/// Bytes are decoded lossily, so output that is not UTF-8 still arrives. A `\r` inside a line keeps
/// only what follows it, as a progress bar's last redraw.
#[derive(Default)]
pub struct Lines {
    held: Vec<u8>,
}

impl Lines {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.held.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(i) = self.held.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.held.drain(..=i).collect();
            out.push(render(&line[..line.len() - 1]));
        }
        if self.held.len() > HOLD_CAP {
            match self.held.iter().rposition(|&b| b == b'\r') {
                Some(i) => {
                    self.held.drain(..=i);
                }
                None => out.push(render(&std::mem::take(&mut self.held))),
            }
        }
        out
    }

    /// What was held without a final newline.
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.held);
        let line = render(&rest);
        (!rest.is_empty() && !line.is_empty()).then_some(line)
    }
}

fn render(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let raw = match raw.iter().rposition(|&b| b == b'\r') {
        Some(i) => &raw[i + 1..],
        None => raw,
    };
    let s = String::from_utf8_lossy(raw);
    if s.chars().count() > LINE_CAP {
        let cut: String = s.chars().take(LINE_CAP).collect();
        format!("{cut} [cut]")
    } else {
        s.into_owned()
    }
}

/// Matched lines waiting to be sent.
#[derive(Debug, PartialEq)]
pub struct Batch {
    /// The newest matched lines, oldest first, at most [`MATCH_LINES`].
    pub lines: Vec<String>,
    /// How many lines matched since the last message, which can be more than `lines` holds.
    pub count: usize,
}

/// Decides when matched lines are sent: the first at once, then at most one message per interval.
///
/// Time is passed in, as the time since the job started, so the rule is tested without sleeping.
pub struct Batcher {
    every: Duration,
    last: Option<Duration>,
    /// Set when a send failed: try again then, rather than a whole interval later.
    retry_at: Option<Duration>,
    lines: VecDeque<String>,
    count: usize,
    total: usize,
}

impl Batcher {
    pub fn new(every: Duration) -> Batcher {
        Batcher { every, last: None, retry_at: None, lines: VecDeque::new(), count: 0, total: 0 }
    }

    /// A line matched at `now`. Returns what to send now, if anything.
    pub fn on_match(&mut self, now: Duration, line: String) -> Option<Batch> {
        self.total += 1;
        self.count += 1;
        self.lines.push_back(line);
        while self.lines.len() > MATCH_LINES {
            self.lines.pop_front();
        }
        self.due(now)
    }

    /// What to send at `now`, if something is held and the interval since the last send has passed.
    pub fn due(&mut self, now: Duration) -> Option<Batch> {
        if self.count == 0 {
            return None;
        }
        let ready = match (self.retry_at, self.last) {
            (Some(at), _) => now >= at,
            (None, Some(last)) => now.saturating_sub(last) >= self.every,
            (None, None) => true,
        };
        if !ready {
            return None;
        }
        self.last = Some(now);
        self.retry_at = None;
        self.take()
    }

    /// When something held becomes due, as time since the start.
    pub fn next_due(&self) -> Option<Duration> {
        if self.count == 0 {
            return None;
        }
        Some(self.retry_at.unwrap_or_else(|| self.last.map_or(Duration::ZERO, |l| l + self.every)))
    }

    /// Everything held, whatever the time: for the end message.
    pub fn take(&mut self) -> Option<Batch> {
        if self.count == 0 {
            return None;
        }
        let b = Batch { lines: self.lines.drain(..).collect(), count: self.count };
        self.count = 0;
        Some(b)
    }

    /// A batch that could not be sent at `now` goes back, ahead of anything newer, and is tried
    /// again after [`MATCH_RETRY`] or the interval, whichever is shorter.
    pub fn restore(&mut self, b: Batch, now: Duration) {
        self.retry_at = Some(now + MATCH_RETRY.min(self.every));
        for l in b.lines.into_iter().rev() {
            self.lines.push_front(l);
        }
        while self.lines.len() > MATCH_LINES {
            self.lines.pop_front();
        }
        self.count += b.count;
    }

    /// Lines matched since the job started.
    pub fn total(&self) -> usize {
        self.total
    }
}

/// How the command ended, in words, and the exit code `job run` passes on.
pub fn outcome(status: &ExitStatus) -> (String, i32) {
    if let Some(code) = status.code() {
        return (format!("exited {code}"), code);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return (format!("killed by signal {sig}"), 128 + sig);
        }
    }
    ("ended, exit status unknown".into(), 1)
}

/// A name for the job from its command: the program's file name and the arguments, cut short.
pub fn default_name(command: &[String]) -> String {
    let mut parts = command.iter();
    let prog = parts
        .next()
        .map(|p| {
            std::path::Path::new(p)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
        })
        .unwrap_or_default();
    let all = std::iter::once(prog).chain(parts.cloned()).collect::<Vec<_>>().join(" ");
    if all.chars().count() > 48 {
        format!("{}…", all.chars().take(47).collect::<String>())
    } else {
        all
    }
}

pub struct RunOpts {
    pub from: String,
    pub name: String,
    pub pattern: Option<(regex_lite::Regex, String)>,
    pub every: Option<(Duration, String)>,
    pub tail: usize,
    pub command: Vec<String>,
    pub retry_for: Duration,
}

/// Sends one message, returning its id or why it failed.
pub type SendFn<'a> = &'a dyn Fn(&str, &str) -> Result<String, String>;

enum Ev {
    Line(String),
    Eof,
    /// The watched process ended, with its exit code when it can be known.
    Gone(Option<i32>),
}

fn utc(t: SystemTime) -> String {
    crate::watch::utc_stamp(time::OffsetDateTime::from(t))
}

fn indented(lines: &[String]) -> String {
    lines.iter().map(|l| format!("    {l}\n")).collect()
}

/// What `job run` and `job watch` share: the last lines, the matches waiting to be sent, and the
/// words that say what is being watched.
struct Report<'a> {
    o: &'a RunOpts,
    send: SendFn<'a>,
    t0: Instant,
    tail: VecDeque<String>,
    batcher: Option<Batcher>,
    /// `Command: …` or `Watching: …`, a line of every message.
    what: String,
    /// `job run` or `job watch`.
    via: &'static str,
    /// What the last lines are the last lines of, and what to say when there are none. None: no
    /// section at all, as for a process watched with no file.
    tail_of: Option<(String, String)>,
}

impl<'a> Report<'a> {
    fn new(
        o: &'a RunOpts,
        send: SendFn<'a>,
        what: String,
        via: &'static str,
        tail_of: Option<(String, String)>,
    ) -> Self {
        Report {
            o,
            send,
            t0: Instant::now(),
            tail: VecDeque::new(),
            batcher: o.every.as_ref().map(|(every, _)| Batcher::new(*every)),
            what,
            via,
            tail_of,
        }
    }

    fn line(&mut self, l: String) {
        if self.o.tail > 0 {
            self.tail.push_back(l.clone());
            while self.tail.len() > self.o.tail {
                self.tail.pop_front();
            }
        }
        let now = self.t0.elapsed();
        if let (Some((re, _)), Some(b)) = (&self.o.pattern, self.batcher.as_mut()) {
            if re.is_match(&l) {
                if let Some(batch) = b.on_match(now, l) {
                    self.send_matches(batch);
                }
            }
        }
    }

    /// Sends held matches whose interval has passed.
    fn tick(&mut self) {
        let now = self.t0.elapsed();
        if let Some(batch) = self.batcher.as_mut().and_then(|b| b.due(now)) {
            self.send_matches(batch);
        }
    }

    /// How long the loop may sleep before something is due.
    fn wait(&self) -> Duration {
        let mut wait = Duration::from_millis(250);
        if let Some(due) = self.batcher.as_ref().and_then(|b| b.next_due()) {
            wait = wait.min(due.saturating_sub(self.t0.elapsed()));
        }
        wait
    }

    /// One try. A batch that fails goes back to wait for the next interval or the end message.
    fn send_matches(&mut self, batch: Batch) {
        let (pat, every) = match (&self.o.pattern, &self.o.every) {
            (Some((_, p)), Some((_, e))) => (p.as_str(), e.as_str()),
            _ => return,
        };
        let now = self.t0.elapsed();
        let Some(b) = self.batcher.as_mut() else { return };
        let n = batch.count;
        let subject = format!("job {}: {n} line{} matched `{pat}`", self.o.name, if n == 1 { "" } else { "s" });
        let body = format!(
            "{n} new line{} of the job {} matched `{pat}` ({} in all so far){}:\n{}\nThe job is still running. Matches are reported at most once every {every}; the end is always reported.\n{}\nSent from {} by `agent-msg-bus {}`.\n",
            if n == 1 { "" } else { "s" },
            self.o.name,
            b.total(),
            if n > batch.lines.len() { ", the newest of them" } else { "" },
            indented(&batch.lines),
            self.what,
            self.o.from,
            self.via
        );
        if let Err(e) = (self.send)(&subject, &body) {
            eprintln!("agent-msg-bus: job: could not report a match ({e}); it will go with a later message");
            b.restore(batch, now);
        }
    }

    /// The end message's sections after its first paragraph: matches not yet sent, the last lines.
    fn closing(&mut self) -> String {
        let mut body = String::new();
        if let (Some(b), Some((_, pat))) = (self.batcher.as_mut(), &self.o.pattern) {
            let total = b.total();
            body.push_str(&match_summary(pat, total, b.take()));
        }
        if let (true, Some((of, none))) = (self.o.tail > 0, &self.tail_of) {
            let lines: Vec<String> = self.tail.drain(..).collect();
            if lines.is_empty() {
                body.push_str(&format!("\n{none}\n"));
            } else {
                body.push_str(&format!("\nThe last {} lines {of}:\n{}", lines.len(), indented(&lines)));
            }
        }
        body
    }

    /// Sends the end message, retrying through a broker restart. Returns `code`, or
    /// [`EXIT_UNSENT`] if it never got through.
    fn end(&self, subject: &str, body: &str, code: i32) -> i32 {
        match send_until(self.send, subject, body, self.o.retry_for) {
            Ok(id) => {
                eprintln!("agent-msg-bus: job: reported the end as {id}");
                code
            }
            Err(e) => {
                eprintln!("agent-msg-bus: job: could not report the end ({e}). The message was:\n{subject}\n\n{body}");
                EXIT_UNSENT
            }
        }
    }
}

/// Runs the command and reports on it. Returns the exit code for `job run`.
pub fn run(o: &RunOpts, send: SendFn) -> i32 {
    let started = SystemTime::now();
    let cmdline = o.command.join(" ");
    let tail_of = ("of output, stdout and stderr together".to_string(), "No output.".to_string());
    let mut r = Report::new(o, send, format!("Command: {cmdline}"), "job run", Some(tail_of));
    let mut child = match Command::new(&o.command[0])
        .args(&o.command[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agent-msg-bus: job: cannot start `{cmdline}`: {e}");
            let subject = format!("job {}: did not start: {e}", o.name);
            let body = format!(
                "The job {} did not start: {e}.\n\nCommand: {cmdline}\nAt: {}\nSent from {} by `agent-msg-bus job run`.\n",
                o.name,
                utc(started),
                o.from
            );
            return match r.end(&subject, &body, EXIT_NOT_STARTED) {
                EXIT_UNSENT => EXIT_UNSENT,
                _ => EXIT_NOT_STARTED,
            };
        }
    };

    let (tx, rx) = mpsc::channel::<Ev>();
    let mut readers = Vec::new();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    readers.push(pump(out, tx.clone(), false));
    readers.push(pump(err, tx, true));

    let mut eofs = 0;
    let mut exited: Option<(ExitStatus, Instant)> = None;
    loop {
        if eofs >= 2 && exited.is_some() {
            break;
        }
        if let Some((_, at)) = exited {
            if at.elapsed() > DRAIN_GRACE {
                break;
            }
        }
        match rx.recv_timeout(r.wait()) {
            Ok(Ev::Line(l)) => r.line(l),
            Ok(Ev::Eof) => eofs += 1,
            Ok(Ev::Gone(_)) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                eofs = 2;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        if exited.is_none() {
            if let Ok(Some(st)) = child.try_wait() {
                exited = Some((st, Instant::now()));
            }
        }
        r.tick();
    }
    let status = match exited {
        Some((st, _)) => st,
        None => child.wait().expect("the child was started, so it can be waited for"),
    };
    let ended = SystemTime::now();
    let ran = ended.duration_since(started).unwrap_or_else(|_| r.t0.elapsed());
    let (how, code) = outcome(&status);

    let subject = format!("job {}: {how} after {}", o.name, human(ran));
    let mut body = format!(
        "The job {} {how} after {}.\n\nCommand: {cmdline}\nStarted: {}\nEnded:   {}\nSent from {} by `agent-msg-bus job run`.\n",
        o.name,
        human(ran),
        utc(started),
        utc(ended),
        o.from
    );
    body.push_str(&r.closing());
    for h in readers {
        // A reader still blocked on a pipe a grandchild holds is left to the process's exit.
        if h.is_finished() {
            let _ = h.join();
        }
    }
    r.end(&subject, &body, code)
}

/// The end message's account of `--match`: how many lines matched, and those not yet reported.
fn match_summary(pat: &str, total: usize, held: Option<Batch>) -> String {
    let lines = if total == 1 { "1 line".to_string() } else { format!("{total} lines") };
    match held {
        _ if total == 0 => format!("\nNo line matched `{pat}`.\n"),
        Some(held) => format!(
            "\nMatched `{pat}`: {lines} in all. {} not reported before the end{}:\n{}",
            held.count,
            if held.count > held.lines.len() { ", the newest of them" } else { "" },
            indented(&held.lines)
        ),
        None if total == 1 => format!("\nMatched `{pat}`: {lines} in all, reported already.\n"),
        None => format!("\nMatched `{pat}`: {lines} in all, all reported already.\n"),
    }
}

/// Reads one of the child's pipes: passes the bytes through to this process's own stream, so a
/// supervisor's log still has them, and the lines to the run loop.
fn pump(
    mut pipe: impl Read + std::marker::Send + 'static,
    tx: mpsc::Sender<Ev>,
    is_err: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut lines = Lines::default();
        let mut buf = [0u8; 8192];
        loop {
            let n = match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if is_err {
                let mut e = std::io::stderr().lock();
                let _ = e.write_all(&buf[..n]);
                let _ = e.flush();
            } else {
                let mut o = std::io::stdout().lock();
                let _ = o.write_all(&buf[..n]);
                let _ = o.flush();
            }
            for l in lines.push(&buf[..n]) {
                let _ = tx.send(Ev::Line(l));
            }
        }
        if let Some(l) = lines.finish() {
            let _ = tx.send(Ev::Line(l));
        }
        let _ = tx.send(Ev::Eof);
    })
}

/// Retries with backoff, 1 s doubling to 30 s, until `limit` has passed.
fn send_until(send: SendFn, subject: &str, body: &str, limit: Duration) -> Result<String, String> {
    let start = Instant::now();
    let mut wait = Duration::from_secs(1);
    loop {
        match send(subject, body) {
            Ok(id) => return Ok(id),
            Err(e) => {
                if start.elapsed() + wait > limit {
                    return Err(e);
                }
                eprintln!("agent-msg-bus: job: send failed ({e}); trying again in {}", human(wait));
                std::thread::sleep(wait);
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// The most `job watch` reads from a file in one go, so a log that grew by a gigabyte while
/// nobody looked is read in pieces rather than into memory at once.
const READ_CHUNK: u64 = 1 << 20;

/// A file followed from its end, like `tail -F`: lines written before `job watch` started are
/// not news.
pub struct Follow {
    path: std::path::PathBuf,
    file: std::fs::File,
    pos: u64,
}

impl Follow {
    pub fn open_at_end(path: &std::path::Path) -> std::io::Result<Follow> {
        use std::io::Seek;
        let mut file = std::fs::File::open(path)?;
        let pos = file.seek(std::io::SeekFrom::End(0))?;
        Ok(Follow { path: path.to_path_buf(), file, pos })
    }

    /// Up to [`READ_CHUNK`] bytes written since the last read. A file now shorter than what was
    /// already read was truncated or replaced, so it is read again from its start.
    pub fn read_new(&mut self) -> std::io::Result<Vec<u8>> {
        use std::io::Seek;
        let now = std::fs::metadata(&self.path)?;
        if now.len() < self.pos || self.replaced(&now) {
            eprintln!("agent-msg-bus: job: {} was truncated or replaced; reading it from the start", self.path.display());
            self.file = std::fs::File::open(&self.path)?;
            self.pos = 0;
        }
        self.file.seek(std::io::SeekFrom::Start(self.pos))?;
        let mut buf = Vec::new();
        (&mut self.file).take(READ_CHUNK).read_to_end(&mut buf)?;
        self.pos += buf.len() as u64;
        Ok(buf)
    }

    #[cfg(unix)]
    fn replaced(&self, now: &std::fs::Metadata) -> bool {
        use std::os::unix::fs::MetadataExt;
        self.file.metadata().map(|m| m.ino() != now.ino() || m.dev() != now.dev()).unwrap_or(true)
    }

    #[cfg(not(unix))]
    fn replaced(&self, _now: &std::fs::Metadata) -> bool {
        false
    }
}

/// The state letter and start time (field 22, in clock ticks since boot) of a `/proc/<pid>/stat`
/// line. The command name in field 2 is in parentheses and may itself hold spaces and `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(s: &str) -> Option<(char, u64)> {
    let rest = &s[s.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let state = f.first()?.chars().next()?;
    let start = f.get(19)?.parse().ok()?;
    Some((state, start))
}

/// Blocks until the process ends; returns its exit code when it can be known.
pub type Waiter = Box<dyn FnOnce() -> Option<i32> + std::marker::Send>;

/// Something that waits for process `pid` to end, or why it can't.
///
/// On Windows it holds a handle on the process, which also yields the exit code and cannot be
/// fooled by the pid being reused. On Linux it polls `/proc` and compares the start time, so a
/// new process given the same pid is not mistaken for the old one; the exit code goes only to the
/// parent, so it is unknown.
#[cfg(windows)]
pub fn wait_for(pid: u32) -> Result<Waiter, String> {
    win::Proc::open(pid).map(|p| Box::new(move || p.wait()) as Waiter)
}

#[cfg(target_os = "linux")]
pub fn wait_for(pid: u32) -> Result<Waiter, String> {
    let stat = |pid: u32| parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?);
    let (_, start) = stat(pid).ok_or_else(|| format!("no process with pid {pid}"))?;
    Ok(Box::new(move || {
        loop {
            match stat(pid) {
                Some((state, t)) if t == start && state != 'Z' && state != 'X' => {
                    std::thread::sleep(Duration::from_secs(1))
                }
                _ => return None,
            }
        }
    }))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn wait_for(pid: u32) -> Result<Waiter, String> {
    Err(format!("job watch --pid {pid}: watching a process works on Linux and Windows only"))
}

#[cfg(windows)]
mod win {
    type Handle = *mut core::ffi::c_void;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INFINITE: u32 = 0xFFFF_FFFF;
    const WAIT_OBJECT_0: u32 = 0;
    const STILL_ACTIVE: u32 = 259;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn WaitForSingleObject(h: Handle, ms: u32) -> u32;
        fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    pub struct Proc(Handle);

    // The handle is used from one thread at a time: opened here, waited on by the watcher thread.
    unsafe impl Send for Proc {}

    impl Proc {
        pub fn open(pid: u32) -> Result<Proc, String> {
            // SAFETY: plain Win32 call; a null result is checked.
            let h = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if h.is_null() {
                Err(format!("no process with pid {pid} that this user can watch: {}", std::io::Error::last_os_error()))
            } else {
                Ok(Proc(h))
            }
        }

        pub fn wait(self) -> Option<i32> {
            let mut code = 0u32;
            // SAFETY: self.0 is a live process handle opened with SYNCHRONIZE and query rights.
            unsafe {
                // A wait that fails (it should not, on a handle this process holds) must not be read
                // as the process ending: fall back to asking every second until it has an exit code.
                while WaitForSingleObject(self.0, INFINITE) != WAIT_OBJECT_0 {
                    if GetExitCodeProcess(self.0, &mut code) != 0 && code != STILL_ACTIVE {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                (GetExitCodeProcess(self.0, &mut code) != 0).then_some(code as i32)
            }
        }
    }

    impl Drop for Proc {
        fn drop(&mut self) {
            // SAFETY: closes the handle opened in `open`, once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Watches a process, a file, or both, and reports as `job run` does. Ends when the process
/// does; watching only a file runs until stopped and sends only matches. Returns the exit code.
pub fn watch(o: &RunOpts, pid: Option<u32>, file: Option<&std::path::Path>, send: SendFn) -> i32 {
    let started = SystemTime::now();
    let waiter = match pid.map(wait_for) {
        Some(Ok(w)) => Some(w),
        Some(Err(e)) => {
            eprintln!("agent-msg-bus: job watch: {e}");
            return 1;
        }
        None => None,
    };
    let mut follow = match file.map(Follow::open_at_end) {
        Some(Ok(f)) => Some(f),
        Some(Err(e)) => {
            eprintln!("agent-msg-bus: job watch: cannot read {}: {e}", file.unwrap().display());
            return 1;
        }
        None => None,
    };
    let what = match (pid, file) {
        (Some(p), Some(f)) => format!("process {p} and the file {}", f.display()),
        (Some(p), None) => format!("process {p}"),
        (None, Some(f)) => format!("the file {}", f.display()),
        (None, None) => unreachable!("clap requires --pid or --file"),
    };
    let tail_of = file.map(|f| {
        (format!("added to {}", f.display()), format!("Nothing was added to {} while it was watched.", f.display()))
    });
    let mut r = Report::new(o, send, format!("Watching: {what}"), "job watch", tail_of);
    let (tx, rx) = mpsc::channel::<Ev>();
    if let Some(w) = waiter {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Ev::Gone(w()));
        });
    }
    eprintln!("agent-msg-bus: job: watching {what}");

    let mut lines = Lines::default();
    let mut read_failed = false;
    let mut gone: Option<Option<i32>> = None;
    loop {
        if let Some(f) = follow.as_mut() {
            loop {
                match f.read_new() {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => {
                        read_failed = false;
                        for l in lines.push(&b) {
                            r.line(l);
                        }
                    }
                    Err(e) => {
                        if !read_failed {
                            eprintln!("agent-msg-bus: job watch: cannot read {}: {e}; still trying", f.path.display());
                        }
                        read_failed = true;
                        break;
                    }
                }
            }
        }
        if gone.is_some() {
            break;
        }
        if let Ok(Ev::Gone(code)) = rx.recv_timeout(r.wait()) {
            // One more pass over the file above picks up what the process wrote last.
            gone = Some(code);
            continue;
        }
        r.tick();
    }
    if let Some(l) = lines.finish() {
        r.line(l);
    }
    drop(tx);

    let code = gone.flatten();
    let ended = SystemTime::now();
    let watched = ended.duration_since(started).unwrap_or_else(|_| r.t0.elapsed());
    let pid = pid.expect("only a watched process ends the loop");
    let subject = match code {
        Some(c) => format!("job {}: process {pid} ended, exit {c}, watched {}", o.name, human(watched)),
        None => format!("job {}: process {pid} ended, watched {}", o.name, human(watched)),
    };
    let mut body = format!(
        "The process {pid}, watched as {}, ended{}. It was watched for {}.\n",
        o.name,
        code.map(|c| format!(" with exit code {c}")).unwrap_or_default(),
        human(watched)
    );
    if code.is_none() {
        body.push_str(
            "Its exit code cannot be known here: only the process that started it is told. \
             `job run` starts the command itself and reports its code.\n",
        );
    }
    body.push_str(&format!(
        "\nWatching: {what}\nStarted watching: {}\nEnded:            {}\nSent from {} by `agent-msg-bus job watch`.\n",
        utc(started),
        utc(ended),
        o.from
    ));
    body.push_str(&r.closing());
    r.end(&subject, &body, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_proc_stat_line_gives_state_and_start_time_even_with_a_strange_name() {
        let line = "4242 (scrape (v2) x) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 2 0 0 20 0 1 0 987654 1000 100";
        assert_eq!(parse_stat(line), Some(('S', 987654)));
        assert_eq!(parse_stat("4242 (zombie) Z 1 2 3"), None, "too short: no start time");
        assert_eq!(parse_stat("garbage"), None);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("amb-job-unit-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("f.log")
    }

    #[test]
    fn a_followed_file_yields_only_what_was_added() {
        use std::io::Write;
        let p = scratch("added");
        std::fs::write(&p, "before\n").unwrap();
        let mut f = Follow::open_at_end(&p).unwrap();
        assert!(f.read_new().unwrap().is_empty());
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(b"after\n").unwrap();
        assert_eq!(f.read_new().unwrap(), b"after\n");
        assert!(f.read_new().unwrap().is_empty());
    }

    #[test]
    fn a_truncated_file_is_read_again_from_its_start() {
        let p = scratch("truncated");
        std::fs::write(&p, "a long first line\n").unwrap();
        let mut f = Follow::open_at_end(&p).unwrap();
        std::fs::write(&p, "new\n").unwrap();
        assert_eq!(f.read_new().unwrap(), b"new\n");
    }

    #[test]
    fn a_big_burst_is_read_in_pieces() {
        use std::io::Write;
        let p = scratch("burst");
        std::fs::write(&p, "").unwrap();
        let mut f = Follow::open_at_end(&p).unwrap();
        let burst = vec![b'x'; READ_CHUNK as usize + 10];
        std::fs::OpenOptions::new().append(true).open(&p).unwrap().write_all(&burst).unwrap();
        assert_eq!(f.read_new().unwrap().len(), READ_CHUNK as usize);
        assert_eq!(f.read_new().unwrap().len(), 10);
    }

    #[test]
    fn watching_a_pid_that_does_not_exist_is_refused_at_once() {
        assert!(wait_for(999_999_999).is_err());
    }

    #[test]
    fn intervals_take_a_number_and_a_unit() {
        assert_eq!(parse_interval("30s"), Ok(30 * S));
        assert_eq!(parse_interval("10m"), Ok(600 * S));
        assert_eq!(parse_interval("2h"), Ok(7200 * S));
        assert_eq!(parse_interval("1d"), Ok(86400 * S));
        for bad in ["", "10", "m", "0m", "1.5h", "5 m", "-1m", "3w"] {
            assert!(parse_interval(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn run_time_reads_as_its_largest_parts() {
        assert_eq!(human(Duration::ZERO), "0s");
        assert_eq!(human(5 * S), "5s");
        assert_eq!(human(65 * S), "1m 5s");
        assert_eq!(human(3 * 3600 * S + 2 * 60 * S + 5 * S), "3h 2m 5s");
        assert_eq!(human(3 * 3600 * S + 5 * S), "3h 5s");
        assert_eq!(human(2 * 86400 * S + 3600 * S + 60 * S + S), "2d 1h 1m");
    }

    #[test]
    fn lines_split_on_newlines_across_reads() {
        let mut l = Lines::default();
        assert_eq!(l.push(b"one\ntw"), vec!["one"]);
        assert_eq!(l.push(b"o\r\nthree"), vec!["two"]);
        assert_eq!(l.finish(), Some("three".into()));
        assert_eq!(l.finish(), None);
    }

    #[test]
    fn a_carriage_return_keeps_the_last_redraw() {
        let mut l = Lines::default();
        assert_eq!(l.push(b"10%\r50%\r100% done\n"), vec!["100% done"]);
    }

    #[test]
    fn output_that_is_not_utf8_still_arrives() {
        let mut l = Lines::default();
        let got = l.push(b"caf\xe9 ok\n");
        assert_eq!(got.len(), 1);
        assert!(got[0].starts_with("caf") && got[0].ends_with(" ok"), "{got:?}");
    }

    #[test]
    fn a_line_that_never_ends_is_bounded() {
        let mut l = Lines::default();
        // A progress bar redrawn forever: only the last redraw is kept.
        let mut got = Vec::new();
        for i in 0..20_000 {
            got.extend(l.push(format!("\r{i:05}").as_bytes()));
        }
        assert!(got.is_empty(), "redraws are not lines: {}", got.len());
        assert!(l.held.len() <= HOLD_CAP + 6, "held {} bytes", l.held.len());
        assert_eq!(l.finish(), Some("19999".into()));
        // No newline and no carriage return: it becomes a line, cut for the message.
        let got = l.push(&vec![b'x'; HOLD_CAP + 1]);
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with(" [cut]") && got[0].chars().count() == LINE_CAP + 6);
    }

    #[test]
    fn the_first_match_is_sent_at_once() {
        let mut b = Batcher::new(600 * S);
        assert_eq!(b.on_match(5 * S, "a".into()), Some(Batch { lines: vec!["a".into()], count: 1 }));
    }

    #[test]
    fn later_matches_wait_for_the_interval_and_go_together() {
        let mut b = Batcher::new(600 * S);
        b.on_match(Duration::ZERO, "a".into());
        assert_eq!(b.on_match(10 * S, "b".into()), None);
        assert_eq!(b.on_match(20 * S, "c".into()), None);
        assert_eq!(b.due(599 * S), None);
        assert_eq!(b.next_due(), Some(600 * S));
        assert_eq!(b.due(600 * S), Some(Batch { lines: vec!["b".into(), "c".into()], count: 2 }));
        assert_eq!(b.due(1300 * S), None, "nothing held, nothing sent");
        assert_eq!(b.next_due(), None);
        assert_eq!(b.total(), 3);
    }

    #[test]
    fn a_match_after_a_quiet_interval_goes_at_once() {
        let mut b = Batcher::new(600 * S);
        b.on_match(Duration::ZERO, "a".into());
        assert!(b.on_match(900 * S, "b".into()).is_some());
    }

    #[test]
    fn a_batch_keeps_the_newest_lines_and_the_full_count() {
        let mut b = Batcher::new(600 * S);
        b.on_match(Duration::ZERO, "first".into());
        for i in 0..25 {
            b.on_match(S, format!("l{i}"));
        }
        let got = b.take().unwrap();
        assert_eq!(got.count, 25);
        assert_eq!(got.lines.len(), MATCH_LINES);
        assert_eq!(got.lines.last().unwrap(), "l24");
        assert_eq!(got.lines.first().unwrap(), "l15");
    }

    #[test]
    fn a_batch_that_failed_to_send_goes_back_first() {
        let mut b = Batcher::new(600 * S);
        let first = b.on_match(Duration::ZERO, "a".into()).unwrap();
        b.restore(first, Duration::ZERO);
        b.on_match(S, "b".into());
        assert_eq!(b.take(), Some(Batch { lines: vec!["a".into(), "b".into()], count: 2 }));
    }

    #[test]
    fn a_failed_send_is_tried_again_within_a_minute_not_a_whole_interval() {
        // With --every 1d, a broker restart at the first match would otherwise cost a day.
        let mut b = Batcher::new(86400 * S);
        let first = b.on_match(100 * S, "a".into()).unwrap();
        b.restore(first, 100 * S);
        assert_eq!(b.next_due(), Some(160 * S));
        assert_eq!(b.due(159 * S), None);
        assert!(b.due(160 * S).is_some());
        // A short interval is not made longer by the retry.
        let mut b = Batcher::new(10 * S);
        let first = b.on_match(100 * S, "a".into()).unwrap();
        b.restore(first, 100 * S);
        assert_eq!(b.next_due(), Some(110 * S));
    }

    #[test]
    fn the_name_comes_from_the_program_and_its_arguments() {
        let c = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(default_name(&c(&["/usr/bin/python3", "scrape.py", "--all"])), "python3 scrape.py --all");
        assert_eq!(default_name(&c(&["C:\\Tools\\scrape.exe"])), if cfg!(windows) { "scrape" } else { "C:\\Tools\\scrape" });
        let long = default_name(&c(&["x", &"y".repeat(100)]));
        assert_eq!(long.chars().count(), 48);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn the_end_message_says_plainly_when_nothing_matched() {
        // Seen live 16 Nov 2026: "Matched `^hit`: 0 lines in all, all reported already."
        assert_eq!(match_summary("^hit", 0, None), "\nNo line matched `^hit`.\n");
        assert_eq!(
            match_summary("^hit", 3, None),
            "\nMatched `^hit`: 3 lines in all, all reported already.\n"
        );
        assert_eq!(
            match_summary("^hit", 1, None),
            "\nMatched `^hit`: 1 line in all, reported already.\n"
        );
        let held = Batch { lines: vec!["hit two".into()], count: 1 };
        assert_eq!(
            match_summary("^hit", 2, Some(held)),
            "\nMatched `^hit`: 2 lines in all. 1 not reported before the end:\n    hit two\n"
        );
    }

    #[test]
    fn patterns_take_the_syntax_the_usage_doc_promises() {
        // docs/usage.md: the usual operators and ASCII classes, no Unicode classes.
        let re = regex_lite::Regex::new(r"^(ERROR|WARN)\b.*\d{3}\s\w+$").unwrap();
        assert!(re.is_match("ERROR at page 404 notfound"));
        assert!(!re.is_match("INFO 200 ok"));
        assert!(regex_lite::Regex::new(r"\p{Greek}").is_err(), "Unicode classes are refused, not ignored");
    }

    #[test]
    fn the_sender_is_the_machines_job_address() {
        assert_eq!(default_from("machine-a"), "machine-a/job");
    }

    fn opts(pattern: Option<&str>) -> RunOpts {
        RunOpts {
            from: "machine-a/job".into(),
            name: "t".into(),
            pattern: pattern.map(|p| (regex_lite::Regex::new(p).unwrap(), p.to_string())),
            every: pattern.map(|_| (3600 * S, "1h".into())),
            tail: 20,
            command: vec![],
            retry_for: Duration::ZERO,
        }
    }

    #[test]
    fn a_match_that_could_not_be_sent_is_held_for_later() {
        let o = opts(Some("x"));
        let down = |_: &str, _: &str| Err("down".to_string());
        let mut r = Report::new(&o, &down, "Command: t".into(), "job run", None);
        r.line("x1".into());
        assert_eq!(r.batcher.as_mut().unwrap().take(), Some(Batch { lines: vec!["x1".into()], count: 1 }));
    }

    #[test]
    fn giving_up_on_the_end_message_returns_the_last_error() {
        let tries = std::cell::Cell::new(0);
        let got = send_until(
            &|_: &str, _: &str| {
                tries.set(tries.get() + 1);
                Err(format!("down {}", tries.get()))
            },
            "s",
            "b",
            Duration::ZERO,
        );
        assert_eq!(got, Err("down 1".into()));
    }
}
