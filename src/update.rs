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

    /// Why installing this source would move the machine BACKWARDS, or `None` if it would not.
    ///
    /// `update` takes its source from this repo's `target/release` build unless told otherwise, and
    /// that build is whatever was last compiled here — which can be months older than what is
    /// installed. On 16 Sep 2026 the repo build on one machine was 0.4.8 while the installed binary
    /// was 0.4.15, so a bare `update` would have put a seven-release-old binary into the path every
    /// session and the relay depend on, reporting success while doing it.
    ///
    /// Refusing is the right default because the damage is silent and machine-wide, while the cost
    /// of a false refusal is one flag. Only a comparison that can actually be made counts: if either
    /// side is not a plain `x.y.z`, this says nothing rather than guessing.
    pub fn downgrade_refusal(&self) -> Option<String> {
        let installed = self.to_version.as_deref()?;
        let new = version_triple(&self.from_version)?;
        let old = version_triple(installed)?;
        if new >= old {
            return None;
        }
        Some(format!(
            "{} reports {}, which is OLDER than the {} already installed at {}. Refusing: `update` \
             takes its source from this repo's target/release build unless --from says otherwise, \
             and that build is whatever was last compiled here — which can be months behind what is \
             deployed. Installing it would put an older binary in the path every session and the \
             relay on this machine use. Build a current one with `cargo build --release`, point \
             --from at the binary you mean, or pass --force to install the older build deliberately.",
            self.from.display(),
            self.from_version,
            installed,
            self.to.display()
        ))
    }
}

/// What a `self-update` run decided. Every variant is a normal outcome, including the ones that do
/// nothing: this runs from a boot task, where an error that stops the fleet starting is far worse
/// than a machine staying on yesterday's build.
#[derive(Debug, PartialEq, Eq)]
pub enum SelfUpdate {
    /// No source configured and none found. Not an error — a machine that was never told where new
    /// builds come from should quietly never self-update.
    NoSource,
    /// The installed binary already reports the source's version.
    AlreadyCurrent(String),
    /// The source is OLDER. Refused for the same reason `update` refuses it, and it matters more
    /// here: this path runs unattended, so a stale source would reinstall itself at every boot.
    SourceIsOlder { source: String, installed: String },
    /// A newer build is available and should be swapped in.
    Install { version: String },
    /// One side's version is not a plain `x.y.z`, so "newer" cannot be established. Does nothing,
    /// and says so: a comparison that cannot be made is not evidence either way, and installing on
    /// a guess is how an unattended path replaces a working binary with a worse one.
    CannotCompare { source: String, installed: String },
}

/// Which binary `self-update` installs from, in precedence order.
///
/// Explicit beats ambient, and the repo build is last because it is the one that is stale by
/// accident: it is whatever was last compiled on that machine, which on 16 Sep 2026 was seven
/// releases behind what was installed.
pub fn resolve_source(cli: Option<&str>, env: Option<&str>, config: Option<&str>) -> Option<PathBuf> {
    // An empty string is "unset", not the current directory: it is what an untouched config field
    // and an exported-but-empty variable both look like, and treating it as a path would point the
    // installer at whatever happened to be there.
    for candidate in [cli, env, config] {
        if let Some(s) = candidate.map(str::trim).filter(|s| !s.is_empty()) {
            return Some(PathBuf::from(s));
        }
    }
    // ⛔ NO IMPLICIT FALLBACK, and in particular not this repo's `target/release` build.
    //
    // 0.4.19 had one, and it was worse than useless: `default_source_path` is RELATIVE, so it
    // resolved only when the process happened to be running inside the repo. Run from a repo shell
    // `self-update` installed the local build; run from a startup task — the entire reason the
    // command exists — the same command silently found nothing. One command, two behaviours, decided
    // by the working directory. Reported from a real starter within a day of shipping it.
    //
    // Making the path absolute would have fixed the inconsistency and kept the real hazard: a
    // logon would install whatever that machine last happened to compile, which is how the 16 Sep
    // near-downgrade happened. An unattended updater takes an explicit source or does nothing.
    // `update` keeps the repo default — it is typed by a person standing in the repo.
    None
}

