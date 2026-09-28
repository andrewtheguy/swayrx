//! RealVNC's RSA-AES security types (RFB 5 and 129), both ends: an
//! authenticated, encrypted RFB session over an ordinary 3.8 wire, and the one
//! standard way a client can tell this server *who* is connecting.
//! [`authenticate`] is the server's half; the client's, `begin`, is the one
//! the tests run it against.
//!
//! The types are RealVNC's, documented in the community `rfbproto` and spoken
//! on the open side by TigerVNC, neatvnc and the remotex gateway, whose client
//! this server was written against. RSA-AES carries either a username and a
//! password, so they can be checked against the system's accounts, or a
//! password alone ([`Subtype`]), and every byte after the key exchange is
//! inside AES-EAX — which classic VncAuth, deliberately not spoken here, never
//! did: it proves knowledge of a machine's secret and encrypts nothing.
//!
//! ## The exchange
//!
//! ```text
//! server → client   u32 bits || modulus || exponent        the server's RSA key
//! client → server   u32 bits || modulus || exponent        the client's, fresh per session
//! server → client   u16 len  || RSA-PKCS1v15(client key, server random)
//! client → server   u16 len  || RSA-PKCS1v15(server key, client random)
//! ```
//!
//! The server speaks first at every step, as the protocol has it and as
//! TigerVNC's server does, so a client that waits for each server message
//! before answering never waits on a server doing the same. Both randoms known,
//! each direction gets its own AES key:
//!
//! ```text
//! client → server   H(server random || client random)[..key]
//! server → client   H(client random || server random)[..key]
//! ```
//!
//! `H` is SHA-1 for the 128-bit type and SHA-256 for the 256-bit one, the random
//! is 16 or 32 bytes to match, and from here every byte in both directions is a
//! [frame](Sealer::frame). Inside the frames: the server's hash of the two public
//! keys, the client's hash of them the other way round (which binds the session
//! to the keys that were actually sent — a middle-man's substitution shows up
//! here), a one-byte subtype saying which credentials the server wants, the
//! credentials, then RFB's own SecurityResult and everything after it.
//!
//! ## The client random as an oracle
//!
//! RSA-PKCS#1 v1.5 is the padding the protocol fixes, and a server that says
//! whether a ciphertext unpadded is the oracle Bleichenbacher's attack needs —
//! against a key that is the same for every connection. So the decryption is
//! blinded, and it never fails, in what it returns or in how it gets there: the
//! raw exponentiation is followed by this module's own unpadding, which checks
//! the fixed shape `00 02 PS 00 random` with the random's known length in
//! constant time and selects, byte by byte and without a branch, between the
//! bytes found there and a substitute derived from a per-process secret and the
//! ciphertext — the substitute computed for every ciphertext, valid or not. The
//! exchange goes on to the hash step exactly as it would have, and the client's
//! hash frame then fails its tag, which is all a wrong ciphertext ever looks like
//! from outside, and is what an honest client with a wrong key would have got
//! too. The `rsa` crate's own PKCS#1 v1.5 decryption is not used: it returns an
//! error on bad padding, and an error is a branch.
//!
//! ## What the server's key is worth
//!
//! The server's key is the one thing a client can pin. It is generated once and
//! kept in a file, so its fingerprint — RealVNC's eight dashed SHA-1 bytes,
//! which this module logs at startup and the remotex gateway logs on every
//! connection — is stable and an operator can compare the two. Against an
//! active middle-man the pinning is the client's to do; against a passive one
//! the session is encrypted.
//!
//! ## The frame
//!
//! ```text
//! wire   u16 len || byte[len] AES-EAX ciphertext || byte[16] tag
//! nonce  a 128-bit little-endian counter, from zero, one per frame per direction
//! aad    the two length bytes
//! ```
//!
//! A frame is a transport unit and not a message: a framebuffer update spans as
//! many frames as it needs, so the read side is an [`AsyncRead`] yielding the
//! concatenation of frame bodies and the write side takes a whole message and
//! cuts it. The counter is the nonce, so a frame is decrypted exactly once and
//! in order; a reader that has failed is finished.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use aes::{Aes128, Aes256};
use eax::Eax;
use eax::aead::{Aead as _, KeyInit as _, Payload};
use rand::Rng as _;
use rsa::hazmat::rsa_decrypt;
use rsa::pkcs8::{DecodePrivateKey as _, EncodePrivateKey as _};
use rsa::traits::PublicKeyParts as _;
use rsa::{BoxedUint, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha1::{Digest as _, Sha1};
use sha2::Sha256;
use subtle::{ConditionallySelectable as _, ConstantTimeEq as _};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};

/// RFB security type `RA2`: RSA key exchange, AES-128-EAX, SHA-1.
pub const SECURITY_RSA_AES_128: u8 = 5;
/// RFB security type `RA2_256`: the same with AES-256-EAX and SHA-256.
pub const SECURITY_RSA_AES_256: u8 = 129;

/// Bits in the key generated for a server without one. TigerVNC's and
/// wayvnc's size; the exchange lets the two ends' sizes differ.
pub const SERVER_KEY_BITS: usize = 2048;
/// Bounds on the other end's key, TigerVNC's. Below the lower one the random it
/// carries is not protected; above the upper one a bogus length turns into a
/// very large allocation and a very slow exponentiation.
const MIN_PEER_KEY_BITS: u32 = 1024;
const MAX_PEER_KEY_BITS: u32 = 8192;
/// The most plaintext put in one outgoing frame: TigerVNC's `MaxMessageSize`,
/// and the receive buffer of every implementation this has been tried against.
const MAX_FRAME_BODY: usize = 8192;
/// Length prefix and tag around a frame's ciphertext.
const HEADER: usize = 2;
const TAG: usize = 16;

/// Which credentials the server asks for once the channel is up, sent as the
/// one subtype byte. What comes back has the same shape either way — a
/// length-prefixed username then a length-prefixed password, TigerVNC's client
/// sending an empty username for [`Subtype::Password`] — so only the byte the
/// server writes differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subtype {
    /// Subtype 1: a username and a password, for a login that names an account.
    UserPass,
    /// Subtype 2: a password alone, for a server whose secret names nobody.
    Password,
}

impl Subtype {
    fn byte(self) -> u8 {
        match self {
            Self::UserPass => 1,
            Self::Password => 2,
        }
    }

    #[cfg(test)]
    fn of(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::UserPass),
            2 => Some(Self::Password),
            _ => None,
        }
    }
}

/// Which of the two types was chosen, deciding the hash, the key length and
/// the random's size together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strength {
    Aes128,
    Aes256,
}

