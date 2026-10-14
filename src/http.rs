//! Conditional GET over `embedded_io`; shared with the host harness, so no esp-hal.
#![forbid(unsafe_code)]

use embedded_io::{Read, Write};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    Fetched(usize),
    NotModified,
}

pub trait IntoFetchError {
    fn into_fetch(self) -> FetchError;
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
