//! The `[hooks]` table: a command for the moment the desktop is taken, and one
//! for the moment it is released.
//!
//! The two follow the desktop, not the connections. A takeover is the desktop
//! passing from one client to another and runs neither, and a client that
//! leaves and comes straight back runs neither either: a release waits
//! `release_after_secs`, and a client that takes the desktop inside that wait
//! cancels it. A display beside holds nothing and counts for nothing here.
//!
//! One hook runs at a time, through `sh -c`, with the daemon's environment —
//! `WAYLAND_DISPLAY` and `SWAYSOCK` included, since it runs inside the session
//! it shares — and its output goes where the daemon's does. While one runs the
//! desktop may be taken and released several times; when it exits, the hook for
//! where the desktop stands *now* runs if that differs from what the last hook
//! told it. A hook still running at `timeout_secs` is killed, so one that hangs
//! cannot hold the other back: the one that puts the monitors back is the one
//! an operator is counting on. Each hook is a process group of its own, and the
//! whole group is what is killed: the shell is rarely the process that hangs.

use std::sync::Arc;
use std::time::Duration;

use log::{error, info, warn};
use serde::Deserialize;
use tokio::time::Instant;

use crate::shared::Shared;

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Run when a client takes a desktop nobody held.
    pub taken: Option<String>,
    /// Run when the client on the desktop has left and nobody has taken it
    /// for `release_after_secs`.
    pub released: Option<String>,
    /// How long a desktop stays unreleased after its client leaves, for the
    /// client to come back without the two hooks running in between.
    #[serde(default = "default_release_after_secs")]
    pub release_after_secs: u64,
    /// How long a hook may run before it is killed.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_release_after_secs() -> u64 {
    5
}

fn default_timeout_secs() -> u64 {
    60
}

impl Hooks {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.taken.is_some() || self.released.is_some(), "[hooks] names neither `taken` nor `released`");
        anyhow::ensure!(self.timeout_secs > 0, "[hooks] timeout_secs must be at least 1");
        Ok(())
    }
}

/// Follow who holds the desktop and run the hooks at its transitions, until the
/// compositor thread is gone.
pub async fn run(shared: Arc<Shared>, hooks: Hooks) {
    let mut seats = shared.seats.subscribe();
    // What the last hook told the desktop: held, or not. Nobody holds it when
    // the daemon starts, and no hook has said otherwise.
    let mut established = false;
    // When a release that is waiting falls due.
    let mut release_at: Option<Instant> = None;
    loop {
        let held = seats.borrow_and_update().holder != 0;
        if held == established {
            release_at = None;
            if seats.changed().await.is_err() {
                return;
            }
            continue;
        }
        if !held {
            let due = *release_at.get_or_insert_with(|| Instant::now() + Duration::from_secs(hooks.release_after_secs));
            match tokio::time::timeout_at(due, seats.changed()).await {
                Ok(Err(_)) => return,
                // Something changed: the loop reads where the desktop stands.
                Ok(Ok(())) => continue,
                Err(_) => {}
            }
            release_at = None;
        }
        let (name, command) = if held { ("taken", &hooks.taken) } else { ("released", &hooks.released) };
        if let Some(command) = command {
            run_hook(name, command, Duration::from_secs(hooks.timeout_secs)).await;
        }
        established = held;
    }
}