impl Strength {
    /// The strength behind an RFB security type, if it is one of the two.
    pub fn of(security_type: u8) -> Option<Self> {
        match security_type {
            SECURITY_RSA_AES_128 => Some(Self::Aes128),
            SECURITY_RSA_AES_256 => Some(Self::Aes256),
            _ => None,
        }
    }

    /// Bytes of random each side contributes, which is also the AES key length.
    fn random_len(self) -> usize {
        match self {
            Self::Aes128 => 16,
            Self::Aes256 => 32,
        }
    }

    /// The type's hash over the given pieces, in order. SHA-1 yields 20 bytes
    /// of which the 128-bit key takes the first 16; SHA-256 yields exactly the
    /// 256-bit key.
    fn hash(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            Self::Aes128 => {
                let mut h = Sha1::new();
                parts.iter().for_each(|p| h.update(p));
                h.finalize().to_vec()
            }
            Self::Aes256 => {
                let mut h = Sha256::new();
                parts.iter().for_each(|p| h.update(p));
                h.finalize().to_vec()
            }
        }
    }

    fn cipher(self, key: &[u8]) -> Cipher {
        match self {
            Self::Aes128 => Cipher::Aes128(Eax::<Aes128>::new_from_slice(key).expect("a 16-byte key")),
            Self::Aes256 => Cipher::Aes256(Eax::<Aes256>::new_from_slice(key).expect("a 32-byte key")),
        }
    }
}

/// An RSA public key as it travels: `u32 bits || modulus || exponent`, the
/// two numbers big-endian and each padded to the modulus' byte length. Kept in
/// wire form because both the key hashes and the fingerprint are over exactly
/// these bytes.
#[derive(Clone, PartialEq, Eq)]
struct WireKey(Vec<u8>);

impl WireKey {
    fn encode(bits: u32, modulus: &[u8], exponent: &[u8]) -> Self {
        let size = (bits as usize).div_ceil(8);
        let mut wire = Vec::with_capacity(4 + 2 * size);
        wire.extend_from_slice(&bits.to_be_bytes());
        wire.extend_from_slice(&left_pad(modulus, size));
        wire.extend_from_slice(&left_pad(exponent, size));
        Self(wire)
    }

    fn of_public(key: &RsaPublicKey) -> Self {
        Self::encode(key.n().bits(), &key.n_bytes(), &key.e_bytes())
    }

    /// The key's byte length, which is what its ciphertexts are long.
    fn size(&self) -> usize {
        (self.0.len() - 4) / 2
    }

    fn modulus(&self) -> &[u8] {
        &self.0[4..4 + self.size()]
    }

    fn exponent(&self) -> &[u8] {
        &self.0[4 + self.size()..]
    }

    /// RealVNC's display of a key: the first eight bytes of its SHA-1, dashed.
    fn fingerprint(&self) -> String {
        let digest = Sha1::digest(&self.0);
        digest[..8].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join("-")
    }

    fn public_key(&self) -> Result<RsaPublicKey, Error> {
        // Public inputs, so the variable-time decoder is the right one.
        RsaPublicKey::new(
            BoxedUint::from_be_slice_vartime(self.modulus()),
            BoxedUint::from_be_slice_vartime(self.exponent()),
        )
        .map_err(|e| Error::Protocol(format!("the other end's RSA key is invalid: {e}")))
    }
}

fn left_pad(bytes: &[u8], size: usize) -> Vec<u8> {
    let bytes = bytes.iter().copied().skip_while(|&b| b == 0).collect::<Vec<_>>();
    assert!(bytes.len() <= size, "a {}-byte number in a {size}-byte field", bytes.len());
    let mut out = vec![0u8; size - bytes.len()];
    out.extend_from_slice(&bytes);
    out
}

/// What can go wrong between the security type being chosen and the
/// credentials arriving.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    /// The other end's bytes do not make an RSA-AES exchange.
    #[error("{0}")]
    Protocol(String),
    /// The other end's key hash does not cover the keys that were exchanged.
    #[error("the other end's RSA-AES key hash does not match the keys exchanged — the connection was tampered with")]
    Tampered,
}

/// The server's long-lived RSA key: the private half for the exchange and the
/// public half in wire form, which is what gets hashed and fingerprinted.
pub struct ServerKey {
    private: RsaPrivateKey,
    wire: WireKey,
    /// What stands in for a client random that did not decrypt, mixed with the
    /// ciphertext so the substitute is consistent for a repeated ciphertext and
    /// unpredictable to whoever sent it. Fresh per process.
    reject_secret: [u8; 32],
}

impl ServerKey {
    /// A fresh [`SERVER_KEY_BITS`]-bit key. Real CPU time — call it off any
    /// latency-sensitive path.
    pub fn generate() -> Result<Self, rsa::Error> {
        Ok(Self::new(RsaPrivateKey::new(&mut rand::rng(), SERVER_KEY_BITS)?))
    }

    fn new(private: RsaPrivateKey) -> Self {
        let wire = WireKey::of_public(private.as_public_key());
        let mut reject_secret = [0u8; 32];
        rand::rng().fill_bytes(&mut reject_secret);
        Self { private, wire, reject_secret }
    }

    /// The client random out of its ciphertext — or, for a ciphertext that does
    /// not decrypt to a random of the right length, a substitute nothing outside
    /// can tell from one. Blinded, never an error, and without a branch on what
    /// the padding looked like; see the module docs. `sealed` is exactly the
    /// key's size, which the caller has checked against the public length field.
    fn open_client_random(&self, sealed: &[u8], strength: Strength) -> Vec<u8> {
        let want = strength.random_len();
        let size = self.wire.size();
        debug_assert_eq!(sealed.len(), size);
        // Computed for every ciphertext, so that the work done is the same
        // whether or not it is used.
        let substitute = Sha256::new().chain_update(self.reject_secret).chain_update(sealed).finalize();
        let substitute = &substitute[..want];
        // The two refusals below are about the ciphertext as an integer — too
        // long for the modulus, or not below it — which is public arithmetic on
        // public bytes, not the padding.
        let n = self.private.n();
        let Some(em) = BoxedUint::from_be_slice(sealed, n.bits_precision())
            .ok()
            .and_then(|c| rsa_decrypt(Some(&mut rand::rng()), &self.private, &c).ok())
        else {
            return substitute.to_vec();
        };
        let em = em.to_be_bytes();
        // The integer is below the modulus, so the bytes beyond the key's size
        // are leading zeros.
        let em = &em[em.len() - size..];
        // EM = 00 || 02 || PS || 00 || M, with M the `want` bytes of a random
        // and PS non-zero bytes filling the rest, so the separator has one
        // place to be. Every byte is inspected whatever the earlier ones held.
        let separator = size - want - 1;
        let mut valid = em[0].ct_eq(&0) & em[1].ct_eq(&2) & em[separator].ct_eq(&0);
        for byte in &em[2..separator] {
            valid &= !byte.ct_eq(&0);
        }
        (0..want).map(|i| u8::conditional_select(&substitute[i], &em[separator + 1 + i], valid)).collect()
    }

