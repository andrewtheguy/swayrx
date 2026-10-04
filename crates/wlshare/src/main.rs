//! wlshare: a VNC server for wlroots-based Wayland compositors that tells the
//! remotex gateway what pixel density its framebuffer is drawn at.
//!
//! Two halves. One thread owns the Wayland connection and everything on it —
//! capture, outputs, input, clipboard — and runs a calloop that also polls a
//! command channel ([`compositor`]). The tokio runtime accepts clients and runs
//! one task per connection ([`session`]). They share the framebuffer and a few
//! channels ([`shared`]), and every protocol byte comes from the `wlshare-rfb`
//! crate. A client that enables audio gets a PipeWire capture thread of its
//! own for as long as it listens ([`audio`]), and a client that plugs a camera
//! gets a PipeWire video source, decoded by libavcodec, for as long as it is
//! plugged ([`camera`], [`decode`]); a client that plugs a microphone gets a
//! PipeWire audio source the same way ([`microphone`]). Whether the desktop is
//! held is told on a socket to whoever connects ([`state`]), and `wlshare watch`
//! is a process of its own that follows it and runs a command for a desktop
//! that is held and one for a desktop that is free ([`watch`]).

mod audio;
mod auth;
mod camera;
mod capture;
mod clipboard;
mod compositor;
mod config;
mod cursor;
mod decode;
mod framebuffer;
mod input;
mod microphone;
mod outputs;
mod pam;
mod session;
mod shared;
mod state;
mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use log::{error, info};
use wlshare_rfb::rsa_aes::ServerKey;

use crate::auth::Login;
use crate::session::{RsaAes, Security};

#[derive(Parser, Debug)]
#[command(name = "wlshare", version, about = "A VNC server for wlroots-based Wayland compositors, with pixel density on the wire")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// The configuration file (default: $XDG_CONFIG_HOME/wlshare/config.toml).
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Listen on this address instead of the configured one.
    #[arg(long)]
    listen: Option<std::net::SocketAddr>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Hash a password for the `hash` field of the `[password]` table, and
    /// print it. The password is read from the terminal without echo, or from
    /// standard input when that is not a terminal.
    HashPassword,
    /// Follow the daemon's state socket and keep the session at what it says:
    /// one command for a desktop a client is on, one for a desktop nobody is
    /// on, the daemon having gone included. The first to run is the one for
    /// where the desktop stands when this starts, so both must be safe to run
    /// twice.
    Watch(watch::Watch),
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let path = args.config.unwrap_or_else(config::default_path);
    let watch = match args.command {
        Some(Command::HashPassword) => return hash_password(),
        Some(Command::Watch(watch)) => Some(watch),
        None => None,
    };
    let mut config = if path.exists() {
        config::Config::load(&path)?
    } else {
        info!("no configuration at {}; using defaults", path.display());
        toml::from_str("").context("default configuration")?
    };
    if let Some(watch) = watch {
        return follow(&config, &path, watch);
    }
    if let Some(listen) = args.listen {
        config.listen = listen;
    }
    let security = security(&config, &path)?;

    // The compositor thread comes up first and hands back what it learned about
    // the output, so ServerInit can name a size before the first frame.
    let (compositor, shared) = compositor::start(&config)?;

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let result = runtime.block_on(serve(config, security, shared.clone(), compositor));
    if let Err(e) = &result {
        error!("{e:#}");
    }
    result
}

/// `wlshare watch`: follow the state socket the configuration names until
/// interrupted or terminated.
fn follow(config: &config::Config, config_path: &std::path::Path, watch: watch::Watch) -> anyhow::Result<()> {
    watch.validate()?;
    let socket = config.state_socket.as_deref().with_context(|| format!("{} names no state_socket to follow", config_path.display()))?;
    let socket = state::path(socket)?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut terminated = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).context("handling SIGTERM")?;
        // Not known until the daemon has said, or has turned out not to be there.
        let (held, desktop) = tokio::sync::watch::channel(None);
        let following = tokio::spawn(state::follow(socket, held));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let watching = tokio::spawn(watch::run(desktop, watch, stopped));
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("interrupted"),
            _ = terminated.recv() => info!("terminated"),
        }
        // A session the watcher left held is freed before it goes.
        let _ = stop.send(());
        watching.await.context("the watcher")?;
        following.abort();
        Ok(())
    })
}

/// What the configuration says about who may connect: the login one of the two
/// tables asks for, with the RSA-AES key that carries it loaded — or generated
/// and written, on a first start. Neither table means no authentication at all,
/// and the offer says so.
fn security(config: &config::Config, config_path: &std::path::Path) -> anyhow::Result<Security> {
    let login = match (&config.pam, &config.password) {
        (Some(pam), _) => Some(Login::Pam { service: pam.service.clone(), account: pam::process_user()? }),
        (None, Some(password)) => {
            auth::check_hash(&password.hash).context("the [password] table's hash")?;
            Some(Login::Password { hash: password.hash.clone() })
        }
        (None, None) => None,
    };
    let Some(login) = login else {
        info!("no [pam] or [password] table: accepting clients without authentication, in the clear");
        return Ok(Security::default());
    };
    let key = server_key(config.rsa_key_file(config_path).expect("a login is configured"))?;
    let (bits, fingerprint) = (key.bits(), key.fingerprint());
    match &login {
        Login::Pam { service, account } => info!("RSA-AES with PAM service {service:?}: the login is {account:?}'s; server key {bits} bits, fingerprint {fingerprint}"),
        Login::Password { .. } => info!("RSA-AES with the configured password, which names no account; server key {bits} bits, fingerprint {fingerprint}"),
    }
    Ok(Security { rsa_aes: Some(RsaAes { key: Arc::new(key), login }) })
}

