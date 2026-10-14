//! The firmware's `http` and `png` against a local server and fixtures, run by
//! `scripts/host_tls_test.sh`.

extern crate alloc;

#[path = "../../src/http.rs"]
mod http;
#[path = "../../src/png.rs"]
mod png;

use core::fmt::Debug;
use http::{Etag, FetchError, FetchOutcome, IntoFetchError, Request, Response};
use sha2::{Digest as _, Sha256};
use std::net::TcpStream;
use std::process::ExitCode;
use std::time::{Duration, Instant};

const NAME: &str = "inkplate-test";
const PANEL: (usize, usize) = (1280, 720);

type Outcome = Result<FetchOutcome, FetchError>;

#[derive(Debug)]
struct StdErr(std::io::ErrorKind);

impl core::fmt::Display for StdErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl core::error::Error for StdErr {}

impl embedded_io::Error for StdErr {
    fn kind(&self) -> embedded_io::ErrorKind {
        match self.0 {
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                embedded_io::ErrorKind::TimedOut
            }
            _ => embedded_io::ErrorKind::ConnectionReset,
        }
    }
}

impl IntoFetchError for StdErr {
    fn into_fetch(self) -> FetchError {
        match embedded_io::Error::kind(&self) {
            embedded_io::ErrorKind::TimedOut => FetchError::Timeout,
            _ => FetchError::ConnectFail,
        }
    }
}

struct StdIo(TcpStream);

impl embedded_io::ErrorType for StdIo {
    type Error = StdErr;
}

impl embedded_io::Read for StdIo {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, StdErr> {
        std::io::Read::read(&mut self.0, buf).map_err(|e| StdErr(e.kind()))
    }
}

impl embedded_io::Write for StdIo {
    fn write(&mut self, buf: &[u8]) -> Result<usize, StdErr> {
        std::io::Write::write(&mut self.0, buf).map_err(|e| StdErr(e.kind()))
    }

    fn flush(&mut self) -> Result<(), StdErr> {
        std::io::Write::flush(&mut self.0).map_err(|e| StdErr(e.kind()))
    }
}

fn connect(host: &str, port: u16, timeout: Duration) -> Result<StdIo, FetchError> {
    let io_err = |e: std::io::Error| StdErr(e.kind()).into_fetch();
    let stream = TcpStream::connect((host, port)).map_err(io_err)?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_nodelay(true))
        .map_err(io_err)?;
    Ok(StdIo(stream))
}

/// The test server's body: SHA-256 blocks of "1:n" (`tls_test_server.py`).
fn expected_body(size: usize) -> Vec<u8> {
    (0u32..)
        .flat_map(|n| Sha256::digest(format!("1:{n}")))
        .take(size)
        .collect()
}

/// A GET against the local test server.
struct Get<'a> {
    port: u16,
    path: &'a str,
    bearer: Option<&'a str>,
}

impl Get<'_> {
    fn run(self, body: &mut [u8], etag: &mut Etag) -> Outcome {
        let host_header = format!("{NAME}:{}", self.port);
        let sent_etag = core::str::from_utf8(etag.get())
            .ok()
            .filter(|e| !e.is_empty());
        let head = http::build_request(&host_header, self.path, sent_etag, self.bearer);
        let request = Request {
            head: head.as_bytes(),
            sent: &mut || {},
        };
        let response = Response { body, etag };
        let mut io = connect("127.0.0.1", self.port, Duration::from_secs(15))?;
        http::exchange(&mut io, request, response)
    }
}

fn is(want: Outcome) -> impl FnOnce(&Outcome) -> bool {
    move |got| *got == want
}

struct Report {
    failures: u32,
}

impl Report {
    fn check<T: Debug>(&mut self, name: &str, got: T, ok: impl FnOnce(&T) -> bool) {
        if ok(&got) {
            println!("PASS  {name}");
        } else {
            println!("FAIL  {name}: {got:?}");
            self.failures += 1;
        }
    }
}

struct Local {
    plain: u16,
    size: usize,
    token: String,
    want: Vec<u8>,
}

impl Local {
    /// The plain-http server, with the right token.
    fn plain<'a>(&'a self, path: &'a str) -> Get<'a> {
        Get {
            port: self.plain,
            path,
            bearer: Some(&self.token),
        }
    }
}

fn local_protocol(cfg: &Local, report: &mut Report) {
    let path = "/frame";
    let fetched = Ok(FetchOutcome::Fetched(cfg.size));
    let mut body = vec![0; cfg.size];
    let mut etag = Etag::new();
    report.check(
        "plain http 200 + body",
        cfg.plain(path).run(&mut body, &mut etag),
        |r| *r == fetched && body == cfg.want && !etag.get().is_empty(),
    );
    report.check(
        "plain http 304 on match",
        cfg.plain(path).run(&mut body, &mut etag),
        is(Ok(FetchOutcome::NotModified)),
    );
}