    /// The key as a PKCS#8 PEM document (`BEGIN PRIVATE KEY`), for the file.
    pub fn to_pem(&self) -> Result<String, rsa::pkcs8::Error> {
        self.private.to_pkcs8_pem(rsa::pkcs8::LineEnding::LF).map(|pem| pem.to_string())
    }

    /// A key from the PEM document [`Self::to_pem`] wrote.
    pub fn from_pem(pem: &str) -> Result<Self, rsa::pkcs8::Error> {
        RsaPrivateKey::from_pkcs8_pem(pem).map(Self::new)
    }

    pub fn bits(&self) -> usize {
        self.wire.size() * 8
    }

    /// RealVNC's display of the key, which is what a client shows or logs.
    pub fn fingerprint(&self) -> String {
        self.wire.fingerprint()
    }
}

/// The most bytes either credential field can carry: its length is one byte on
/// the wire, so a password longer than this is one no client can send, whatever
/// the server would have made of it.
pub const MAX_CREDENTIAL_LEN: usize = u8::MAX as usize;

/// What the client sent as its account: `u8 len || username || u8 len ||
/// password`, each UTF-8 as far as it goes and at most [`MAX_CREDENTIAL_LEN`]
/// bytes. The username is what a client answering [`Subtype::Password`] leaves
/// empty, and means nothing there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// Both directions' ciphers after the exchange, each already advanced past the
/// handshake frames it carried. [`Opener`] holds whatever the other end sent
/// beyond them, so wrap it around the same reader.
pub struct Session {
    pub sealer: Sealer,
    pub opener: Opener,
}

/// Run the exchange on a freshly chosen RSA-AES type: key exchange in the
/// clear, then the key hashes, `subtype` and the credentials inside frames.
///
/// Returns once the credentials are in hand; checking them and answering with
/// SecurityResult is the caller's, through [`Session::sealer`] like everything
/// after.
pub async fn authenticate<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    strength: Strength,
    key: &ServerKey,
    subtype: Subtype,
) -> Result<(Credentials, Session), Error> {
    writer.write_all(&key.wire.0).await?;

    let client_wire = read_peer_key(reader).await?;
    let client_key = client_wire.public_key()?;

    let mut server_random = vec![0u8; strength.random_len()];
    rand::rng().fill_bytes(&mut server_random);
    let sealed_random = client_key
        .encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &server_random)
        .map_err(|e| Error::Protocol(format!("encrypting the server random to the client's key failed: {e}")))?;
    if sealed_random.len() != client_wire.size() {
        return Err(Error::Protocol(format!(
            "the server random encrypted to {} bytes under a {}-byte client key",
            sealed_random.len(),
            client_wire.size()
        )));
    }
    let mut out = (sealed_random.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&sealed_random);
    writer.write_all(&out).await?;

    let client_random = read_client_random(reader, key, strength).await?;

    let send_key = strength.hash(&[&client_random, &server_random]);
    let recv_key = strength.hash(&[&server_random, &client_random]);
    let key_len = strength.random_len();
    let mut sealer = Sealer::new(strength.cipher(&send_key[..key_len]));
    let server_hash = strength.hash(&[&key.wire.0, &client_wire.0]);
    writer.write_all(&sealer.frame(&server_hash)).await?;

    let mut frames = FrameReader::new(reader, Opener::new(strength.cipher(&recv_key[..key_len])));
    let mut client_hash = vec![0u8; server_hash.len()];
    frames.read_exact(&mut client_hash).await?;
    if client_hash != strength.hash(&[&client_wire.0, &key.wire.0]) {
        return Err(Error::Tampered);
    }
    writer.write_all(&sealer.frame(&[subtype.byte()])).await?;

    let username = read_field(&mut frames).await?;
    let password = read_field(&mut frames).await?;
    let (_, opener) = frames.into_parts();
    Ok((Credentials { username, password }, Session { sealer, opener }))
}

/// The key a client makes for one session and forgets with it.
#[cfg(test)]
struct ClientKey {
    private: RsaPrivateKey,
    wire: WireKey,
}

#[cfg(test)]
impl ClientKey {
    fn of_bits(bits: usize) -> Result<Self, rsa::Error> {
        let private = RsaPrivateKey::new(&mut rand::rng(), bits)?;
        let wire = WireKey::of_public(private.as_public_key());
        Ok(Self { private, wire })
    }
}

/// A client's exchange, run as far as the server's question: the channel is up
/// and the keys are proven, and what is left is to decide whether
/// [`Self::fingerprint`] is the server meant and to answer [`Self::subtype`]
/// with [`Self::login`]. The decision comes first — the credentials go to
/// whoever holds that key.
#[cfg(test)]
struct Exchange {
    session: Session,
    fingerprint: String,
    subtype: Subtype,
}

/// Run the client's half of the exchange on a freshly chosen RSA-AES type, up
/// to the subtype byte.
///
/// The key is taken, not borrowed: refusing a server random that does not
/// decrypt is safe only because the key dies with the connection that refused
/// it. Answer the same key twice and the refusals become a padding oracle, so
/// a second exchange has to be a second key.
#[cfg(test)]
async fn begin<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    strength: Strength,
    key: ClientKey,
) -> Result<Exchange, Error> {
    let server_wire = read_peer_key(reader).await?;
    let server_key = server_wire.public_key()?;
    writer.write_all(&key.wire.0).await?;

    let len = usize::from(reader.read_u16().await?);
    if len != key.wire.size() {
        return Err(Error::Protocol(format!("the server random is {len} bytes, not the {} of the client key", key.wire.size())));
    }
    let mut sealed = vec![0u8; len];
    reader.read_exact(&mut sealed).await?;
    // The key is this session's alone, so a refusal here tells nobody anything
    // about a key worth having.
    let server_random = key
        .private
        .decrypt(Pkcs1v15Encrypt, &sealed)
        .map_err(|e| Error::Protocol(format!("the server random does not decrypt under the client's key: {e}")))?;
    if server_random.len() != strength.random_len() {
        return Err(Error::Protocol(format!("the server random is {} bytes, not {}", server_random.len(), strength.random_len())));
    }

    let mut client_random = vec![0u8; strength.random_len()];
    rand::rng().fill_bytes(&mut client_random);
    let sealed_random = server_key
        .encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &client_random)
        .map_err(|e| Error::Protocol(format!("encrypting the client random to the server's key failed: {e}")))?;
    let mut out = (sealed_random.len() as u16).to_be_bytes().to_vec();
    out.extend_from_slice(&sealed_random);
    writer.write_all(&out).await?;

    let send_key = strength.hash(&[&server_random, &client_random]);
    let recv_key = strength.hash(&[&client_random, &server_random]);
    let key_len = strength.random_len();
    let mut sealer = Sealer::new(strength.cipher(&send_key[..key_len]));
    let mut frames = FrameReader::new(reader, Opener::new(strength.cipher(&recv_key[..key_len])));

    let mut server_hash = strength.hash(&[&server_wire.0, &key.wire.0]);
    let expected = server_hash.clone();
    frames.read_exact(&mut server_hash).await?;
    if server_hash != expected {
        return Err(Error::Tampered);
    }
    writer.write_all(&sealer.frame(&strength.hash(&[&key.wire.0, &server_wire.0]))).await?;

    let subtype = frames.read_u8().await?;
    let subtype = Subtype::of(subtype).ok_or_else(|| Error::Protocol(format!("the server asks for credentials of subtype {subtype}, which is not one")))?;
    let (_, opener) = frames.into_parts();
    Ok(Exchange { session: Session { sealer, opener }, fingerprint: server_wire.fingerprint(), subtype })
}

