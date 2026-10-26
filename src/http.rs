//! Conditional GET over `embedded_io`, optionally in TLS 1.3; shared with the
//! host harness, so no esp-hal.
#![forbid(unsafe_code)]

use embedded_io::{Read, Write};
use embedded_tls::blocking::{
    Aes128GcmSha256, Certificate, CertificateEntryRef, CertificateRef, CertificateVerifyRef,
    InsecureIgnoreDates, TlsCipherSuite, TlsConfig, TlsConnection, TlsContext, TlsVerifier,
};
use embedded_tls::embassy_crypto::Sha256;
use embedded_tls::webpki::CertVerifier;
use embedded_tls::{SignatureScheme, TlsError};
use pki_types::{
    AlgorithmIdentifier, CertificateDer, InvalidSignature, SignatureVerificationAlgorithm, alg_id,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    ConnectFail,
    Timeout,
    Malformed,
    /// Neither 200 nor 304.
    HttpStatus(u16),
    Unsupported,
    TooLarge,
    TooShort,
    Tls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Fetched(usize),
    NotModified,
}

pub trait IntoFetchError {
    fn into_fetch(self) -> FetchError;
}

impl IntoFetchError for TlsError {
    fn into_fetch(self) -> FetchError {
        match self {
            Self::Io(embedded_io::ErrorKind::TimedOut) => FetchError::Timeout,
            Self::Io(_) => FetchError::ConnectFail,
            _ => FetchError::Tls,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Inline<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Inline<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0; N],
            len: 0,
        }
    }

    pub fn get(&self) -> &[u8] {
        self.buf.get(..self.len).unwrap_or_default()
    }

    /// Empties if `bytes` doesn't fit.
    pub fn set(&mut self, bytes: &[u8]) {
        self.len = self.buf.get_mut(..bytes.len()).map_or(0, |dst| {
            dst.copy_from_slice(bytes);
            bytes.len()
        });
    }
}

impl<const N: usize> Default for Inline<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Quotes included.
pub type Etag = Inline<64>;

pub struct TlsBufs<'a> {
    pub read: &'a mut [u8],
    pub write: &'a mut [u8],
}

/// A full 16KB record, with header and tag.
pub const TLS_READ: usize = 16640;
pub const TLS_WRITE: usize = 4096;

pub struct Request<'a> {
    pub head: &'a [u8],
    pub sent: &'a mut dyn FnMut(),
}

pub struct Response<'a> {
    pub body: &'a mut [u8],
    pub etag: &'a mut Etag,
}

/// The token goes in a header, never the logged path.
pub fn build_request(
    host: &str,
    path: &str,
    etag: Option<&str>,
    bearer: Option<&str>,
) -> alloc::string::String {
    let mut req = alloc::format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (name, value) in [
        ("If-None-Match: ", etag),
        ("Authorization: Bearer ", bearer),
    ] {
        if let Some(value) = value {
            req.push_str(name);
            req.push_str(value);
            req.push_str("\r\n");
        }
    }
    req.push_str("\r\n");
    req
}

/// Fly.io's heads are ~640 bytes.
const HEAD_BUF: usize = 2048;