/// Run one hook to its end, or to the timeout.
async fn run_hook(name: &str, command: &str, timeout: Duration) {
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
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => info!("desktop {name}: hook finished"),
        Ok(Ok(status)) => warn!("desktop {name}: hook exited with {status}"),
        Ok(Err(e)) => error!("desktop {name}: waiting for the hook: {e}"),
        Err(_) => {
            error!("desktop {name}: hook still running after {}s; killing it", timeout.as_secs());
            // The group is named by the shell's pid, which is still the
            // shell's: nothing has waited for it.
            if let Some(pid) = child.id()
                && unsafe { libc::killpg(pid as libc::pid_t, libc::SIGKILL) } != 0
            {
                error!("desktop {name}: killing the hook's process group: {}", std::io::Error::last_os_error());
            }
            // The shell itself, should the group have been missed, and its reaping.
            if let Err(e) = child.kill().await {
                error!("desktop {name}: killing the hook: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::Framebuffer;
    use crate::shared::{ClientId, Displays, Geometry};

    fn shared() -> Arc<Shared> {
        let (commands, _rx) = calloop::channel::channel();
        Arc::new(Shared::new(Framebuffer::new(1, 1), Geometry { width: 1, height: 1, scale: 1.0 }, Displays::default(), commands))
    }

    /// Hooks that append a word to a file, so a test can read what ran in what
    /// order.
    fn hooks(log: &std::path::Path, release_after_secs: u64) -> Hooks {
        let line = |word: &str| format!("echo {word} >> {}", log.display());
        Hooks { taken: Some(line("taken")), released: Some(line("released")), release_after_secs, timeout_secs: 5 }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    fn lines(log: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_owned).collect()
    }

    #[tokio::test]
    async fn a_takeover_runs_nothing_and_a_release_waits_its_grace() {
        let dir = tempfile_dir("takeover");
        let log = dir.join("log");
        let shared = shared();
        let task = tokio::spawn(run(shared.clone(), hooks(&log, 1)));
        shared.set_holder(Some(ClientId(1)));
        settle().await;
        assert_eq!(lines(&log), ["taken"]);
        // A takeover: held before, held after.
        shared.set_holder(Some(ClientId(2)));
        settle().await;
        assert_eq!(lines(&log), ["taken"]);
        // Left and back inside the grace: nothing.
        shared.set_holder(None);
        settle().await;
        shared.set_holder(Some(ClientId(3)));
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(lines(&log), ["taken"]);
        // Left for good: released once the grace is up, not before.
        shared.set_holder(None);
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(lines(&log), ["taken"]);
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&log), ["taken", "released"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_hook_for_where_the_desktop_stands_runs_after_a_slow_one() {
        let dir = tempfile_dir("slow");
        let log = dir.join("log");
        let shared = shared();
        let hooks = Hooks {
            taken: Some(format!("sleep 0.5; echo taken >> {}", log.display())),
            released: Some(format!("echo released >> {}", log.display())),
            release_after_secs: 0,
            timeout_secs: 5,
        };
        let task = tokio::spawn(run(shared.clone(), hooks));
        shared.set_holder(Some(ClientId(1)));
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Taken, released and taken again while the slow hook runs: it ends with
        // the desktop held, which is what the first hook said, so nothing more.
        shared.set_holder(None);
        shared.set_holder(Some(ClientId(2)));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(lines(&log), ["taken"]);
        // With no hook running and no grace, a release runs at once.
        shared.set_holder(None);
        settle().await;
        assert_eq!(lines(&log), ["taken", "released"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_hook_past_its_timeout_is_killed_and_the_next_still_runs() {
        let dir = tempfile_dir("timeout");
        let log = dir.join("log");
        let shared = shared();
        let hooks = Hooks {
            // What hangs is the shell, and what would write late is a child of
            // it that killing the shell alone leaves running.
            taken: Some(format!("(sleep 2; echo late >> {}) & sleep 30", log.display())),
            released: Some(format!("echo released >> {}", log.display())),
            release_after_secs: 0,
            timeout_secs: 1,
        };
        let task = tokio::spawn(run(shared.clone(), hooks));
        shared.set_holder(Some(ClientId(1)));
        tokio::time::sleep(Duration::from_millis(200)).await;
        shared.set_holder(None);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(lines(&log), ["released"]);
        // Past when the child would have written: it went with its group.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(lines(&log), ["released"]);
        task.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_table_needs_a_command_and_a_timeout() {
        let none: Hooks = toml::from_str("").unwrap();
        assert!(none.validate().is_err());
        let taken: Hooks = toml::from_str("taken = \"true\"").unwrap();
        taken.validate().unwrap();
        assert_eq!(taken.release_after_secs, 5);
        assert_eq!(taken.timeout_secs, 60);
        let zero: Hooks = toml::from_str("released = \"true\"\ntimeout_secs = 0").unwrap();
        assert!(zero.validate().is_err());
    }

    /// A directory of this test's own: the tests run at once, and a clock
    /// alone has named two of them the same.
    fn tempfile_dir(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("wlshare-hooks-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