#[cfg(test)]
impl Exchange {
    /// RealVNC's display of the server's key, to compare with the one the
    /// server logged at startup or the one seen last time.
    fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Which credentials the server wants.
    fn subtype(&self) -> Subtype {
        self.subtype
    }

    /// Answer the server's question. The username goes out empty to a server
    /// that asked for a password alone. What comes back is the transport
    /// SecurityResult and everything after it arrive over.
    async fn login<W: AsyncWrite + Unpin>(mut self, writer: &mut W, credentials: &Credentials) -> Result<Session, Error> {
        let username = match self.subtype {
            Subtype::UserPass => credentials.username.as_str(),
            Subtype::Password => "",
        };
        let mut fields = Vec::with_capacity(2 + username.len() + credentials.password.len());
        for (name, field) in [("username", username), ("password", credentials.password.as_str())] {
            let len = u8::try_from(field.len())
                .map_err(|_| Error::Protocol(format!("a {name} of {} bytes is over the {MAX_CREDENTIAL_LEN} RSA-AES can carry", field.len())))?;
            fields.push(len);
            fields.extend_from_slice(field.as_bytes());
        }
        writer.write_all(&self.session.sealer.frame(&fields)).await?;
        Ok(self.session)
    }
}

/// The other end's public key, off the wire.
async fn read_peer_key<R: AsyncRead + Unpin>(reader: &mut R) -> Result<WireKey, Error> {
    let bits = reader.read_u32().await?;
    if !(MIN_PEER_KEY_BITS..=MAX_PEER_KEY_BITS).contains(&bits) {
        return Err(Error::Protocol(format!(
            "the other end's RSA key is {bits} bits; {MIN_PEER_KEY_BITS} to {MAX_PEER_KEY_BITS} are accepted"
        )));
    }
    let size = (bits as usize).div_ceil(8);
    let mut wire = vec![0u8; 4 + 2 * size];
    wire[..4].copy_from_slice(&bits.to_be_bytes());
    reader.read_exact(&mut wire[4..]).await?;
    Ok(WireKey(wire))
}

async fn read_client_random<R: AsyncRead + Unpin>(reader: &mut R, key: &ServerKey, strength: Strength) -> Result<Vec<u8>, Error> {
    let len = usize::from(reader.read_u16().await?);
    if len != key.wire.size() {
        return Err(Error::Protocol(format!(
            "the client random is {len} bytes, not the {} of the server key",
            key.wire.size()
        )));
    }
    let mut sealed = vec![0u8; len];
    reader.read_exact(&mut sealed).await?;
    Ok(key.open_client_random(&sealed, strength))
}

/// One credential: a byte of length, then that many bytes of text.
async fn read_field<R: AsyncRead + Unpin>(frames: &mut R) -> Result<String, Error> {
    let len = usize::from(frames.read_u8().await?);
    let mut bytes = vec![0u8; len];
    frames.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).map_err(|_| Error::Protocol("a credential is not UTF-8".to_owned()))
}

/// One direction's AES-EAX, at whichever width the type chose.
enum Cipher {
    Aes128(Eax<Aes128>),
    Aes256(Eax<Aes256>),
}

impl Cipher {
    fn seal(&self, nonce: &[u8; 16], aad: &[u8], msg: &[u8]) -> Vec<u8> {
        let payload = Payload { msg, aad };
        let sealed = match self {
            Self::Aes128(c) => c.encrypt(nonce.into(), payload),
            Self::Aes256(c) => c.encrypt(nonce.into(), payload),
        };
        sealed.expect("AES-EAX encryption of an in-memory buffer cannot fail")
    }

    fn open(&self, nonce: &[u8; 16], aad: &[u8], sealed: &[u8]) -> Option<Vec<u8>> {
        let payload = Payload { msg: sealed, aad };
        match self {
            Self::Aes128(c) => c.decrypt(nonce.into(), payload),
            Self::Aes256(c) => c.decrypt(nonce.into(), payload),
        }
        .ok()
    }
}

/// The 128-bit little-endian frame counter. Never resets; a frame is numbered
/// once.
fn bump(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut() {
        *byte = byte.wrapping_add(1);
        if *byte != 0 {
            break;
        }
    }
}

/// The sending direction: takes a message, returns the frames that carry it.
pub struct Sealer {
    cipher: Cipher,
    counter: [u8; 16],
}

impl Sealer {
    fn new(cipher: Cipher) -> Self {
        Self { cipher, counter: [0; 16] }
    }

    /// Frame a message, cutting it at 8192 bytes (`MAX_FRAME_BODY`). An empty message is
    /// no frame at all — nothing has ever needed to send one, and a reader that
    /// receives one has only a counter to advance for it.
    pub fn frame(&mut self, msg: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(msg.len() + (msg.len() / MAX_FRAME_BODY + 1) * (HEADER + TAG));
        for body in msg.chunks(MAX_FRAME_BODY) {
            let header = (body.len() as u16).to_be_bytes();
            let sealed = self.cipher.seal(&self.counter, &header, body);
            bump(&mut self.counter);
            out.extend_from_slice(&header);
            out.extend_from_slice(&sealed);
        }
        out
    }
}

enum Phase {
    /// The two length bytes.
    Len,
    /// `len` bytes of ciphertext and the tag behind them.
    Sealed { len: usize },
}

