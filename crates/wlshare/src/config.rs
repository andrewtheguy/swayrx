//! The configuration file: what the server listens on, who may connect, which
//! output it shares, whether its sound goes with it, whether a client may lend
//! it a camera and a microphone, where it says whether the desktop is held, and
//! how the virtual keyboard is laid out.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where to accept clients. With neither `[pam]` nor `[password]` the
    /// session is unauthenticated and not encrypted, so this is a loopback or a
    /// VPN address unless something in front of it encrypts.
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    /// RSA-AES with the system login: the client names the account wlshare
    /// runs as and gives its password, PAM checks the two, and the session is
    /// encrypted.
    pub pam: Option<Pam>,
    /// RSA-AES with one configured password and no account at all, for a host
    /// with no system password to spend on a client. Not with `[pam]`: the
    /// server asks for one credential or the other, not both. With neither,
    /// anyone who reaches the port is in.
    pub password: Option<Password>,
    /// The output to capture, by its `wl_output` name; absent means the first
    /// one the compositor lists.
    pub output: Option<String>,
    /// Whether clients may resize the output and set its scale. Only a headless
    /// output is ever reconfigured.
    #[serde(default = "default_true")]
    pub resize: bool,
    /// The most frames captured per second.
    #[serde(default = "default_max_fps")]
    pub max_fps: u32,
    /// How many seconds a connection has to finish the handshake — the
    /// security exchange and the login — before it is dropped. A client that
    /// asks its user to confirm the server key and then type a password spends
    /// most of this waiting on the person.
    #[serde(default = "default_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,
    /// Whether a client may hear the desktop, off unless set: the audio
    /// extension is announced to a client that asks, and while the client has
    /// it enabled a sink of wlshare's own is the default, captured from
    /// PipeWire: streams that follow the default play there and are silent on
    /// the host, and one pinned to a sink of the host's is still heard on it.
    #[serde(default)]
    pub audio: bool,
    /// Whether a client may lend the desktop its camera, off unless set: the
    /// camera extension is announced to a client that asks, and a camera it
    /// plugs becomes a PipeWire video source until it unplugs or leaves.
    #[serde(default)]
    pub camera: bool,
    /// Whether a client may lend the desktop its microphone, off unless set:
    /// the microphone extension is announced to a client that asks, and a
    /// microphone it plugs becomes a PipeWire audio source until it unplugs or
    /// leaves.
    #[serde(default)]
    pub microphone: bool,
    /// The desktop name in ServerInit.
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default)]
    pub xkb: Xkb,
    /// A Unix socket that tells whoever connects whether a client is on the
    /// desktop, and again whenever that changes; under `$XDG_RUNTIME_DIR`
    /// when the path is relative. `wlshare watch` follows it and runs a
    /// command for each, so the compositor can be told to move the windows
    /// onto a headless output and the monitors back on. Absent means no
    /// socket.
    pub state_socket: Option<PathBuf>,
}

/// The keymap the virtual keyboard uploads and the server resolves keysyms
/// through — RMLVO names, as `xkbcommon` takes them. Empty means the library's
/// default; `XKB_CONFIG_EXTRA_PATH` in the environment adds a search directory
/// for a custom layout.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Xkb {
    #[serde(default)]
    pub rules: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub layout: String,
    #[serde(default)]
    pub variant: String,
    #[serde(default)]
    pub options: String,
}

/// The `[pam]` table: RSA-AES security with the credentials checked by PAM.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Pam {
    /// The PAM service name, which is the stack under `/etc/pam.d`.
    #[serde(default = "default_pam_service")]
    pub service: String,
    /// The server's RSA key as PKCS#8 PEM, generated on first start when the
    /// file is missing. Absent means `rsa_key.pem` beside the configuration
    /// file.
    pub rsa_key_file: Option<PathBuf>,
}

/// The `[password]` table: RSA-AES security with one password of the
/// operator's, which no system account has to know about.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Password {
    /// The password's Argon2 hash as the PHC string `wlshare hash-password`
    /// prints. The password itself is never in this file.
    pub hash: String,
    /// The server's RSA key, as under `[pam]`.
    pub rsa_key_file: Option<PathBuf>,
}

fn default_pam_service() -> String {
    "wlshare".to_owned()
}

fn default_listen() -> SocketAddr {
    "127.0.0.1:5900".parse().unwrap()
}

fn default_true() -> bool {
    true
}

fn default_max_fps() -> u32 {
    60
}

fn default_handshake_timeout_secs() -> u64 {
    120
}

