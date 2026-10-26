//! Conditional GET over `embedded_io`, optionally in TLS 1.3; shared with the
//! host harness, so no esp-hal.
#![forbid(unsafe_code)]

use embedded_io::{Read, Write};
use embedded_tls::blocking::{
    Aes128GcmSha256, NoClock, TlsCipherSuite, TlsConfig, TlsConnection, TlsContext,
};
use embedded_tls::pki::CertVerifier;
use embedded_tls::{
    Certificate, CertificateEntryRef, CertificateRef, CertificateVerifyRef, CryptoProvider,
    CryptoRngCore, SignatureScheme, TlsError, TlsVerifier,
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

const CERT_BUF: usize = 4096;
const LEAF_BUF: usize = 2048;

/// SHA-256 of (name, roots, served DERs) of the last verified chain: with no
/// clock, a match can skip path-building.
pub struct ChainCache {
    digest: Option<[u8; 32]>,
    pub last_hit: bool,
    /// Candidates that ran signature math.
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

struct MultiRootVerifier<'a> {
    roots: &'a [&'a [u8]],
    name: &'a str,
    cache: &'a mut ChainCache,
    leaf: Inline<LEAF_BUF>,
    transcript: Option<Hash>,
}

impl MultiRootVerifier<'_> {
    fn chain_digest(&self, cert: &CertificateRef<'_>) -> Option<[u8; 32]> {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        let mut field = |bytes: &[u8]| {
            h.update(u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_be_bytes());
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
        Some(h.finalize().into())
    }

    /// Each root against each truncation (cross-signed tails) whose top names
    /// it as issuer, with only CAs above the leaf.
    fn verify_path(
        &mut self,
        transcript: &Hash,
        cert: &CertificateRef<'_>,
    ) -> Result<(), TlsError> {
        for root in self.roots {
            let Ok((_, root_subject)) = issuer_subject(root) else {
                continue;
            };
            for keep in (1..=cert.entries.len()).rev() {
                let Some(entries) = cert.entries.get(..keep) else {
                    continue;
                };
                let issued_by_root = matches!(
                    entries.last(),
                    Some(CertificateEntryRef::X509(top))
                        if issuer_subject(top).is_ok_and(|(issuer, _)| issuer == root_subject)
                );
                let signed_by_cas = entries
                    .iter()
                    .skip(1)
                    .all(|entry| matches!(entry, CertificateEntryRef::X509(der) if is_ca(der)));
                if !issued_by_root || !signed_by_cas {
                    continue;
                }
                self.cache.last_attempts = self.cache.last_attempts.saturating_add(1);
                let mut candidate = CertificateRef::with_context(&[]);
                for entry in entries {
                    candidate.add(match entry {
                        CertificateEntryRef::X509(der) => CertificateEntryRef::X509(der),
                        CertificateEntryRef::RawPublicKey(key) => {
                            CertificateEntryRef::RawPublicKey(key)
                        }
                    })?;
                }
                let mut verifier: CertVerifier<'_, Aes128GcmSha256, NoClock, CERT_BUF> =
                    CertVerifier::new(Certificate::X509(root));
                verifier.set_hostname_verification(self.name)?;
                if verifier.verify_certificate(transcript, candidate).is_ok() {
                    return Ok(());
                }
            }
        }
        Err(TlsError::InvalidCertificate)
    }
}

fn decode_error<E>(_: E) -> TlsError {
    TlsError::DecodeError
}

/// Past the optional `[0]` version.
fn tbs_reader(cert_der: &[u8]) -> Result<der::SliceReader<'_>, TlsError> {
    use der::{Decode as _, Reader as _};
    let mut r = der::SliceReader::new(cert_der).map_err(decode_error)?;
    der::Header::decode(&mut r).map_err(decode_error)?;
    der::Header::decode(&mut r).map_err(decode_error)?;
    let peeked = der::Header::peek(&r).map_err(decode_error)?;
    if matches!(peeked.tag(), der::Tag::ContextSpecific { .. }) {
        r.tlv_bytes().map_err(decode_error)?;
    }
    Ok(r)
}

/// Raw DER: a CA spelling its name two ways fails closed.
fn issuer_subject(cert_der: &[u8]) -> Result<(&[u8], &[u8]), TlsError> {
    use der::Reader as _;
    let mut r = tbs_reader(cert_der)?;
    let mut next = || r.tlv_bytes().map_err(decode_error);
    let [_serial, _signature, issuer, _validity, subject] =
        [next()?, next()?, next()?, next()?, next()?];
    Ok((issuer, subject))
}

const EXTENSIONS: u8 = 0xA3;
const BASIC_CONSTRAINTS: &[u8] = &[0x06, 0x03, 0x55, 0x1D, 0x13];
const CRITICAL: &[u8] = &[0x01, 0x01, 0xFF];

/// basicConstraints `cA`, which embedded-tls never checks: else any leaf
/// could sign for any name.
fn is_ca(cert_der: &[u8]) -> bool {
    tbs_reader(cert_der).is_ok_and(|tbs| ca_flag(tbs) == Ok(true))
}