/// The receiving direction: the cipher, the counter, and whatever has
/// been read but not yet handed on. Separate from the reader it feeds so the
/// handshake can run it over a borrowed socket and the session over the owned
/// one, without a byte falling between — a client sends ClientInit on the heels
/// of its credentials, and it may already be here.
pub struct Opener {
    cipher: Cipher,
    counter: [u8; 16],
    /// The frame in flight: the header while it is being read, then the sealed
    /// body and tag.
    staging: Vec<u8>,
    filled: usize,
    phase: Phase,
    /// Opened bytes not yet handed upward.
    body: Vec<u8>,
    body_pos: usize,
    /// A frame failed, which is terminal: the counter that would decrypt the
    /// next one has moved on from the one that was never accepted.
    failed: bool,
}

impl Opener {
    fn new(cipher: Cipher) -> Self {
        Self {
            cipher,
            counter: [0; 16],
            staging: Vec::new(),
            filled: 0,
            phase: Phase::Len,
            body: Vec::new(),
            body_pos: 0,
            failed: false,
        }
    }

    /// Open the complete frame in `staging`, leaving its body ready to hand on.
    fn accept(&mut self, len: usize) -> io::Result<()> {
        let (header, sealed) = self.staging.split_at(HEADER);
        let body = self
            .cipher
            .open(&self.counter, header, &sealed[..len + TAG])
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "an RSA-AES frame failed its authentication tag"))?;
        bump(&mut self.counter);
        self.body = body;
        self.body_pos = 0;
        Ok(())
    }

    fn poll_read<R: AsyncRead + Unpin>(&mut self, inner: &mut R, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if self.failed {
            return Poll::Ready(Err(io::Error::new(io::ErrorKind::InvalidData, "the RSA-AES transport already failed")));
        }
        loop {
            if self.body_pos < self.body.len() {
                let n = buf.remaining().min(self.body.len() - self.body_pos);
                buf.put_slice(&self.body[self.body_pos..self.body_pos + n]);
                self.body_pos += n;
                return Poll::Ready(Ok(()));
            }
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            match self.phase {
                Phase::Len => {
                    self.staging.resize(HEADER, 0);
                    let n = ready!(poll_fill(inner, cx, &mut self.staging[self.filled..]))?;
                    if n == 0 {
                        // Between frames a hang-up is an ordinary close.
                        return if self.filled == 0 { Poll::Ready(Ok(())) } else { Poll::Ready(Err(truncated())) };
                    }
                    self.filled += n;
                    if self.filled == HEADER {
                        let len = usize::from(u16::from_be_bytes([self.staging[0], self.staging[1]]));
                        self.staging.resize(HEADER + len + TAG, 0);
                        self.phase = Phase::Sealed { len };
                    }
                }
                Phase::Sealed { len } => {
                    let end = HEADER + len + TAG;
                    let n = ready!(poll_fill(inner, cx, &mut self.staging[self.filled..end]))?;
                    if n == 0 {
                        return Poll::Ready(Err(truncated()));
                    }
                    self.filled += n;
                    if self.filled == end {
                        if let Err(e) = self.accept(len) {
                            self.failed = true;
                            return Poll::Ready(Err(e));
                        }
                        self.filled = 0;
                        self.phase = Phase::Len;
                    }
                }
            }
        }
    }
}

fn poll_fill<R: AsyncRead + Unpin>(inner: &mut R, cx: &mut Context<'_>, dst: &mut [u8]) -> Poll<io::Result<usize>> {
    let mut buf = ReadBuf::new(dst);
    ready!(Pin::new(inner).poll_read(cx, &mut buf))?;
    Poll::Ready(Ok(buf.filled().len()))
}

fn truncated() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, "the connection ended inside an RSA-AES frame")
}

/// An [`Opener`] over a reader: the plaintext stream, as the RFB layer reads it.
pub struct FrameReader<R> {
    inner: R,
    opener: Opener,
}

impl<R> FrameReader<R> {
    pub fn new(inner: R, opener: Opener) -> Self {
        Self { inner, opener }
    }

