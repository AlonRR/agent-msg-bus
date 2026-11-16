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
    lines: VecDeque<String>,
    count: usize,
    total: usize,
}

impl Batcher {
    pub fn new(every: Duration) -> Batcher {
        Batcher { every, last: None, lines: VecDeque::new(), count: 0, total: 0 }
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
        if let Some(last) = self.last {
            if now.saturating_sub(last) < self.every {
                return None;
            }
        }
        self.last = Some(now);
        self.take()
    }

    /// When something held becomes due, as time since the start.
    pub fn next_due(&self) -> Option<Duration> {
        if self.count == 0 {
            return None;
        }
        Some(self.last.map_or(Duration::ZERO, |l| l + self.every))
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

    /// A batch that could not be sent goes back, ahead of anything newer.
    pub fn restore(&mut self, b: Batch) {
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
}

fn utc(t: SystemTime) -> String {
    crate::watch::utc_stamp(time::OffsetDateTime::from(t))
}

fn indented(lines: &[String]) -> String {
    lines.iter().map(|l| format!("    {l}\n")).collect()
}

/// Runs the command and reports on it. Returns the exit code for `job run`.
pub fn run(o: &RunOpts, send: SendFn) -> i32 {
    let started = SystemTime::now();
    let t0 = Instant::now();
    let cmdline = o.command.join(" ");
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
            return match send_until(send, &subject, &body, o.retry_for) {
                Ok(_) => EXIT_NOT_STARTED,
                Err(_) => EXIT_UNSENT,
            };
        }
    };

    let (tx, rx) = mpsc::channel::<Ev>();
    let mut readers = Vec::new();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    readers.push(pump(out, tx.clone(), false));
    readers.push(pump(err, tx, true));

    let mut tail: VecDeque<String> = VecDeque::new();
    let mut batcher = o.every.as_ref().map(|(every, _)| Batcher::new(*every));
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
        let mut wait = Duration::from_millis(250);
        if let Some(due) = batcher.as_ref().and_then(|b| b.next_due()) {
            wait = wait.min(due.saturating_sub(t0.elapsed()));
        }
        match rx.recv_timeout(wait) {
            Ok(Ev::Line(l)) => {
                if o.tail > 0 {
                    tail.push_back(l.clone());
                    while tail.len() > o.tail {
                        tail.pop_front();
                    }
                }
                if let (Some((re, _)), Some(b)) = (&o.pattern, batcher.as_mut()) {
                    if re.is_match(&l) {
                        if let Some(batch) = b.on_match(t0.elapsed(), l) {
                            send_matches(o, send, b, batch);
                        }
                    }
                }
            }
            Ok(Ev::Eof) => eofs += 1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
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
        if let Some(b) = batcher.as_mut() {
            if let Some(batch) = b.due(t0.elapsed()) {
                send_matches(o, send, b, batch);
            }
        }
    }
    let status = match exited {
        Some((st, _)) => st,
        None => child.wait().expect("the child was started, so it can be waited for"),
    };
    let ended = SystemTime::now();
    let ran = ended.duration_since(started).unwrap_or_else(|_| t0.elapsed());
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
    if let (Some(b), Some((_, pat))) = (batcher.as_mut(), &o.pattern) {
        let total = b.total();
        match b.take() {
            Some(held) => body.push_str(&format!(
                "\nMatched `{pat}`: {total} lines in all. {} not reported before the end{}:\n{}",
                held.count,
                if held.count > held.lines.len() { ", the newest of them" } else { "" },
                indented(&held.lines)
            )),
            None => body.push_str(&format!("\nMatched `{pat}`: {total} lines in all, all reported already.\n")),
        }
    }
    if o.tail > 0 {
        let lines: Vec<String> = tail.into_iter().collect();
        if lines.is_empty() {
            body.push_str("\nNo output.\n");
        } else {
            body.push_str(&format!(
                "\nThe last {} lines of output, stdout and stderr together:\n{}",
                lines.len(),
                indented(&lines)
            ));
        }
    }
    for r in readers {
        // A reader still blocked on a pipe a grandchild holds is left to the process's exit.
        if r.is_finished() {
            let _ = r.join();
        }
    }
    match send_until(send, &subject, &body, o.retry_for) {
        Ok(id) => {
            eprintln!("agent-msg-bus: job: reported the end as {id}");
            code
        }
        Err(e) => {
            eprintln!(
                "agent-msg-bus: job: could not report the end ({e}). The message was:\n{subject}\n\n{body}"
            );
            EXIT_UNSENT
        }
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

/// One try. A batch that fails goes back to wait for the next interval or the end message.
fn send_matches(o: &RunOpts, send: SendFn, b: &mut Batcher, batch: Batch) {
    let (pat, every) = match (&o.pattern, &o.every) {
        (Some((_, p)), Some((_, e))) => (p.as_str(), e.as_str()),
        _ => return,
    };
    let n = batch.count;
    let subject = format!("job {}: {n} line{} matched `{pat}`", o.name, if n == 1 { "" } else { "s" });
    let body = format!(
        "{n} new line{} of the job {} matched `{pat}` ({} in all so far){}:\n{}\nThe job is still running. Matches are reported at most once every {every}; the end is always reported.\nCommand: {}\nSent from {} by `agent-msg-bus job run`.\n",
        if n == 1 { "" } else { "s" },
        o.name,
        b.total(),
        if n > batch.lines.len() { ", the newest of them" } else { "" },
        indented(&batch.lines),
        o.command.join(" "),
        o.from
    );
    if let Err(e) = send(&subject, &body) {
        eprintln!("agent-msg-bus: job: could not report a match ({e}); it will go with a later message");
        b.restore(batch);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

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
        b.restore(first);
        b.on_match(S, "b".into());
        assert_eq!(b.take(), Some(Batch { lines: vec!["a".into(), "b".into()], count: 2 }));
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
        let mut b = Batcher::new(3600 * S);
        let batch = b.on_match(Duration::ZERO, "x1".into()).unwrap();
        send_matches(&o, &|_: &str, _: &str| Err("down".to_string()), &mut b, batch);
        assert_eq!(b.take(), Some(Batch { lines: vec!["x1".into()], count: 1 }));
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
