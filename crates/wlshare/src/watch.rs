//! `wlshare watch`: a command for a desktop that is held and one for a desktop
//! that is free, run by a process that is not the daemon.
//!
//! The watcher follows the daemon's state socket ([`crate::state`]) and keeps
//! the session at what it says: *held* runs when a client is on the desktop,
//! *free* when nobody has been for `free_after_secs`. A daemon that is not
//! there holds nothing, so one that was killed where it stood frees the desktop
//! as one that stopped does, and a daemon that comes back inside the wait, or a
//! client that leaves and comes straight back, runs neither. A takeover is not
//! a change the socket reports.
//!
//! It knows nothing of what the commands did before it started, so the first
//! thing it does, once the daemon has said where the desktop stands or has
//! turned out not to be there, is run the one for that: both have to be
//! safe to run on a session already as they would leave it. After that a
//! command runs only when the desktop stands otherwise than the last one said.
//!
//! One command runs at a time, through `sh -c`, with the watcher's environment
//! and its output going where the watcher's does. While one runs the desktop
//! may change hands several times; when it exits, the command for where the
//! desktop stands *now* runs if that differs, *free* as soon as the wait since
//! the client left is over, whether or not a command was running through it. A
//! command still running at `timeout_secs` is killed, so one that hangs cannot
//! hold the other back: the one that puts the monitors back is the one an
//! operator is counting on. Each is a process group of its own, and the whole
//! group is what is killed, there or when the task running it is dropped: the
//! shell is rarely the process that hangs. A watcher that stops with the desktop
//! held frees it on its way out, without the wait.

use std::time::Duration;

use log::{error, info, warn};
use tokio::sync::{oneshot, watch};
use tokio::time::Instant;

#[derive(clap::Args, Debug, Clone)]
pub struct Watch {
    /// Run when a client is on the desktop.
    #[arg(long, value_name = "COMMAND")]
    pub held: Option<String>,
    /// Run when nobody has been on the desktop for --free-after-secs, the
    /// daemon having gone included.
    #[arg(long, value_name = "COMMAND")]
    pub free: Option<String>,
    /// How long a desktop stays held after its client leaves, for the client
    /// to come back, or the daemon to, without the two commands running in
    /// between.
    #[arg(long, default_value_t = 5)]
    pub free_after_secs: u64,
    /// How long a command may run before it is killed.
    #[arg(long, default_value_t = 60)]
    pub timeout_secs: u64,
}

impl Watch {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.held.is_some() || self.free.is_some(), "watch names neither --held nor --free");
        anyhow::ensure!(self.timeout_secs > 0, "--timeout-secs must be at least 1");
        Ok(())
    }
}

/// Keep the session at what `desktop` says — `None` being not known yet, for
/// which nothing runs — until `stop` is sent or dropped or
/// nothing is following the daemon any more. The desktop is then freed, if the
/// last command said it was held: nothing else would put back what *held* did.
pub async fn run(mut desktop: watch::Receiver<Option<bool>>, watch: Watch, mut stop: oneshot::Receiver<()>) {
    let timeout = Duration::from_secs(watch.timeout_secs);
    // What the last command told the session: held, or free. None has run yet,
    // and what the session was left as is not known.
    let mut established = None;
    // Since when nobody has held the desktop, while nobody does.
    let mut vacant_since: Option<Instant> = None;
    loop {
        let held = observe(&mut desktop, &mut vacant_since);
        if held.is_none() || established == held {
            tokio::select! {
                biased;
                _ = &mut stop => break,
                changed = desktop.changed() => if changed.is_err() {
                    break;
                },
            }
            continue;
        }
        // The desktop is freed a wait after it was left, however much of that
        // a command was running for.
        if let Some(since) = vacant_since {
            let due = since + Duration::from_secs(watch.free_after_secs);
            tokio::select! {
                biased;
                _ = &mut stop => break,
                changed = tokio::time::timeout_at(due, desktop.changed()) => match changed {
                    Ok(Err(_)) => break,
                    // Something changed: the loop reads where the desktop stands.
                    Ok(Ok(())) => continue,
                    Err(_) => {}
                },
            }
        }
        let held = held == Some(true);
        let (name, command) = if held { ("held", &watch.held) } else { ("free", &watch.free) };
        if let Some(command) = command {
            let running = run_command(name, command, timeout);
            tokio::pin!(running);
            // The desktop is watched while the command runs, for when it is left.
            let mut watching = true;
            loop {
                tokio::select! {
                    () = &mut running => break,
                    changed = desktop.changed(), if watching => match changed {
                        Ok(()) => {
                            observe(&mut desktop, &mut vacant_since);
                        }
                        Err(_) => watching = false,
                    },
                }
            }
        }
        established = Some(held);
    }
    if established == Some(true) && let Some(command) = &watch.free {
        info!("stopping with the desktop held: freeing it");
        run_command("free", command, timeout).await;
    }
}