    /// Take the transport apart again, with everything read so far still in
    /// the [`Opener`].
    pub fn into_parts(self) -> (R, Opener) {
        (self.inner, self.opener)
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FrameReader<R> {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        me.opener.poll_read(&mut me.inner, cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(strength: Strength) -> (Sealer, Opener) {
        let key = vec![0x42u8; strength.random_len()];
        (Sealer::new(strength.cipher(&key)), Opener::new(strength.cipher(&key)))
    }

    /// A small key for the tests: generation dominates their run time, and the
    /// exchange does not care about the size.
    fn test_key() -> ServerKey {
        ServerKey::new(RsaPrivateKey::new(&mut rand::rng(), 1024).unwrap())
    }

    #[tokio::test]
    async fn frames_round_trip_across_both_widths_and_split_reads() {
        for strength in [Strength::Aes128, Strength::Aes256] {
            let (mut sealer, opener) = pair(strength);
            let big: Vec<u8> = (0..20_000u32).map(|i| (i * 7) as u8).collect();
            let mut wire = sealer.frame(b"hello");
            wire.extend(sealer.frame(&big));
            wire.extend(sealer.frame(b"!"));
            // Three frames for the big message and one each for the others.
            assert_eq!(wire.len(), 5 + big.len() + 1 + 5 * (HEADER + TAG));

            // Delivered a byte at a time, read in odd sizes: still one stream.
            let (mut tx, rx) = tokio::io::duplex(1);
            let feeder = tokio::spawn(async move {
                for b in wire {
                    tx.write_all(&[b]).await.unwrap();
                }
            });
            let mut reader = FrameReader::new(rx, opener);
            let mut got = Vec::new();
            let mut chunk = [0u8; 777];
            loop {
                let n = reader.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&chunk[..n]);
            }
            feeder.await.unwrap();
            let mut want = b"hello".to_vec();
            want.extend(&big);
            want.push(b'!');
            assert_eq!(got, want);
        }
    }

    #[tokio::test]
    async fn a_tampered_frame_closes_the_transport() {
        let (mut sealer, opener) = pair(Strength::Aes128);
        let mut wire = sealer.frame(b"first");
        wire.extend(sealer.frame(b"second"));
        wire[HEADER + 1] ^= 1;
        let mut reader = FrameReader::new(std::io::Cursor::new(wire), opener);
        let mut buf = [0u8; 16];
        let err = reader.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        let err = reader.read(&mut buf).await.unwrap_err();
        assert!(err.to_string().contains("already failed"), "{err}");
    }

    #[tokio::test]
    async fn a_frame_is_numbered_once() {
        let (mut sealer, opener) = pair(Strength::Aes256);
        let first = sealer.frame(b"once");
        let mut wire = first.clone();
        wire.extend(&first);
        let mut reader = FrameReader::new(std::io::Cursor::new(wire), opener);
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"once");
        assert!(reader.read(&mut buf).await.is_err());
    }

    #[test]
    fn the_counter_is_little_endian_with_carry() {
        let mut c = [0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        bump(&mut c);
        assert_eq!(c, [0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn wire_key_pads_and_fingerprints_like_realvnc() {
        let key = WireKey::encode(1024, &[0, 0, 0x01, 0x02], &[0x01, 0x00, 0x01]);
        assert_eq!(key.0.len(), 4 + 256);
        assert_eq!(&key.0[..4], &[0, 0, 4, 0]);
        assert_eq!(key.size(), 128);
        assert_eq!(&key.modulus()[126..], &[0x01, 0x02]);
        assert_eq!(&key.exponent()[125..], &[0x01, 0x00, 0x01]);
        let fp = key.fingerprint();
        assert_eq!(fp.len(), 8 * 2 + 7, "{fp}");
        assert_eq!(fp.matches('-').count(), 7, "{fp}");
    }

    #[test]
    fn a_key_survives_the_pem_round_trip_with_its_fingerprint() {
        let key = test_key();
        let pem = key.to_pem().unwrap();
        assert!(pem.starts_with("-----BEGIN PRIVATE KEY-----\n"), "{pem}");
        let back = ServerKey::from_pem(&pem).unwrap();
        assert_eq!(back.fingerprint(), key.fingerprint());
        assert_eq!(back.bits(), 1024);
        assert!(ServerKey::from_pem("-----BEGIN PRIVATE KEY-----\nbm9wZQ==\n-----END PRIVATE KEY-----\n").is_err());
    }

    /// The client side of the exchange, written from the specification rather
    /// than by calling the server's helpers, so a mistake in one cannot be
    /// agreed with by the other. Strictly lockstep: it waits for each server
    /// message before answering, which is what proves the server speaks first
    /// at every step. Sends the credentials, then reads the SecurityResult and a
    /// message behind it and echoes a message back.
    async fn scripted_client<S: AsyncRead + AsyncWrite + Unpin>(
        mut sock: S,
        strength: Strength,
        subtype: Subtype,
        username: &str,
        password: &str,
        misbehave: Misbehave,
    ) -> Result<(u32, Vec<u8>), io::Error> {
        let client_key = RsaPrivateKey::new(&mut rand::rng(), 1024).unwrap();
        let bits = client_key.n().bits();
        let size = (bits as usize).div_ceil(8);
        let mut client_wire = bits.to_be_bytes().to_vec();
        client_wire.extend(left_pad(&client_key.n_bytes(), size));
        client_wire.extend(left_pad(&client_key.e_bytes(), size));

        let server_bits = sock.read_u32().await?;
        let server_size = (server_bits as usize).div_ceil(8);
        let mut server_wire = server_bits.to_be_bytes().to_vec();
        server_wire.resize(4 + 2 * server_size, 0);
        sock.read_exact(&mut server_wire[4..]).await?;
        let server_key = RsaPublicKey::new(
            BoxedUint::from_be_slice_vartime(&server_wire[4..4 + server_size]),
            BoxedUint::from_be_slice_vartime(&server_wire[4 + server_size..]),
        )
        .unwrap();
        sock.write_all(&client_wire).await?;

        let len = usize::from(sock.read_u16().await?);
        assert_eq!(len, size);
        let mut sealed = vec![0u8; len];
        sock.read_exact(&mut sealed).await?;
        let server_random = client_key.decrypt(Pkcs1v15Encrypt, &sealed).unwrap();
        let random_len = strength.random_len();
        assert_eq!(server_random.len(), random_len);

        let mut client_random = vec![0u8; random_len];
        rand::rng().fill_bytes(&mut client_random);
        let mut sealed = server_key.encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &client_random).unwrap();
        if misbehave == Misbehave::GarbageRandom {
            rand::rng().fill_bytes(&mut sealed);
            sealed[0] = 0;
        }
        let mut out = (sealed.len() as u16).to_be_bytes().to_vec();
        out.extend(&sealed);
        sock.write_all(&out).await?;

        // The client's send key is H(server || client); its receive key the reverse.
        let send = strength.hash(&[&server_random, &client_random]);
        let recv = strength.hash(&[&client_random, &server_random]);
        let mut sealer = Sealer::new(strength.cipher(&send[..random_len]));
        let mut frames = FrameReader::new(&mut sock, Opener::new(strength.cipher(&recv[..random_len])));
        let mut server_hash = vec![0u8; strength.hash(&[]).len()];
        frames.read_exact(&mut server_hash).await?;
        assert_eq!(server_hash, strength.hash(&[&server_wire, &client_wire]));

        let (sock, opener) = frames.into_parts();
        let mut client_hash = strength.hash(&[&client_wire, &server_wire]);
        if misbehave == Misbehave::LieInHash {
            client_hash[0] ^= 1;
        }
        sock.write_all(&sealer.frame(&client_hash)).await?;

        let mut frames = FrameReader::new(sock, opener);
        // The number written out again rather than asked of Subtype::byte, so
        // the two tables cannot agree on a wrong one.
        let expected = match subtype {
            Subtype::UserPass => 1u8,
            Subtype::Password => 2u8,
        };
        assert_eq!(frames.read_u8().await?, expected);
        let (sock, opener) = frames.into_parts();
        let mut credentials = vec![username.len() as u8];
        credentials.extend(username.as_bytes());
        credentials.push(password.len() as u8);
        credentials.extend(password.as_bytes());
        sock.write_all(&sealer.frame(&credentials)).await?;

        let mut frames = FrameReader::new(sock, opener);
        let result = frames.read_u32().await?;
        let mut after = [0u8; 5];
        frames.read_exact(&mut after).await?;
        let (sock, _) = frames.into_parts();
        sock.write_all(&sealer.frame(b"echo")).await?;
        Ok((result, after.to_vec()))
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Misbehave {
        No,
        /// A client random that is not a ciphertext of anything.
        GarbageRandom,
        /// A key hash over other keys.
        LieInHash,
    }

    async fn exchange(
        strength: Strength,
        subtype: Subtype,
        username: &str,
        password: &str,
        misbehave: Misbehave,
    ) -> (Result<Credentials, Error>, Result<(u32, Vec<u8>), io::Error>) {
        let key = test_key();
        let (client_sock, server_sock) = tokio::io::duplex(4096);
        let username = username.to_owned();
        let password = password.to_owned();
        let client = tokio::spawn(async move { scripted_client(client_sock, strength, subtype, &username, &password, misbehave).await });

        // The halves live inside the block, so a server that gives up drops them
        // and the client reads a hang-up instead of waiting for a hash forever.
        let server = async {
            let (mut reader, mut writer) = tokio::io::split(server_sock);
            let (credentials, Session { mut sealer, opener }) = authenticate(&mut reader, &mut writer, strength, &key, subtype).await?;
            // SecurityResult and a message, both inside one frame, then an echo back.
            let mut after = 0u32.to_be_bytes().to_vec();
            after.extend_from_slice(b"after");
            writer.write_all(&sealer.frame(&after)).await?;
            let mut frames = FrameReader::new(reader, opener);
            let mut echo = [0u8; 4];
            frames.read_exact(&mut echo).await?;
            assert_eq!(&echo, b"echo");
            Ok(credentials)
        }
        .await;
        (server, client.await.unwrap())
    }

    #[tokio::test]
    async fn the_exchange_authenticates_against_a_client_written_from_the_specification() {
        for strength in [Strength::Aes128, Strength::Aes256] {
            let (server, client) = exchange(strength, Subtype::UserPass, "andrew", "hunter2", Misbehave::No).await;
            let credentials = server.unwrap();
            assert_eq!(credentials.username, "andrew");
            assert_eq!(credentials.password, "hunter2");
            let (result, after) = client.unwrap();
            assert_eq!(result, 0);
            assert_eq!(after, b"after");
        }
    }

    /// Subtype 2: the byte on the wire is 2, and the client answers with an
    /// empty username — which is what TigerVNC's viewer sends — so the password
    /// arrives and names nobody.
    #[tokio::test]
    async fn a_password_only_exchange_asks_for_subtype_2_and_carries_no_username() {
        let (server, client) = exchange(Strength::Aes256, Subtype::Password, "", "hunter2", Misbehave::No).await;
        let credentials = server.unwrap();
        assert_eq!(credentials.username, "");
        assert_eq!(credentials.password, "hunter2");
        assert_eq!(client.unwrap().0, 0);
    }

    #[tokio::test]
    async fn a_client_hash_over_other_keys_ends_the_exchange() {
        let (server, client) = exchange(Strength::Aes256, Subtype::UserPass, "andrew", "hunter2", Misbehave::LieInHash).await;
        assert!(matches!(server, Err(Error::Tampered)), "{server:?}");
        assert!(client.is_err());
    }

    /// A client random that does not decrypt is not reported as such: the
    /// server sends its hash as if it had, and the exchange fails where any
    /// wrong key would — at the client's hash frame, whose tag cannot check.
    #[tokio::test]
    async fn a_ciphertext_that_does_not_decrypt_fails_at_the_hash_like_any_wrong_key() {
        let (server, client) = exchange(Strength::Aes128, Subtype::UserPass, "andrew", "hunter2", Misbehave::GarbageRandom).await;
        // The client's hash frame does not open under the substitute keys — or,
        // this lockstep client having failed on the server's hash first and hung
        // up, never arrives. Either is the hash step; neither names the random.
        match server {
            Err(Error::Io(e)) => assert!(matches!(e.kind(), io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof), "{e}"),
            other => panic!("expected a failure at the hash step, got {other:?}"),
        }
        // The client got as far as the server's hash, so the server did send it.
        let err = client.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }

    /// The shape the unpadding accepts is exactly PKCS#1 v1.5 with a message
    /// of the random's length: a properly sealed random comes back as itself,
    /// and a sealed message one byte short or long — valid PKCS#1, wrong
    /// shape — comes back as the substitute, the same one the substitute
    /// construction gives on its own.
    #[test]
    fn the_unpadding_accepts_a_random_of_the_right_length_and_nothing_else() {
        let key = test_key();
        let public = key.wire.public_key().unwrap();
        for (len, strength) in [(32usize, Strength::Aes256), (16, Strength::Aes128)] {
            let random: Vec<u8> = (0..len as u8).map(|i| i.wrapping_mul(37).wrapping_add(11)).collect();
            let sealed = public.encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &random).unwrap();
            assert_eq!(key.open_client_random(&sealed, strength), random);
            for wrong in [len - 1, len + 1] {
                let message = vec![0xa5u8; wrong];
                let sealed = public.encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &message).unwrap();
                let opened = key.open_client_random(&sealed, strength);
                let substitute = Sha256::new().chain_update(key.reject_secret).chain_update(&sealed).finalize()[..len].to_vec();
                assert_eq!(opened, substitute, "a {wrong}-byte message is not a {len}-byte random");
            }
        }
        // A ciphertext that is not below the modulus is refused the same way.
        let sealed = vec![0xffu8; key.wire.size()];
        let substitute = Sha256::new().chain_update(key.reject_secret).chain_update(&sealed).finalize()[..32].to_vec();
        assert_eq!(key.open_client_random(&sealed, Strength::Aes256), substitute);
    }

    #[test]
    fn a_rejected_random_is_consistent_per_ciphertext_and_differs_between_them() {
        let key = test_key();
        let a = key.open_client_random(&[1u8; 128], Strength::Aes256);
        let b = key.open_client_random(&[1u8; 128], Strength::Aes256);
        let c = key.open_client_random(&[2u8; 128], Strength::Aes256);
        assert_eq!(a.len(), 32);
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Another key's secret gives another substitute for the same bytes.
        assert_ne!(a, test_key().open_client_random(&[1u8; 128], Strength::Aes256));
    }

    #[tokio::test]
    async fn a_client_key_outside_the_bounds_is_refused_after_the_server_key() {
        let key = test_key();
        let mut offer = 512u32.to_be_bytes().to_vec();
        offer.resize(4 + 128, 1);
        let mut sent = Vec::new();
        let err = authenticate(&mut offer.as_slice(), &mut sent, Strength::Aes128, &key, Subtype::UserPass).await.err().expect("a 512-bit key is refused");
        assert!(err.to_string().contains("512 bits"), "{err}");
        // The server's key went out first, as the exchange has it, and nothing else.
        assert_eq!(sent.len(), 4 + 2 * 128);
    }

    /// The server side of the exchange, written from the specification the way
    /// [`scripted_client`] is, for the client's half to be run against. Reads
    /// the credentials, answers SecurityResult OK, and returns what it read.
    async fn scripted_server<S: AsyncRead + AsyncWrite + Unpin>(mut sock: S, strength: Strength, subtype: u8, lie_in_hash: bool) -> io::Result<(String, String)> {
        let server_key = RsaPrivateKey::new(&mut rand::rng(), 1024).unwrap();
        let size = (server_key.n().bits() as usize).div_ceil(8);
        let mut server_wire = server_key.n().bits().to_be_bytes().to_vec();
        server_wire.extend(left_pad(&server_key.n_bytes(), size));
        server_wire.extend(left_pad(&server_key.e_bytes(), size));
        sock.write_all(&server_wire).await?;

        let client_bits = sock.read_u32().await?;
        let client_size = (client_bits as usize).div_ceil(8);
        let mut client_wire = client_bits.to_be_bytes().to_vec();
        client_wire.resize(4 + 2 * client_size, 0);
        sock.read_exact(&mut client_wire[4..]).await?;
        let client_key = RsaPublicKey::new(
            BoxedUint::from_be_slice_vartime(&client_wire[4..4 + client_size]),
            BoxedUint::from_be_slice_vartime(&client_wire[4 + client_size..]),
        )
        .unwrap();

        let random_len = strength.random_len();
        let mut server_random = vec![0u8; random_len];
        rand::rng().fill_bytes(&mut server_random);
        let sealed = client_key.encrypt(&mut rand::rng(), Pkcs1v15Encrypt, &server_random).unwrap();
        let mut out = (sealed.len() as u16).to_be_bytes().to_vec();
        out.extend(&sealed);
        sock.write_all(&out).await?;

        let len = usize::from(sock.read_u16().await?);
        assert_eq!(len, size);
        let mut sealed = vec![0u8; len];
        sock.read_exact(&mut sealed).await?;
        let client_random = server_key.decrypt(Pkcs1v15Encrypt, &sealed).unwrap();

        // The server's send key is H(client || server); its receive key the reverse.
        let send = strength.hash(&[&client_random, &server_random]);
        let recv = strength.hash(&[&server_random, &client_random]);
        let mut sealer = Sealer::new(strength.cipher(&send[..random_len]));
        let mut server_hash = strength.hash(&[&server_wire, &client_wire]);
        if lie_in_hash {
            server_hash[0] ^= 1;
        }
        sock.write_all(&sealer.frame(&server_hash)).await?;

        let mut frames = FrameReader::new(&mut sock, Opener::new(strength.cipher(&recv[..random_len])));
        let mut client_hash = vec![0u8; server_hash.len()];
        frames.read_exact(&mut client_hash).await?;
        assert_eq!(client_hash, strength.hash(&[&client_wire, &server_wire]));
        let (sock, opener) = frames.into_parts();
        sock.write_all(&sealer.frame(&[subtype])).await?;

        let mut frames = FrameReader::new(sock, opener);
        let mut fields = Vec::new();
        for _ in 0..2 {
            let len = usize::from(frames.read_u8().await?);
            let mut field = vec![0u8; len];
            frames.read_exact(&mut field).await?;
            fields.push(String::from_utf8(field).unwrap());
        }
        let (sock, _) = frames.into_parts();
        sock.write_all(&sealer.frame(&[0, 0, 0, 0])).await?;
        Ok((fields.remove(0), fields.remove(0)))
    }

    #[tokio::test]
    async fn the_client_half_logs_in_to_a_server_written_from_the_specification() {
        for (strength, subtype, sent_username) in [(Strength::Aes128, 1u8, "andrew"), (Strength::Aes256, 2, "")] {
            let (client_sock, server_sock) = tokio::io::duplex(4096);
            let server = tokio::spawn(scripted_server(server_sock, strength, subtype, false));
            let (mut reader, mut writer) = tokio::io::split(client_sock);
            // A key apiece, because `begin` takes one: two exchanges are two
            // connections and a connection's key is its own.
            let key = ClientKey::of_bits(1024).unwrap();
            let exchange = begin(&mut reader, &mut writer, strength, key).await.unwrap();
            assert_eq!(exchange.subtype().byte(), subtype);
            assert_eq!(exchange.fingerprint().len(), 8 * 2 + 7);
            let credentials = Credentials { username: "andrew".to_owned(), password: "hunter2".to_owned() };
            let Session { opener, .. } = exchange.login(&mut writer, &credentials).await.unwrap();
            let mut frames = FrameReader::new(reader, opener);
            assert_eq!(frames.read_u32().await.unwrap(), 0, "SecurityResult arrives inside the frames");
            assert_eq!(server.await.unwrap().unwrap(), (sent_username.to_owned(), "hunter2".to_owned()));
        }
    }

    #[tokio::test]
    async fn the_client_half_meets_the_server_half() {
        let server_key = test_key();
        let fingerprint = server_key.fingerprint();
        let (client_sock, server_sock) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(server_sock);
            authenticate(&mut reader, &mut writer, Strength::Aes256, &server_key, Subtype::UserPass).await.map(|(credentials, _)| credentials)
        });
        let (mut reader, mut writer) = tokio::io::split(client_sock);
        let exchange = begin(&mut reader, &mut writer, Strength::Aes256, ClientKey::of_bits(1024).unwrap()).await.unwrap();
        assert_eq!(exchange.fingerprint(), fingerprint, "the client shows the fingerprint the server logs");
        let credentials = Credentials { username: "andrew".to_owned(), password: "hunter2".to_owned() };
        exchange.login(&mut writer, &credentials).await.unwrap();
        assert_eq!(server.await.unwrap().unwrap(), credentials);
    }

    #[tokio::test]
    async fn a_server_hash_over_other_keys_ends_the_exchange_before_any_credentials() {
        let (client_sock, server_sock) = tokio::io::duplex(4096);
        let server = tokio::spawn(scripted_server(server_sock, Strength::Aes128, 1, true));
        let (mut reader, mut writer) = tokio::io::split(client_sock);
        let result = begin(&mut reader, &mut writer, Strength::Aes128, ClientKey::of_bits(1024).unwrap()).await;
        assert!(matches!(result, Err(Error::Tampered)), "{:?}", result.err());
        drop((reader, writer));
        assert!(server.await.unwrap().is_err(), "the server is hung up on at the hash");
    }

    #[tokio::test]
    async fn a_credential_too_long_for_its_length_byte_is_refused_unsent() {
        let (client_sock, server_sock) = tokio::io::duplex(4096);
        let _server = tokio::spawn(scripted_server(server_sock, Strength::Aes128, 2, false));
        let (mut reader, mut writer) = tokio::io::split(client_sock);
        let exchange = begin(&mut reader, &mut writer, Strength::Aes128, ClientKey::of_bits(1024).unwrap()).await.unwrap();
        let credentials = Credentials { username: String::new(), password: "x".repeat(256) };
        let err = exchange.login(&mut writer, &credentials).await.err().expect("256 bytes do not fit");
        assert!(err.to_string().contains("256 bytes"), "{err}");
    }
}