/// The body ends at Content-Length, else at the close.
pub fn exchange<T>(
    io: &mut T,
    Request {
        head: request,
        sent,
    }: Request<'_>,
    Response { body: buffer, etag }: Response<'_>,
) -> Result<FetchOutcome, FetchError>
where
    T: Read + Write,
    T::Error: IntoFetchError,
{
    io.write_all(request).map_err(IntoFetchError::into_fetch)?;
    io.flush().map_err(IntoFetchError::into_fetch)?;
    sent();

    let mut buf = [0u8; HEAD_BUF];
    let (head, early_body) = read_head(io, &mut buf)?;
    match parse_status(head).ok_or(FetchError::Malformed)? {
        304 => return Ok(FetchOutcome::NotModified),
        200 => {}
        other => return Err(FetchError::HttpStatus(other)),
    }
    if header_value(head, b"transfer-encoding")
        .is_some_and(|v| v.windows(7).any(|w| w.eq_ignore_ascii_case(b"chunked")))
    {
        return Err(FetchError::Unsupported);
    }
    let declared = header_value(head, b"content-length")
        .map(|v| parse_decimal(v).ok_or(FetchError::Malformed))
        .transpose()?;
    etag.set(header_value(head, b"etag").unwrap_or_default());

    let (body, overrun) = match declared {
        Some(n) => (
            buffer.get_mut(..n).ok_or(FetchError::TooLarge)?,
            FetchError::Malformed,
        ),
        None => (buffer, FetchError::TooLarge),
    };
    let (start, _) = body.split_at_mut_checked(early_body.len()).ok_or(overrun)?;
    start.copy_from_slice(early_body);
    let mut filled = early_body.len();
    while let Some(free) = body.get_mut(filled..).filter(|free| !free.is_empty()) {
        match io.read(free).map_err(IntoFetchError::into_fetch)? {
            0 if declared.is_none() => return Ok(FetchOutcome::Fetched(filled)),
            0 => return Err(FetchError::TooShort),
            n => filled += n,
        }
    }
    // An undeclared body filling the buffer must end.
    if declared.is_none() && io.read(&mut [0]).map_err(IntoFetchError::into_fetch)? != 0 {
        return Err(FetchError::TooLarge);
    }
    Ok(FetchOutcome::Fetched(filled))
}

/// (head, any body bytes read with it).
fn read_head<'b, T>(io: &mut T, buf: &'b mut [u8]) -> Result<(&'b [u8], &'b [u8]), FetchError>
where
    T: Read,
    T::Error: IntoFetchError,
{
    let mut len = 0;
    let end = loop {
        let free = buf.get_mut(len..).filter(|free| !free.is_empty());
        let n = io
            .read(free.ok_or(FetchError::Malformed)?)
            .map_err(IntoFetchError::into_fetch)?;
        if n == 0 {
            return Err(FetchError::TooShort);
        }
        len += n;
        let got = buf.get(..len).unwrap_or_default();
        if let Some(end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let got: &'b [u8] = buf.get(..len).unwrap_or_default();
    got.split_at_checked(end).ok_or(FetchError::Malformed)
}

fn parse_status(head: &[u8]) -> Option<u16> {
    let rest = head
        .strip_prefix(b"HTTP/1.1 ")
        .or_else(|| head.strip_prefix(b"HTTP/1.0 "))?;
    parse_decimal(rest.first_chunk::<3>()?).and_then(|code| u16::try_from(code).ok())
}

fn parse_decimal(digits: &[u8]) -> Option<usize> {
    if digits.is_empty() || digits.len() > 9 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0, |acc, &d| acc * 10 + usize::from(d - b'0')),
    )
}

/// `name` in lower case.
fn header_value<'h>(head: &'h [u8], name: &[u8]) -> Option<&'h [u8]> {
    head.split(|&b| b == b'\n').find_map(|line| {
        let (key, rest) = line.split_at_checked(name.len())?;
        let value = rest.strip_prefix(b":")?;
        key.eq_ignore_ascii_case(name).then(|| value.trim_ascii())
    })
}

/// `cache` lives across fetches.
pub struct TlsVerify<'a> {
    pub roots: &'a [&'a [u8]],
    pub name: &'a str,
    pub cache: &'a mut ChainCache,
}

/// The server's own certificate, kept for `CertificateVerify`.
const LEAF_BUF: usize = 2048;

/// SHA-256 of (name, roots, served DERs) of the last verified chain: with no
/// clock, a match can skip path-building.
pub struct ChainCache {
    digest: Option<[u8; 32]>,
    pub last_hit: bool,
    /// Roots tried, in store order, up to the one that verified.
    pub last_attempts: u8,
}

impl ChainCache {
    pub const fn new() -> Self {
        Self {
            digest: None,
            last_hit: false,
            last_attempts: 0,
        }
    }
}

impl Default for ChainCache {
    fn default() -> Self {
        Self::new()
    }
}

type Hash = <Aes128GcmSha256 as TlsCipherSuite>::Hash;

