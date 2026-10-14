//! The firmware's `png` against fixtures, run by `scripts/host_tls_test.sh`.

#[path = "../../src/png.rs"]
mod png;

use core::fmt::Debug;
use std::process::ExitCode;
use std::time::Instant;

const PANEL: (usize, usize) = (1280, 720);

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut report = Report { failures: 0 };
    let run = match args.as_slice() {
        [mode, dir] if mode == "png" => png_fixtures(dir, &mut report),
        _ => Err("usage: png <dir>".into()),
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