fn default_name() -> String {
    "wlshare".to_owned()
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate().with_context(|| format!("in {}", path.display()))?;
        Ok(config)
    }

    /// What the types cannot say: a rate that is a rate, a timeout a handshake
    /// can finish inside, and one answer to the question of who may connect.
    fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.max_fps > 0, "max_fps must be at least 1");
        anyhow::ensure!(self.handshake_timeout_secs > 0, "handshake_timeout_secs must be at least 1");
        anyhow::ensure!(
            !(self.pam.is_some() && self.password.is_some()),
            "[pam] and [password] are two answers to the same question; keep one"
        );
        Ok(())
    }

    /// Where the RSA-AES key lives when either table is set: that table's path,
    /// or `rsa_key.pem` beside the configuration file at `config_path`. `None`
    /// when neither is, which is the unauthenticated server.
    pub fn rsa_key_file(&self, config_path: &Path) -> Option<PathBuf> {
        let configured = match (&self.pam, &self.password) {
            (Some(pam), _) => &pam.rsa_key_file,
            (None, Some(password)) => &password.rsa_key_file,
            (None, None) => return None,
        };
        Some(configured.clone().unwrap_or_else(|| config_path.with_file_name("rsa_key.pem")))
    }
}

/// The default configuration path: `$XDG_CONFIG_HOME/wlshare/config.toml`.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("wlshare").join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_an_empty_file() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.listen, default_listen());
        assert!(c.resize);
        assert_eq!(c.max_fps, 60);
        assert_eq!(c.handshake_timeout_secs, 120);
        assert!(!c.audio);
        assert!(!c.camera);
        assert!(!c.microphone);
        assert_eq!(c.name, "wlshare");
        assert!(c.xkb.layout.is_empty());
        assert!(c.state_socket.is_none());
    }

    /// The hooks are gone: the daemon says where the desktop stands on its
    /// state socket and runs nothing, and `wlshare watch` is what runs the
    /// commands. A file that still has the table is refused as any unknown key
    /// is.
    #[test]
    fn the_state_socket_is_a_path_and_the_hooks_table_is_no_longer_one() {
        let c: Config = toml::from_str("state_socket = \"wlshare/state.sock\"").unwrap();
        c.validate().unwrap();
        assert_eq!(c.state_socket, Some(PathBuf::from("wlshare/state.sock")));
        assert!(toml::from_str::<Config>("[hooks]\ntaken = \"true\"").is_err());
    }

    #[test]
    fn a_password_table_takes_the_key_and_rules_out_pam() {
        let c: Config = toml::from_str("[password]\nhash = \"$argon2id$v=19$x\"").unwrap();
        c.validate().unwrap();
        assert_eq!(c.password.as_ref().unwrap().hash, "$argon2id$v=19$x");
        assert_eq!(c.rsa_key_file(Path::new("/etc/x/config.toml")), Some(PathBuf::from("/etc/x/rsa_key.pem")));
        let c: Config = toml::from_str("[password]\nhash = \"h\"\nrsa_key_file = \"/k.pem\"").unwrap();
        assert_eq!(c.rsa_key_file(Path::new("/etc/x/config.toml")), Some(PathBuf::from("/k.pem")));
        // A hash is the whole point of the table, and both tables at once is a
        // question with two answers.
        assert!(toml::from_str::<Config>("[password]\n").is_err());
        let both: Config = toml::from_str("[pam]\n[password]\nhash = \"h\"").unwrap();
        assert!(both.validate().is_err());
    }

    #[test]
    fn pam_defaults_its_service_and_key_beside_the_config() {
        let c: Config = toml::from_str("[pam]\n").unwrap();
        let pam = c.pam.as_ref().unwrap();
        assert_eq!(pam.service, "wlshare");
        assert_eq!(c.rsa_key_file(Path::new("/etc/x/config.toml")), Some(PathBuf::from("/etc/x/rsa_key.pem")));
        let c: Config = toml::from_str("[pam]\nservice = \"vnc\"\nrsa_key_file = \"/k.pem\"").unwrap();
        assert_eq!(c.pam.as_ref().unwrap().service, "vnc");
        assert_eq!(c.rsa_key_file(Path::new("/etc/x/config.toml")), Some(PathBuf::from("/k.pem")));
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.rsa_key_file(Path::new("/etc/x/config.toml")), None);
    }

    #[test]
    fn the_handshake_timeout_is_configurable_but_never_zero() {
        let c: Config = toml::from_str("handshake_timeout_secs = 30").unwrap();
        c.validate().unwrap();
        assert_eq!(c.handshake_timeout_secs, 30);
        let zero: Config = toml::from_str("handshake_timeout_secs = 0").unwrap();
        assert!(zero.validate().is_err());
    }

    /// The VP9 keys are gone: the quality is the ceiling the client's list
    /// names, and its one client always names one; the floor is the walk's
    /// own, and the settle sharpens a quiet desktop at the ceiling, which is
    /// what the floor was for. A file that still writes either is refused as
    /// any unknown key is.
    #[test]
    fn the_vp9_keys_are_no_longer_keys() {
        for key in ["vp9_quality = 80", "vp9_quality_min = 45"] {
            assert!(toml::from_str::<Config>(key).is_err(), "{key}");
        }
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<Config>("port = 5900").is_err());
    }
}