/// The server's RSA key from `key_path`, generated there on a first start.
fn server_key(key_path: std::path::PathBuf) -> anyhow::Result<ServerKey> {
    let key = match std::fs::read_to_string(&key_path) {
        Ok(pem) => ServerKey::from_pem(&pem).with_context(|| format!("{} is not a PKCS#8 PEM RSA key", key_path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            info!("generating a {}-bit RSA key into {}", wlshare_rfb::rsa_aes::SERVER_KEY_BITS, key_path.display());
            let key = ServerKey::generate().context("generating the RSA key")?;
            if let Some(dir) = key_path.parent() {
                std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            write_private(&key_path, key.to_pem().context("encoding the RSA key")?.as_bytes())?;
            key
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", key_path.display())),
    };
    Ok(key)
}

/// `wlshare hash-password`: read a password and print the PHC string the
/// `[password]` table wants. Asked twice at a terminal, where a typo would
/// otherwise become a password nobody knows; once from a pipe, which has no one
/// to ask.
fn hash_password() -> anyhow::Result<()> {
    let interactive = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let password = read_password("Password: ", interactive)?;
    if interactive {
        anyhow::ensure!(password == read_password("Again: ", interactive)?, "the two passwords differ");
    }
    println!("{}", auth::hash_password(&password)?);
    Ok(())
}

/// One line of standard input, without echoing it when that is a terminal. The
/// prompt goes to standard error, so the hash alone is what a pipe collects.
fn read_password(prompt: &str, interactive: bool) -> anyhow::Result<String> {
    use std::io::{BufRead as _, Write as _};
    let mut line = String::new();
    let read = if interactive {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let _echo_off = EchoOff::new()?;
        let read = std::io::stdin().lock().read_line(&mut line);
        eprintln!();
        read
    } else {
        std::io::stdin().lock().read_line(&mut line)
    };
    read.context("reading the password")?;
    Ok(line.trim_end_matches(['\n', '\r']).to_owned())
}

/// Echo off on the terminal for as long as this lives, and the settings back as
/// they were when it does not — an error on the way included.
struct EchoOff(libc::termios);

impl EchoOff {
    fn new() -> anyhow::Result<Self> {
        // SAFETY: both calls take a termios this function owns, and the file
        // descriptor is the process's own standard input.
        unsafe {
            let mut term: libc::termios = std::mem::zeroed();
            anyhow::ensure!(libc::tcgetattr(libc::STDIN_FILENO, &mut term) == 0, "tcgetattr: {}", std::io::Error::last_os_error());
            let saved = term;
            term.c_lflag &= !libc::ECHO;
            anyhow::ensure!(
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &term) == 0,
                "tcsetattr: {}",
                std::io::Error::last_os_error()
            );
            Ok(Self(saved))
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: the termios is the one tcgetattr filled, and restoring it
        // cannot fail in a way this can answer.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.0);
        }
    }
}

/// Write a file only its owner can read, created fresh.
fn write_private(path: &std::path::Path, contents: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(contents).with_context(|| format!("writing {}", path.display()))
}

async fn serve(
    config: config::Config,
    security: Security,
    shared: Arc<shared::Shared>,
    mut compositor: compositor::Handle,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.listen).await.with_context(|| format!("listening on {}", config.listen))?;
    info!("listening on {}", config.listen);
    // Removed with the daemon, when the daemon gets to: a follower reads the
    // closed socket and not the file.
    let _state = match &config.state_socket {
        Some(socket) => {
            let socket = state::path(socket)?;
            let (followers, bound) = state::bind(&socket)?;
            info!("telling whether the desktop is held on {}", socket.display());
            tokio::spawn(state::serve(followers, shared.clone()));
            Some(bound)
        }
        None => None,
    };
    // What systemd stops a unit with; without a handler it ends the process
    // where it stands, the socket's file left behind.
    let mut terminated = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).context("handling SIGTERM")?;
    let session_config = Arc::new(session::SessionConfig {
        security,
        name: config.name.clone(),
        resize: config.resize,
        capture: std::time::Duration::from_secs_f64(1.0 / f64::from(config.max_fps)),
        audio: config.audio,
        camera: config.camera,
        microphone: config.microphone,
        handshake_timeout: std::time::Duration::from_secs(config.handshake_timeout_secs),
    });
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, peer) = match accepted.context("accepting a client") {
                    Ok(accepted) => accepted,
                    Err(e) => break Err(e),
                };
                let id = shared.next_client();
                info!("client {} connected from {peer}", id.0);
                let shared = shared.clone();
                let session_config = session_config.clone();
                tokio::spawn(async move {
                    if let Err(e) = session::run(id, socket, shared.clone(), session_config).await {
                        info!("client {}: {e:#}", id.0);
                    }
                    shared.command(shared::Command::ClientLeft(id));
                    info!("client {} disconnected", id.0);
                });
            }
            result = compositor.exited() => {
                break match result {
                    Ok(()) => Err(anyhow::anyhow!("the compositor connection closed")),
                    Err(e) => Err(e),
                };
            }
            _ = tokio::signal::ctrl_c() => {
                info!("interrupted");
                break Ok(());
            }
            _ = terminated.recv() => {
                info!("terminated");
                break Ok(());
            }
        }
    }
}