/// No clock: rustls-webpki takes the newest `notBefore` in the chain as now.
type ChainVerifier<'a> = CertVerifier<'a, Aes128GcmSha256, InsecureIgnoreDates, LEAF_BUF>;

/// What checks the server's `CertificateVerify`.
enum Proof<'a> {
    None,
    /// The verifier that accepted the chain, holding the leaf.
    Chain(ChainVerifier<'a>),
    /// The memo vouched for the chain. The record buffer is reused before
    /// `CertificateVerify`, so the leaf is copied.
    Memo {
        leaf: Inline<LEAF_BUF>,
        transcript: Hash,
    },
}

struct MultiRootVerifier<'a> {
    roots: &'a [&'a [u8]],
    name: &'a str,
    cache: &'a mut ChainCache,
    proof: Proof<'a>,
}

impl MultiRootVerifier<'_> {
    fn chain_digest(&self, cert: &CertificateRef<'_>) -> Option<[u8; 32]> {
        let mut h = Sha256::new();
        let mut field = |bytes: &[u8]| {
            h.update(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
            h.update(bytes);
        };
        field(b"inkplate-chain-v1");
        field(self.name.as_bytes());
        self.roots.iter().for_each(|root| field(root));
        field(b"served");
        for entry in &cert.entries {
            match entry {
                CertificateEntryRef::X509(der) => field(der),
                CertificateEntryRef::RawPublicKey(_) => return None,
            }
        }
        Some(h.finalize())
    }
}

/// The verifier takes the chain by value, so each root gets a copy.
fn copy_chain<'c>(cert: &CertificateRef<'c>) -> Result<CertificateRef<'c>, TlsError> {
    let mut copy = CertificateRef::with_context(&[]);
    for entry in &cert.entries {
        copy.add(match entry {
            CertificateEntryRef::X509(der) => CertificateEntryRef::X509(der),
            CertificateEntryRef::RawPublicKey(key) => CertificateEntryRef::RawPublicKey(key),
        })?;
    }
    Ok(copy)
}

impl<'a> TlsVerifier<Aes128GcmSha256> for MultiRootVerifier<'a> {
    /// Checked against `name`.
    fn set_hostname_verification(&mut self, _: &str) -> Result<(), TlsError> {
        Ok(())
    }

    /// Each root in store order, until one verifies the chain.
    fn verify_certificate(
        &mut self,
        transcript: &Hash,
        cert: CertificateRef<'_>,
    ) -> Result<(), TlsError> {
        let digest = self.chain_digest(&cert);
        self.cache.last_attempts = 0;
        self.cache.last_hit = digest.is_some() && digest == self.cache.digest;
        if self.cache.last_hit {
            let Some(CertificateEntryRef::X509(der)) = cert.entries.first() else {
                return Err(TlsError::DecodeError);
            };
            let mut leaf = Inline::new();
            leaf.set(der);
            if leaf.get().len() != der.len() {
                return Err(TlsError::InsufficientSpace);
            }
            self.proof = Proof::Memo {
                leaf,
                transcript: transcript.clone(),
            };
            return Ok(());
        }
        let roots: &'a [&'a [u8]] = self.roots;
        for root in roots {
            self.cache.last_attempts = self.cache.last_attempts.saturating_add(1);
            self.proof = Proof::Chain(ChainVerifier::new(Certificate::X509(root)));
            if let Proof::Chain(verifier) = &mut self.proof {
                verifier.set_hostname_verification(self.name)?;
                if verifier
                    .verify_certificate(transcript, copy_chain(&cert)?)
                    .is_ok()
                {
                    self.cache.digest = digest;
                    return Ok(());
                }
            }
        }
        self.proof = Proof::None;
        Err(TlsError::InvalidCertificate)
    }

    /// RFC 8446 §4.4.3: a replayed public chain is refused.
    fn verify_signature(&mut self, verify: CertificateVerifyRef<'_>) -> Result<(), TlsError> {
        let result = match &mut self.proof {
            Proof::None => Err(TlsError::InvalidCertificate),
            Proof::Chain(verifier) => verifier.verify_signature(verify),
            Proof::Memo { leaf, transcript } => memo_signature(leaf.get(), transcript, &verify),
        };
        self.proof = Proof::None;
        result
    }
}