/// Only the TBS has `EXTENSIONS`.
fn ca_flag(mut tbs: der::SliceReader<'_>) -> der::Result<bool> {
    use der::{Decode as _, Reader as _};
    while tbs.peek_byte().is_some_and(|tag| tag != EXTENSIONS) {
        tbs.tlv_bytes()?;
    }
    if tbs.is_finished() {
        return Ok(false);
    }
    let mut exts = der::SliceReader::new(tbs.tlv_bytes()?)?;
    der::Header::decode(&mut exts)?;
    der::Header::decode(&mut exts)?;
    while !exts.is_finished() {
        let mut ext = der::SliceReader::new(exts.tlv_bytes()?)?;
        der::Header::decode(&mut ext)?;
        if ext.tlv_bytes()? == BASIC_CONSTRAINTS {
            // [critical,] OCTET STRING { SEQUENCE { cA TRUE, .. } }
            let rest = ext.read_slice(ext.remaining_len())?;
            let value = rest.strip_prefix(CRITICAL).unwrap_or(rest);
            return Ok(matches!(value, [0x04, _, 0x30, _, 0x01, 0x01, 0xFF, ..]));
        }
    }
    Ok(false)
}

fn spki_point(cert_der: &[u8]) -> Result<&[u8], TlsError> {
    use der::{Decode as _, Reader as _};
    let mut r = tbs_reader(cert_der)?;
    for _ in 0..5 {
        r.tlv_bytes().map_err(decode_error)?;
    }
    der::Header::decode(&mut r).map_err(decode_error)?;
    r.tlv_bytes().map_err(decode_error)?;
    let key = der::asn1::BitStringRef::decode(&mut r).map_err(decode_error)?;
    key.as_bytes().ok_or(TlsError::DecodeError)
}

const VERIFY_CONTEXT: &[u8] = b"TLS 1.3, server CertificateVerify\x00";

impl TlsVerifier<Aes128GcmSha256> for MultiRootVerifier<'_> {
    /// Checked against `name`.
    fn set_hostname_verification(&mut self, _: &str) -> Result<(), TlsError> {
        Ok(())
    }

    fn verify_certificate(
        &mut self,
        transcript: &Hash,
        cert: CertificateRef<'_>,
    ) -> Result<(), TlsError> {
        let digest = self.chain_digest(&cert);
        self.cache.last_attempts = 0;
        self.cache.last_hit = digest.is_some() && digest == self.cache.digest;
        if !self.cache.last_hit {
            self.verify_path(transcript, &cert)?;
            self.cache.digest = digest;
        }
        // The record buffer is reused before `CertificateVerify`.
        let Some(CertificateEntryRef::X509(leaf)) = cert.entries.first() else {
            return Err(TlsError::DecodeError);
        };
        self.leaf.set(leaf);
        if self.leaf.get().len() != leaf.len() {
            return Err(TlsError::InsufficientSpace);
        }
        self.transcript = Some(transcript.clone());
        Ok(())
    }

    /// RFC 8446 §4.4.3: a replayed public chain is refused.
    fn verify_signature(&mut self, verify: CertificateVerifyRef<'_>) -> Result<(), TlsError> {
        use sha2::Digest as _;
        let hash = self
            .transcript
            .take()
            .ok_or(TlsError::InvalidCertificate)?
            .finalize();
        let mut msg = [b' '; 64 + 34 + 32];
        for (dst, src) in msg
            .iter_mut()
            .skip(64)
            .zip(VERIFY_CONTEXT.iter().chain(&hash))
        {
            *dst = *src;
        }
        let point = spki_point(self.leaf.get())?;
        let ok = match verify.signature_scheme {
            SignatureScheme::EcdsaSecp256r1Sha256 => {
                use p256::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
                let key = VerifyingKey::from_sec1_bytes(point).map_err(decode_error)?;
                let sig = Signature::from_der(verify.signature).map_err(decode_error)?;
                key.verify(&msg, &sig).is_ok()
            }
            SignatureScheme::EcdsaSecp384r1Sha384 => {
                use p384::ecdsa::{Signature, VerifyingKey, signature::Verifier as _};
                let key = VerifyingKey::from_sec1_bytes(point).map_err(decode_error)?;
                let sig = Signature::from_der(verify.signature).map_err(decode_error)?;
                key.verify(&msg, &sig).is_ok()
            }
            _ => return Err(TlsError::InvalidSignatureScheme),
        };
        if ok {
            Ok(())
        } else {
            Err(TlsError::InvalidSignature)
        }
    }
}

struct RootsProvider<'v, RNG> {
    rng: RNG,
    verifier: MultiRootVerifier<'v>,
}

impl<RNG: CryptoRngCore> CryptoProvider for RootsProvider<'_, RNG> {
    type CipherSuite = Aes128GcmSha256;
    type Signature = &'static [u8];

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}

pub fn tls_exchange<T>(
    io: T,
    TlsBufs { read, write }: TlsBufs<'_>,
    rng: impl CryptoRngCore,
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
        RootsProvider {
            rng,
            verifier: MultiRootVerifier {
                roots,
                name,
                cache,
                leaf: Inline::new(),
                transcript: None,
            },
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