/// What to do, given the source's version and the installed one. Pure, so the interesting
/// combinations are testable without touching a filesystem.
pub fn decide(source_version: Option<&str>, installed: Option<&str>) -> SelfUpdate {
    let Some(source) = source_version else {
        return SelfUpdate::NoSource;
    };
    // Nothing installed is a first install, not a downgrade — and it is the case this path exists
    // to fix on a machine that has never had the binary.
    let Some(installed) = installed else {
        return SelfUpdate::Install { version: source.to_string() };
    };
    if source == installed {
        return SelfUpdate::AlreadyCurrent(installed.to_string());
    }
    match (version_triple(source), version_triple(installed)) {
        (Some(new), Some(old)) if new > old => SelfUpdate::Install { version: source.to_string() },
        (Some(_), Some(_)) => SelfUpdate::SourceIsOlder {
            source: source.to_string(),
            installed: installed.to_string(),
        },
        _ => SelfUpdate::CannotCompare {
            source: source.to_string(),
            installed: installed.to_string(),
        },
    }
}

/// The three numbers in `x.y.z`, or `None` for anything else.
///
/// Numeric on purpose: compared as text, "0.4.10" sorts below "0.4.9", which would get the
/// comparison wrong in both directions at exactly the versions this project is at. Anything that is
/// not three plain integers — a git describe, a nightly tag, a `-dirty` suffix — returns `None`, and
/// every caller treats that as "cannot say" rather than as a verdict.
fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let mut parts = v.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
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

    // ---- the downgrade guard ----------------------------------------------

    fn versions(from: &str, to: Option<&str>) -> Plan {
        Plan {
            from: PathBuf::from("new"),
            to: PathBuf::from("installed"),
            from_version: from.into(),
            to_version: to.map(|s| s.to_string()),
            backup: PathBuf::from("backup"),
        }
    }

    /// The case this guard exists for, measured on a real machine on 16 Sep 2026: the repo's
    /// `target/release` build was 0.4.8 and the installed binary was 0.4.15, so the default `update`
    /// would have installed a seven-release-old binary over a working one — machine-wide, silently,
    /// and reporting success.
    #[test]
    fn an_older_source_is_refused_rather_than_installed_over_a_newer_build() {
        let why = versions("0.4.8", Some("0.4.15"))
            .downgrade_refusal()
            .expect("an older build was accepted as an update");
        assert!(why.contains("0.4.8") && why.contains("0.4.15"), "refusal names neither version: {why}");
    }

    /// A refusal a caller cannot get past is a bug report, not a guard. Deliberate downgrades are a
    /// real operation — a rollback is one — so the message has to name the way through.
    #[test]
    fn the_downgrade_refusal_says_how_to_override_it() {
        let why = versions("0.4.8", Some("0.4.15")).downgrade_refusal().unwrap();
        assert!(why.contains("--force"), "refusal does not say how to proceed deliberately: {why}");
    }

    #[test]
    fn a_newer_source_is_not_a_downgrade() {
        assert!(versions("0.4.16", Some("0.4.15")).downgrade_refusal().is_none());
    }

    #[test]
    fn an_equal_version_is_not_a_downgrade() {
        assert!(versions("0.4.15", Some("0.4.15")).downgrade_refusal().is_none());
    }

    /// ⚠️ The comparison must be NUMERIC. Compared as text, "0.4.10" sorts below "0.4.9", so a
    /// string comparison would wave through the downgrade this guard is for and block the upgrade
    /// past it — wrong in both directions at exactly the version numbers this project is at.
    #[test]
    fn versions_are_compared_numerically_not_as_text() {
        assert!(
            versions("0.4.10", Some("0.4.9")).downgrade_refusal().is_none(),
            "0.4.10 over 0.4.9 is an upgrade and was refused - the comparison is textual"
        );
        assert!(
            versions("0.4.9", Some("0.4.10")).downgrade_refusal().is_some(),
            "0.4.9 over 0.4.10 is a downgrade and was allowed - the comparison is textual"
        );
    }

    /// A comparison that cannot be made must say nothing rather than guess. A local build with an
    /// unusual version string is not evidence of anything, and blocking it would make the guard the
    /// reason a machine could not be updated.
    #[test]
    fn a_version_that_cannot_be_parsed_is_never_refused() {
        assert!(versions("nightly-abc", Some("0.4.15")).downgrade_refusal().is_none());
        assert!(versions("0.4.15", Some("nightly-abc")).downgrade_refusal().is_none());
    }

    /// An installed binary too old to report a version is the case `update` exists to end. Refusing
    /// it would leave the oldest machines the only ones that cannot be fixed.
    #[test]
    fn an_installed_binary_that_cannot_report_a_version_is_never_refused() {
        assert!(versions("0.4.15", None).downgrade_refusal().is_none());
    }

    // ---- self-update: where the build comes from, and whether to take it ----

    /// Explicit beats ambient, every time. A machine is told to use a specific binary precisely
    /// when the ambient answer is wrong, so the ambient answer must never win.
    #[test]
    fn an_explicit_source_beats_every_ambient_one() {
        assert_eq!(
            resolve_source(Some("cli.exe"), Some("env.exe"), Some("cfg.exe")),
            Some(PathBuf::from("cli.exe"))
        );
    }

    #[test]
    fn the_environment_beats_the_config() {
        assert_eq!(resolve_source(None, Some("env.exe"), Some("cfg.exe")), Some(PathBuf::from("env.exe")));
    }

    #[test]
    fn the_config_is_used_when_nothing_else_is_given() {
        assert_eq!(resolve_source(None, None, Some("cfg.exe")), Some(PathBuf::from("cfg.exe")));
    }

    /// ⛔ THE 0.4.19 DEFECT, pinned so it cannot come back. There was a fallback to this repo's
    /// `target/release` build, and `default_source_path` is RELATIVE — so the fallback resolved
    /// only when the process happened to be running inside the repo. From a repo shell the command
    /// installed the local build; from a startup task, which is the reason it exists, the same
    /// command silently found nothing. Reported from a real starter within a day of shipping it.
    ///
    /// An unattended updater takes an explicit source or does nothing at all. There is no third
    /// option that is not "install whatever this machine last happened to compile".
    #[test]
    fn nothing_configured_means_no_source_and_never_an_implicit_repo_build() {
        assert_eq!(resolve_source(None, None, None), None);
    }

    /// An empty string is "unset", not a path to the current directory. It is what an untouched
    /// config field and an exported-but-empty variable both look like.
    #[test]
    fn an_empty_setting_counts_as_unset() {
        assert_eq!(resolve_source(Some(""), Some(""), Some("")), None);
        assert_eq!(resolve_source(Some("   "), None, None), None);
    }

    #[test]
    fn a_newer_source_is_installed() {
        assert_eq!(
            decide(Some("0.4.19"), Some("0.4.18")),
            SelfUpdate::Install { version: "0.4.19".into() }
        );
    }

    #[test]
    fn an_equal_version_is_already_current_and_does_nothing() {
        assert_eq!(decide(Some("0.4.18"), Some("0.4.18")), SelfUpdate::AlreadyCurrent("0.4.18".into()));
    }

    /// ⛔ The one that matters unattended: a stale source must not be reinstalled at every boot.
    #[test]
    fn an_older_source_is_refused_rather_than_reinstalled_every_boot() {
        assert_eq!(
            decide(Some("0.4.8"), Some("0.4.18")),
            SelfUpdate::SourceIsOlder { source: "0.4.8".into(), installed: "0.4.18".into() }
        );
    }

    /// Versions compare numerically here too — the same trap as the downgrade guard.
    #[test]
    fn self_update_compares_versions_numerically() {
        assert_eq!(decide(Some("0.4.10"), Some("0.4.9")), SelfUpdate::Install { version: "0.4.10".into() });
        assert!(matches!(decide(Some("0.4.9"), Some("0.4.10")), SelfUpdate::SourceIsOlder { .. }));
    }

    /// Nothing installed yet is a first install, not a downgrade.
    #[test]
    fn a_first_install_is_an_install() {
        assert_eq!(decide(Some("0.4.19"), None), SelfUpdate::Install { version: "0.4.19".into() });
    }

    #[test]
    fn no_source_version_is_no_source() {
        assert_eq!(decide(None, Some("0.4.18")), SelfUpdate::NoSource);
    }

    /// A version neither side can parse must not be turned into a verdict. Unattended, "install
    /// because I could not tell" is how a working binary gets replaced by a worse one.
    #[test]
    fn a_version_that_cannot_be_compared_installs_nothing_and_says_so() {
        assert_eq!(
            decide(Some("nightly-abc"), Some("0.4.18")),
            SelfUpdate::CannotCompare { source: "nightly-abc".into(), installed: "0.4.18".into() }
        );
        assert!(matches!(
            decide(Some("0.4.18"), Some("built-from-source")),
            SelfUpdate::CannotCompare { .. }
        ));
    }

    /// Two identical unparseable strings are still "the same build", which is the common case for a
    /// locally built binary that has not moved.
    #[test]
    fn identical_unparseable_versions_are_already_current() {
        assert_eq!(decide(Some("nightly-abc"), Some("nightly-abc")), SelfUpdate::AlreadyCurrent("nightly-abc".into()));
    }
}
