//! The state socket: whether the desktop is held, told to whoever connects.
//!
//! A Unix socket the daemon listens on when `state_socket` names one. A
//! connection is told where the desktop stands — `held` or `free`, one word to
//! a line — and then again each time that changes; it says nothing itself. A
//! takeover is the desktop passing from one client to another, held before and
//! after, and is not a change. A display beside holds nothing.
//!
//! What it is for is the end of the stream. The kernel closes the socket when
//! the daemon is gone, however it went, so a follower learns of a daemon that
//! was killed where it stood exactly as it learns of one that stopped: nothing
//! the daemon does on its way out is being counted on. [`follow`] is that
//! follower, and reads a daemon it cannot reach as a desktop nobody holds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use log::{error, info, warn};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use crate::shared::{Seats, Shared};

/// A client is on the desktop.
const HELD: &str = "held";
/// Nobody is.
const FREE: &str = "free";

/// How long a follower waits before it tries the socket again.
const RETRY: Duration = Duration::from_secs(1);

/// Where the configured socket is: the path itself, or under
/// `$XDG_RUNTIME_DIR` when it is relative.
pub fn path(configured: &Path) -> anyhow::Result<PathBuf> {
    if configured.is_absolute() {
        return Ok(configured.to_owned());
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .with_context(|| format!("state_socket {} is relative and XDG_RUNTIME_DIR is not set", configured.display()))?;
    Ok(PathBuf::from(runtime).join(configured))
}

/// The socket's file, removed when this is dropped: a follower that finds no
/// file knows as much as one whose connection is refused, sooner.
pub struct Bound(PathBuf);

impl Drop for Bound {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Listen at `path`, in place of whatever a daemon before this one left there.
/// Only the owner can connect.
pub fn bind(path: &Path) -> anyhow::Result<(UnixListener, Bound)> {
    use std::os::unix::fs::DirBuilderExt as _;
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
    }
    // The owner's alone from the moment it exists: a mode set afterwards would
    // leave it open to others in between, in a directory this did not make.
    // The mask is the whole process's, and all another thread could get from it
    // meanwhile is a file more closed than it asked for.
    // SAFETY: umask only swaps the process's file mode creation mask.
    let mask = unsafe { libc::umask(0o177) };
    let listener = UnixListener::bind(path);
    // SAFETY: as above, putting back the mask it returned.
    unsafe { libc::umask(mask) };
    let listener = listener.with_context(|| format!("listening on {}", path.display()))?;
    Ok((listener, Bound(path.to_owned())))
}

/// Accept followers for as long as the daemon runs.
pub async fn serve(listener: UnixListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(tell(stream, shared.seats.subscribe()));
            }
            Err(e) => {
                error!("the state socket: accepting a follower: {e}");
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// Tell one follower where the desktop stands, and again whenever that
/// changes, until it hangs up or the compositor thread is gone.
async fn tell(mut stream: UnixStream, mut seats: watch::Receiver<Seats>) {
    let mut told = None;
    let mut ignored = [0u8; 64];
    loop {
        let held = seats.borrow_and_update().holder != 0;
        if told != Some(held) {
            let line = format!("{}\n", if held { HELD } else { FREE });
            if stream.write_all(line.as_bytes()).await.is_err() {
                return;
            }
            told = Some(held);
        }
        tokio::select! {
            changed = seats.changed() => if changed.is_err() {
                return;
            },
            // A follower has nothing to say; this is how its hanging up is seen.
            read = stream.read(&mut ignored) => if matches!(read, Ok(0) | Err(_)) {
                return;
            },
        }
    }
}

/// Keep `held` at what the daemon listening at `path` says, for good. A daemon
/// that cannot be reached — not started yet, stopped, killed — holds nothing,
/// and the socket is tried again until it is there.
pub async fn follow(path: PathBuf, held: watch::Sender<bool>) {
    let set = |now: bool| held.send_if_modified(|held| std::mem::replace(held, now) != now);
    // Whether the last attempt reached the daemon, so that a daemon that stays
    // away is logged once.
    let mut reached = true;
    loop {
        match UnixStream::connect(&path).await {
            Ok(stream) => {
                info!("following the daemon at {}", path.display());
                reached = true;
                let mut lines = BufReader::new(stream).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) if line == HELD => set(true),
                        Ok(Some(line)) if line == FREE => set(false),
                        Ok(Some(line)) => {
                            error!("the daemon said {line:?}, which is neither {HELD:?} nor {FREE:?}");
                            break;
                        }
                        Ok(None) => {
                            warn!("the daemon is gone");
                            break;
                        }
                        Err(e) => {
                            error!("reading {}: {e}", path.display());
                            break;
                        }
                    };
                }
            }
            Err(e) => {
                if reached {
                    warn!("no daemon at {}: {e}", path.display());
                }
                reached = false;
            }
        }
        set(false);
        tokio::time::sleep(RETRY).await;
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

    /// A directory of this test's own: the tests run at once.
    fn dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wlshare-state-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// What has arrived on `stream` within a moment, read with nothing of the
    /// module's own.
    async fn arrived(stream: &mut UnixStream) -> String {
        let mut text = Vec::new();
        let mut buffer = [0u8; 64];
        while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(200), stream.read(&mut buffer)).await {
            if n == 0 {
                text.extend_from_slice(b"<closed>");
                break;
            }
            text.extend_from_slice(&buffer[..n]);
        }
        String::from_utf8(text).unwrap()
    }

    #[tokio::test]
    async fn a_follower_is_told_where_the_desktop_stands_and_each_change_but_no_takeover() {
        let dir = dir("tell");
        let path = dir.join("nested").join("state.sock");
        let shared = shared();
        let (listener, bound) = bind(&path).unwrap();
        let serving = tokio::spawn(serve(listener, shared.clone()));
        let mut early = UnixStream::connect(&path).await.unwrap();
        assert_eq!(arrived(&mut early).await, "free\n");
        shared.set_holder(Some(ClientId(1)));
        assert_eq!(arrived(&mut early).await, "held\n");
        // A takeover: held before, held after.
        shared.set_holder(Some(ClientId(2)));
        assert_eq!(arrived(&mut early).await, "");
        // One that connects now starts from where the desktop stands.
        let mut late = UnixStream::connect(&path).await.unwrap();
        assert_eq!(arrived(&mut late).await, "held\n");
        shared.set_holder(None);
        assert_eq!(arrived(&mut early).await, "free\n");
        assert_eq!(arrived(&mut late).await, "free\n");
        // The daemon gone: the compositor thread's end of the seats with it.
        serving.abort();
        drop(shared);
        assert_eq!(arrived(&mut early).await, "<closed>");
        drop(bound);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn binding_replaces_a_socket_left_behind_and_keeps_others_out() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = dir("bind");
        let path = dir.join("state.sock");
        // A daemon killed where it stood leaves its file.
        let (listener, bound) = bind(&path).unwrap();
        std::mem::forget(bound);
        drop(listener);
        // SAFETY: umask only swaps the process's file mode creation mask.
        let mask = unsafe { libc::umask(0o022) };
        let (_listener, _bound) = bind(&path).unwrap();
        // SAFETY: as above.
        let restored = unsafe { libc::umask(mask) };
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // The mask the process had is the one it has again.
        assert_eq!(restored, 0o022);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_follower_reads_a_daemon_that_is_gone_as_a_desktop_nobody_holds() {
        let dir = dir("follow");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.sock");
        let (held, mut seen) = watch::channel(false);
        let following = tokio::spawn(follow(path.clone(), held));
        // No daemon yet: nothing holds it, and the socket is tried again. The
        // daemon here is a plain listener writing the lines by hand.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!*seen.borrow_and_update());
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = UnixListener::from_std(listener).unwrap();
        let (mut daemon, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await.unwrap().unwrap();
        daemon.write_all(b"free\nheld\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), seen.wait_for(|held| *held)).await.unwrap().unwrap();
        // Killed: no word, only the end of the stream.
        drop(daemon);
        tokio::time::timeout(Duration::from_secs(1), seen.wait_for(|held| !*held)).await.unwrap().unwrap();
        // And back: the follower finds it again by itself.
        let (mut daemon, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await.unwrap().unwrap();
        daemon.write_all(b"held\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), seen.wait_for(|held| *held)).await.unwrap().unwrap();
        // A word it does not know ends the connection, held by nobody.
        daemon.write_all(b"taken\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), seen.wait_for(|held| !*held)).await.unwrap().unwrap();
        following.abort();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_relative_socket_is_under_the_runtime_directory() {
        assert_eq!(path(Path::new("/run/x/state.sock")).unwrap(), PathBuf::from("/run/x/state.sock"));
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
            assert_eq!(path(Path::new("wlshare/state.sock")).unwrap(), PathBuf::from(runtime).join("wlshare/state.sock"));
        }
    }
}