/// Whether the desktop is held now, if that is known yet, with `vacant_since`
/// kept to match.
fn observe(desktop: &mut watch::Receiver<Option<bool>>, vacant_since: &mut Option<Instant>) -> Option<bool> {
    let held = *desktop.borrow_and_update();
    match held {
        Some(true) => *vacant_since = None,
        Some(false) => {
            vacant_since.get_or_insert_with(Instant::now);
        }
        None => {}
    }
    held
}

/// A running command's process group, killed if this is dropped before the command
/// has ended: the task was cancelled, and `kill_on_drop` reaches the shell
/// alone.
struct Group(Option<libc::pid_t>);

impl Group {
    fn kill(&mut self) -> std::io::Result<()> {
        let Some(pid) = self.0.take() else { return Ok(()) };
        if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

/// Run one command to its end, or to the timeout.
async fn run_command(name: &str, command: &str, timeout: Duration) {
    info!("desktop {name}: running {command:?}");
    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        // A group of its own, so a timeout reaches what the shell started.
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return error!("desktop {name}: starting {command:?}: {e}"),
    };
    // The group is named by the shell's pid, which stays the shell's until
    // something has waited for it.
    let mut group = Group(child.id().map(|pid| pid as libc::pid_t));
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => info!("desktop {name}: command finished"),
        Ok(Ok(status)) => warn!("desktop {name}: command exited with {status}"),
        Ok(Err(e)) => error!("desktop {name}: waiting for the command: {e}"),
        Err(_) => {
            error!("desktop {name}: command still running after {}s; killing it", timeout.as_secs());
            if let Err(e) = group.kill() {
                error!("desktop {name}: killing the command's process group: {e}");
            }
            // The shell itself, should the group have been missed, and its reaping.
            if let Err(e) = child.kill().await {
                error!("desktop {name}: killing the command: {e}");
            }
        }
    }
    // The command has ended: what it left running on purpose is left, and its pid
    // may be another's by now.
    group.0 = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Commands that append a word to a file, so a test can read what ran in
    /// what order.
    fn watch(log: &std::path::Path, free_after_secs: u64) -> Watch {
        let line = |word: &str| format!("echo {word} >> {}", log.display());
        Watch { held: Some(line("held")), free: Some(line("free")), free_after_secs, timeout_secs: 5 }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    fn lines(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn the_command_for_where_the_desktop_stands_runs_first_whichever_it_is() {
        let dir = tempfile_dir("first");
        // Free when the watcher starts, as after a daemon that was killed with
        // the desktop held: the session is put back, once the wait is over.
        let log = dir.join("free");
        let (_desktop, following) = watch::channel(Some(false));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 1), stopped));
        settle().await;
        assert_eq!(lines(&log), Vec::<String>::new());
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(lines(&log), ["free"]);
        task.abort();
        // Held when it starts: no wait.
        let log = dir.join("held");
        let (_desktop, following) = watch::channel(Some(true));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 1), stopped));
        settle().await;
        assert_eq!(lines(&log), ["held"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn nothing_runs_until_the_desktop_is_known() {
        let dir = tempfile_dir("unknown");
        let log = dir.join("log");
        // No wait at all, which is what would run `free` at once.
        let (desktop, following) = watch::channel(None);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 0), stopped));
        settle().await;
        assert_eq!(lines(&log), Vec::<String>::new());
        // Held is the first thing known, and the first thing run.
        desktop.send(Some(true)).unwrap();
        settle().await;
        assert_eq!(lines(&log), ["held"]);
        stop.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(lines(&log), ["held", "free"]);
        // Stopped before anything was known: nothing.
        let log = dir.join("stopped");
        let (_desktop, following) = watch::channel(None);
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 0), stopped));
        settle().await;
        stop.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(lines(&log), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_desktop_left_and_taken_inside_the_wait_runs_nothing() {
        let dir = tempfile_dir("wait");
        let log = dir.join("log");
        let (desktop, following) = watch::channel(Some(true));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 1), stopped));
        settle().await;
        assert_eq!(lines(&log), ["held"]);
        // Left and back inside the wait, as a client on a flapping link is and
        // as a daemon that was restarted is: nothing.
        desktop.send(Some(false)).unwrap();
        settle().await;
        desktop.send(Some(true)).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(lines(&log), ["held"]);
        // Left for good: freed once the wait is up, not before.
        desktop.send(Some(false)).unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(lines(&log), ["held"]);
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&log), ["held", "free"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_command_for_where_the_desktop_stands_runs_after_a_slow_one() {
        let dir = tempfile_dir("slow");
        let log = dir.join("log");
        let watch = Watch {
            held: Some(format!("sleep 0.5; echo held >> {}", log.display())),
            free: Some(format!("echo free >> {}", log.display())),
            free_after_secs: 0,
            timeout_secs: 5,
        };
        let (desktop, following) = watch::channel(Some(true));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch, stopped));
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Left and held again while the slow command runs: it ends with the
        // desktop held, which is what it said, so nothing more.
        desktop.send(Some(false)).unwrap();
        desktop.send(Some(true)).unwrap();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&log), ["held"]);
        // With no command running and no wait, the desktop is freed at once.
        desktop.send(Some(false)).unwrap();
        settle().await;
        assert_eq!(lines(&log), ["held", "free"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_command_past_its_timeout_is_killed_and_the_next_still_runs() {
        let dir = tempfile_dir("timeout");
        let log = dir.join("log");
        let watch = Watch {
            // What hangs is the shell, and what would write late is a child of
            // it that killing the shell alone leaves running.
            held: Some(format!("(sleep 2; echo late >> {}) & sleep 30", log.display())),
            free: Some(format!("echo free >> {}", log.display())),
            free_after_secs: 0,
            timeout_secs: 1,
        };
        let (desktop, following) = watch::channel(Some(true));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch, stopped));
        tokio::time::sleep(Duration::from_millis(200)).await;
        desktop.send(Some(false)).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(lines(&log), ["free"]);
        // Past when the child would have written: it went with its group.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(lines(&log), ["free"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_wait_counts_from_the_leave_and_not_from_a_slow_command() {
        let dir = tempfile_dir("grace");
        let log = dir.join("log");
        let watch = Watch {
            held: Some(format!("sleep 1; echo held >> {}", log.display())),
            free: Some(format!("echo free >> {}", log.display())),
            free_after_secs: 1,
            timeout_secs: 5,
        };
        let (desktop, following) = watch::channel(Some(true));
        let (_stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch, stopped));
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Left while the slow command runs: the wait is over soon after it
        // ends, and one counted from its end would not be for another second.
        desktop.send(Some(false)).unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(lines(&log), ["held", "free"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn stopping_frees_a_desktop_that_was_held_and_no_other() {
        let dir = tempfile_dir("stop");
        let log = dir.join("log");
        // No command has run yet: nothing of the watcher's to put back.
        let (_desktop, following) = watch::channel(Some(false));
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 60), stopped));
        settle().await;
        stop.send(()).unwrap();
        task.await.unwrap();
        assert_eq!(lines(&log), Vec::<String>::new());
        // Held when it stops: freed, and with no wait.
        let (_desktop, following) = watch::channel(Some(true));
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(run(following, watch(&log, 60), stopped));
        settle().await;
        drop(stop);
        tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap();
        assert_eq!(lines(&log), ["held", "free"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_command_whose_task_is_dropped_is_killed_with_what_it_started() {
        let dir = tempfile_dir("dropped");
        let log = dir.join("log");
        let command = format!("(sleep 1; echo late >> {}) & sleep 30", log.display());
        let task = tokio::spawn(async move { run_command("held", &command, Duration::from_secs(60)).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();
        tokio::time::sleep(Duration::from_millis(1300)).await;
        assert_eq!(lines(&log), Vec::<String>::new());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_watcher_needs_a_command_and_a_timeout() {
        let watch = |held: Option<&str>, timeout_secs| Watch { held: held.map(str::to_owned), free: None, free_after_secs: 5, timeout_secs };
        assert!(watch(None, 60).validate().is_err());
        watch(Some("true"), 60).validate().unwrap();
        assert!(watch(Some("true"), 0).validate().is_err());
    }

    /// A directory of this test's own: the tests run at once, and a clock
    /// alone has named two of them the same.
    fn tempfile_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wlshare-watch-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
