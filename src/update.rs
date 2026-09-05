//! Replace the installed binary **without stopping anything**.
//!
//! The installer does the opposite, and has to: it kills every `agent-msg-bus.exe` before copying,
//! because a running image cannot be overwritten. That is acceptable for a first install and is a
//! bad way to ship an update — it ends every session's `watch` (its inbox) and stops the relay,
//! whose recovery depends on a supervisor that is not guaranteed to be working. On one machine here
//! the relay's scheduled task has been refusing to relaunch since 22 Aug, so killing the relay would
//! make every session on it deaf with no automatic way back.
//!
//! **A running `.exe` can be RENAMED even though it cannot be overwritten.** So:
//!
//! 1. rename the installed binary out of the way, keeping its version in the name
//! 2. copy the new one into the path that was just freed
//!
//! Processes already running keep executing the renamed file — undisturbed, unaware, and still on
//! the old build. Only *new* invocations pick up the new binary. Nothing is killed and nothing is
//! restarted, which means this command can never be the reason a machine goes deaf.
//!
//! The cost of that guarantee is that it does not finish the job by itself: a long-lived process
//! goes on running old code until something restarts it. That is a fact to REPORT, not to fix by
//! force — so `plan` collects it and the caller prints it.

use std::path::{Path, PathBuf};

/// Ask a binary which build it is, by running it.
///
/// `None` means it could not say — either it is not there, or it predates `--version` (0.1.0-era,
/// which answers "unexpected argument" and exits non-zero). Both are real answers worth showing
/// rather than errors worth aborting on: "the thing I am replacing cannot tell me what it is" is
/// exactly the situation an update exists to end.
pub fn version_of(exe: &Path) -> Option<String> {
    let out = std::process::Command::new(exe).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `clap` renders "<name> <version>"; take the last whitespace-separated field so a renamed
    // binary still parses.
    text.split_whitespace().last().map(|s| s.to_string())
}

/// Where this platform's installer puts the binary.
pub fn default_install_path() -> PathBuf {
    if cfg!(windows) {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_default();
        PathBuf::from(base).join("agent-msg-bus").join("agent-msg-bus.exe")
    } else {
        PathBuf::from("/usr/local/bin/agent-msg-bus")
    }
}

/// The freshly built binary in this repo, if the caller did not name one.
pub fn default_source_path() -> PathBuf {
    let name = if cfg!(windows) { "agent-msg-bus.exe" } else { "agent-msg-bus" };
    PathBuf::from("target").join("release").join(name)
}

#[derive(Debug)]
pub struct Plan {
    pub from: PathBuf,
    pub to: PathBuf,
    pub from_version: String,
    /// `None` when the installed binary is absent, or too old to report a version.
    pub to_version: Option<String>,
    pub backup: PathBuf,
}

impl Plan {
    /// Nothing to do — the installed binary already reports the source's version.
    pub fn already_current(&self) -> bool {
        self.to_version.as_deref() == Some(self.from_version.as_str())
    }
}

/// Work out what would change, verifying the SOURCE before anything is moved.
///
/// The source is run and asked its version first, deliberately: an update that installs a file it
/// never checked can happily put a corrupt download, a wrong-architecture build, or a half-copied
/// file into the path every session depends on, and the failure would surface later as a machine
/// that had quietly stopped receiving.
pub fn plan(from: &Path, to: &Path) -> Result<Plan, String> {
    if !from.exists() {
        return Err(format!(
            "no binary at {} — build one first with `cargo build --release`",
            from.display()
        ));
    }
    let from_version = version_of(from).ok_or_else(|| {
        format!(
            "{} could not report a version, so it is not safe to install. A binary that cannot say \
             what it is has no business replacing one that can.",
            from.display()
        )
    })?;
    let to_version = if to.exists() { version_of(to) } else { None };

    // The old version is in the backup name, so a second update never has to overwrite a backup
    // that a still-running process is executing — which on Windows would fail — and so the file
    // itself says what it is if someone has to roll back by hand.
    let label = to_version.clone().unwrap_or_else(|| "unknown".into());
    let mut backup = to.as_os_str().to_os_string();
    backup.push(format!(".{label}.old"));

    Ok(Plan {
        from: from.to_path_buf(),
        to: to.to_path_buf(),
        from_version,
        to_version,
        backup: PathBuf::from(backup),
    })
}