const VERIFY_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify\x00";

/// The check the chain verifier makes, on the leaf the memo vouched for.
fn memo_signature(
    leaf: &[u8],
    transcript: &Hash,
    verify: &CertificateVerifyRef<'_>,
) -> Result<(), TlsError> {
    let hash = transcript.clone().finalize();
    let mut msg = [b' '; 64 + 34 + 32];
    for (dst, src) in msg
        .iter_mut()
        .skip(64)
        .zip(VERIFY_CONTEXT.iter().chain(&hash))
    {
        *dst = *src;
    }
    let algorithm: &dyn SignatureVerificationAlgorithm = match verify.signature_scheme {
        SignatureScheme::EcdsaSecp256r1Sha256 => &ECDSA_P256_SHA256,
        SignatureScheme::EcdsaSecp384r1Sha384 => &ECDSA_P384_SHA384,
        _ => return Err(TlsError::InvalidSignatureScheme),
    };
    let der = CertificateDer::from(leaf);
    let cert = webpki::EndEntityCert::try_from(&der).map_err(decode_error)?;
    cert.verify_signature(algorithm, &msg, verify.signature)
        .map_err(invalid_signature)
}

fn decode_error<E>(_: E) -> TlsError {
    TlsError::DecodeError
}

fn invalid_signature<E>(_: E) -> TlsError {
    TlsError::InvalidSignature
}

const fn rejected(_: ecdsa::Error) -> InvalidSignature {
    InvalidSignature
}

/// (public key, message, DER signature).
type VerifyFn = fn(&[u8], &[u8], &[u8]) -> Result<(), ecdsa::Error>;

/// A rustls-webpki algorithm over the `RustCrypto` curve crates, for the
/// `CertificateVerify` schemes the client offers.
#[derive(Debug)]
struct Ecdsa {
    key: AlgorithmIdentifier,
    hash: AlgorithmIdentifier,
    verify: VerifyFn,
}

impl SignatureVerificationAlgorithm for Ecdsa {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        (self.verify)(public_key, message, signature).map_err(rejected)
    }

    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.key
    }

    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.hash
    }
}

static ECDSA_P256_SHA256: Ecdsa = Ecdsa {
    key: alg_id::ECDSA_P256,
    hash: alg_id::ECDSA_SHA256,
    verify: |key, message, signature| {
        use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
        VerifyingKey::from_sec1_bytes(key)?.verify(message, &Signature::from_der(signature)?)
    },
};

static ECDSA_P384_SHA384: Ecdsa = Ecdsa {
    key: alg_id::ECDSA_P384,
    hash: alg_id::ECDSA_SHA384,
    verify: |key, message, signature| {
        use p384::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
        VerifyingKey::from_sec1_bytes(key)?.verify(message, &Signature::from_der(signature)?)
    },
};

/// Randomness comes from embassy-crypto's RNG driver.
pub fn tls_exchange<T>(
    io: T,
    TlsBufs { read, write }: TlsBufs<'_>,
    TlsVerify { roots, name, cache }: TlsVerify<'_>,
    request: Request<'_>,
    response: Response<'_>,
) -> Result<FetchOutcome, FetchError>
where
    T: Read + Write,
    T::Error: embedded_io::Error,
{
    let mut tls: TlsConnection<'_, T, Aes128GcmSha256> = TlsConnection::new(io, read, write);
    let opened = tls.open(TlsContext::new(
        &TlsConfig::new().with_server_name(name),
        MultiRootVerifier {
            roots,
            name,
            cache,
            proof: Proof::None,
        },
    ));
    let result = opened
        .map_err(IntoFetchError::into_fetch)
        .and_then(|()| exchange(&mut tls, request, response));
    // A failed close_notify doesn't spoil a response already read.
    drop(match tls.close() {
        Ok(socket) | Err((socket, _)) => socket,
    });
    result
}