fn local_framing(cfg: &Local, report: &mut Report) {
    let fetched = |n| Ok(FetchOutcome::Fetched(n));
    let plain = |query: &str, body: &mut [u8]| {
        cfg.plain(&format!("/frame?{query}"))
            .run(body, &mut Etag::new())
    };
    let bearer = |token, body: &mut [u8]| {
        let mut get = cfg.plain("/frame");
        get.bearer = token;
        get.run(body, &mut Etag::new())
    };
    let mut big = vec![0; cfg.size + 100];
    let mut small = vec![0; cfg.size - 1];
    report.check(
        "short body -> TooShort",
        plain("short=1", &mut big),
        is(Err(FetchError::TooShort)),
    );
    report.check(
        "content-length body < buffer",
        plain("len=4028", &mut big),
        |r| *r == fetched(4028) && big.get(..4028) == Some(&expected_body(4028)[..]),
    );
    report.check("close-delimited body", plain("nolen=1", &mut big), |r| {
        *r == fetched(cfg.size) && big.get(..cfg.size) == Some(&cfg.want[..])
    });
    report.check(
        "declared body > buffer -> TooLarge",
        plain("x=1", &mut small),
        is(Err(FetchError::TooLarge)),
    );
    report.check(
        "undeclared body > buffer -> TooLarge",
        plain("nolen=1", &mut small),
        is(Err(FetchError::TooLarge)),
    );
    report.check(
        "chunked -> Unsupported",
        plain("chunked=1", &mut big),
        is(Err(FetchError::Unsupported)),
    );
    report.check(
        "404 -> HttpStatus(404)",
        cfg.plain("/nope").run(&mut big, &mut Etag::new()),
        is(Err(FetchError::HttpStatus(404))),
    );
    report.check(
        "no bearer token -> HttpStatus(403)",
        bearer(None, &mut big),
        is(Err(FetchError::HttpStatus(403))),
    );
    report.check(
        "wrong bearer token -> HttpStatus(403)",
        bearer(Some("nope"), &mut big),
        is(Err(FetchError::HttpStatus(403))),
    );
}

/// Each manifest line: `<png> <w> <h> ok:<1bpp>:<levels>` or `err:<variant>`.
fn png_fixtures(dir: &str, report: &mut Report) -> Result<(), String> {
    let read =
        |file: &str| std::fs::read(format!("{dir}/{file}")).map_err(|e| format!("{file}: {e}"));
    let manifest = String::from_utf8(read("manifest.txt")?).map_err(|e| e.to_string())?;
    let mut scratch = vec![0; png::raw_len(PANEL.0, PANEL.1, 8)];
    for line in manifest.lines().filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split(' ').collect();
        let &[file, w, h, expect] = fields.as_slice() else {
            return Err(format!("bad manifest line: {line}"));
        };
        let dim = |n: &str| n.parse::<usize>().map_err(|e| format!("{line}: {e}"));
        let (width, height) = (dim(w)?, dim(h)?);
        let data = read(file)?;
        let mut fb = vec![0xA5; width.div_ceil(8) * height];
        let mut levels = vec![0xA5; png::levels_len(width, height)];
        let t = Instant::now();
        let got = png::decode(&data, width, height, &mut scratch).and_then(|image| {
            image.to_1bpp(&mut fb)?;
            image.to_levels(&mut levels)
        });
        let ok = match (expect.split_once(':'), &got) {
            (Some(("ok", want)), Ok(_)) => {
                let (want_1bpp, want_levels) = want.split_once(':').ok_or("ok:<1bpp>:<levels>")?;
                read(want_1bpp)? == fb && read(want_levels)? == levels
            }
            (Some(("err", want)), Err(e)) => {
                format!("{e:?}").split([' ', '{']).next() == Some(want)
            }
            _ => false,
        };
        report.check(
            &format!("png {file} ({:?})", t.elapsed()),
            (expect, got),
            |_| ok,
        );
    }
    Ok(())
}

fn local(args: &[String], report: &mut Report) -> Result<(), String> {
    let [plain, size, token] = args else {
        return Err("usage: <plain> <size> <token>".into());
    };
    let port = |p: &str| p.parse::<u16>().map_err(|e| format!("port {p}: {e}"));
    let body_size = size.parse().map_err(|e| format!("size: {e}"))?;
    let cfg = Local {
        plain: port(plain)?,
        size: body_size,
        token: token.clone(),
        want: expected_body(body_size),
    };
    local_protocol(&cfg, report);
    local_framing(&cfg, report);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut report = Report { failures: 0 };
    let run = match args.as_slice() {
        [mode, dir] if mode == "png" => png_fixtures(dir, &mut report),
        rest => local(rest, &mut report),
    };
    match run {
        Err(e) => {
            println!("ERROR {e}");
            ExitCode::FAILURE
        }
        Ok(()) if report.failures == 0 => {
            println!("ALL PASS");
            ExitCode::SUCCESS
        }
        Ok(()) => {
            println!("{} FAILURES", report.failures);
            ExitCode::FAILURE
        }
    }
}