/// Rename the installed binary aside and copy the new one into its place.
///
/// Never kills, signals or restarts a process. That is the entire point, and it is why this is safe
/// to run on a machine whose relay supervisor is broken.
pub fn apply(plan: &Plan) -> Result<(), String> {
    if let Some(dir) = plan.to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if plan.to.exists() {
        // A backup from an earlier update may still be executing in some long-lived process, in
        // which case it cannot be removed — but it also cannot be in the way, because the name
        // carries the version it holds. Only a same-version leftover collides, and removing that is
        // safe to attempt and safe to fail.
        let _ = std::fs::remove_file(&plan.backup);
        std::fs::rename(&plan.to, &plan.backup).map_err(|e| {
            format!(
                "cannot move {} aside to {}: {e}",
                plan.to.display(),
                plan.backup.display()
            )
        })?;
    }
    if let Err(e) = std::fs::copy(&plan.from, &plan.to) {
        // Put it back rather than leaving the machine with no binary at all. A failed update must
        // not be worse than no update.
        let _ = std::fs::rename(&plan.backup, &plan.to);
        return Err(format!("cannot copy {} to {}: {e}", plan.from.display(), plan.to.display()));
    }
    Ok(())
}

/// Processes still executing the binary that was just replaced.
///
/// Reported, never acted on. A long-lived process keeps running the old image until something
/// restarts it, and the honest thing is to say which ones and let a human decide — a relay restart
/// in particular is a decision about whether a machine can get its relay back.
#[cfg(windows)]
pub fn processes_still_on_the_old_build() -> Vec<String> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-CimInstance Win32_Process -Filter \"Name='agent-msg-bus.exe'\" | \
             ForEach-Object { \"$($_.ProcessId)|$($_.CommandLine)\" }",
        ])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(windows))]
pub fn processes_still_on_the_old_build() -> Vec<String> {
    let out = std::process::Command::new("pgrep").args(["-a", "agent-msg-bus"]).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_string())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("amb-update-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The swap must preserve the old file rather than destroy it, and the backup name must say
    /// which version it holds — that is what makes a rollback possible without guessing.
    #[test]
    fn apply_moves_the_old_binary_aside_and_never_deletes_it() {
        let d = tmp("swap");
        let from = d.join("new.bin");
        let to = d.join("installed.bin");
        std::fs::write(&from, b"NEW").unwrap();
        std::fs::write(&to, b"OLD").unwrap();

        let plan = Plan {
            from: from.clone(),
            to: to.clone(),
            from_version: "9.9.9".into(),
            to_version: Some("1.1.1".into()),
            backup: PathBuf::from(format!("{}.1.1.1.old", to.display())),
        };
        apply(&plan).unwrap();

        assert_eq!(std::fs::read(&to).unwrap(), b"NEW", "new binary is not in place");
        assert_eq!(
            std::fs::read(&plan.backup).unwrap(),
            b"OLD",
            "the replaced binary was destroyed instead of kept"
        );
    }

    /// Installing into a path with nothing there yet must work, and must not invent a backup.
    #[test]
    fn a_first_install_needs_no_backup() {
        let d = tmp("fresh");
        let from = d.join("new.bin");
        let to = d.join("nothing-here.bin");
        std::fs::write(&from, b"NEW").unwrap();

        let plan = Plan {
            from,
            to: to.clone(),
            from_version: "9.9.9".into(),
            to_version: None,
            backup: PathBuf::from(format!("{}.unknown.old", to.display())),
        };
        apply(&plan).unwrap();
        assert_eq!(std::fs::read(&to).unwrap(), b"NEW");
        assert!(!plan.backup.exists(), "invented a backup of a file that never existed");
    }

    /// A source that cannot say what it is must be refused before anything is moved. Otherwise a
    /// corrupt or wrong-architecture file lands in the path every session on the machine depends on.
    #[test]
    fn a_source_that_cannot_report_its_version_is_refused() {
        let d = tmp("junk");
        let from = d.join("not-a-program.bin");
        std::fs::write(&from, b"this is not an executable").unwrap();
        let to = d.join("installed.bin");
        std::fs::write(&to, b"OLD").unwrap();

        assert!(plan(&from, &to).is_err(), "a file that cannot run was accepted as an update");
        assert_eq!(std::fs::read(&to).unwrap(), b"OLD", "the installed binary was touched anyway");
    }

    #[test]
    fn a_missing_source_is_refused_by_name() {
        let d = tmp("absent");
        let e = plan(&d.join("nope.bin"), &d.join("installed.bin")).unwrap_err();
        assert!(e.contains("cargo build --release"), "error does not say how to fix it: {e}");
    }

    /// `already_current` is what stops a no-op update from churning the backup on every run.
    #[test]
    fn an_identical_version_is_recognised_as_nothing_to_do() {
        let p = Plan {
            from: PathBuf::from("a"),
            to: PathBuf::from("b"),
            from_version: "0.4.0".into(),
            to_version: Some("0.4.0".into()),
            backup: PathBuf::from("c"),
        };
        assert!(p.already_current());
    }

    /// An installed binary too old to report a version is NOT "current", however it compares.
    #[test]
    fn an_unreportable_installed_version_is_never_treated_as_current() {
        let p = Plan {
            from: PathBuf::from("a"),
            to: PathBuf::from("b"),
            from_version: "0.4.0".into(),
            to_version: None,
            backup: PathBuf::from("c"),
        };
        assert!(!p.already_current());
    }
}
