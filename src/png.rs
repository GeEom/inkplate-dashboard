//! Greyscale PNG (depth 1-8, any filter, split IDAT) to panel levels or 1bpp.
#![forbid(unsafe_code)]

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PngError {
    NotPng,
    Truncated,
    BadCrc,
    BadHeader,
    Unsupported,
    SizeMismatch { width: usize, height: usize },
    ScratchTooSmall,
    OutputTooSmall,
    Inflate,
    BadFilter,
}

struct PngInfo {
    width: usize,
    height: usize,
    bit_depth: u8,
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

pub const fn raw_len(width: usize, height: usize, bit_depth: usize) -> usize {
    ((width * bit_depth).div_ceil(8) + 1) * height
}

pub const fn levels_len(width: usize, height: usize) -> usize {
    width.div_ceil(2) * height
}

pub struct Image<'s> {
    info: PngInfo,
    stride: usize,
    raw: &'s [u8],
}

impl Image<'_> {
    fn rows(&self) -> impl Iterator<Item = &[u8]> {
        self.raw
            .chunks_exact(self.stride + 1)
            .filter_map(|row| row.split_first().map(|(_, data)| data))
    }

    fn samples<'r>(&self, row: &'r [u8]) -> impl Iterator<Item = u8> + 'r {
        let depth = self.info.bit_depth;
        let mask = u8::MAX >> (8 - depth);
        row.iter()
            .flat_map(move |&byte| (1..=8 / depth).map(move |k| (byte >> (8 - depth * k)) & mask))
            .take(self.info.width)
    }

    /// MSB leftmost, 1 = black.
    pub fn to_1bpp(&self, fb: &mut [u8]) -> Result<(), PngError> {
        let stride = self.info.width.div_ceil(8);
        let fb = fb
            .get_mut(..stride * self.info.height)
            .ok_or(PngError::OutputTooSmall)?;
        let half = 1 << (self.info.bit_depth - 1);
        for (out, row) in fb.chunks_exact_mut(stride).zip(self.rows()) {
            let mut samples = self.samples(row);
            for byte in out.iter_mut() {
                *byte = samples
                    .by_ref()
                    .take(8)
                    .enumerate()
                    .filter(|&(_, sample)| sample < half)
                    .fold(0, |acc, (i, _)| acc | (0x80 >> i));
            }
        }
        Ok(())
    }

    /// 0 = black, even x in the high nibble; true if any pixel is grey.
    pub fn to_levels(&self, out: &mut [u8]) -> Result<bool, PngError> {
        let stride = self.info.width.div_ceil(2);
        let out = out
            .get_mut(..stride * self.info.height)
            .ok_or(PngError::OutputTooSmall)?;
        let max = u16::MAX >> (16 - u16::from(self.info.bit_depth));
        let level = |s: u8| u8::try_from((u16::from(s) * 14 + max) / (2 * max)).unwrap_or(7);
        let mut grey = false;
        for (dst, row) in out.chunks_exact_mut(stride).zip(self.rows()) {
            let mut levels = self.samples(row).map(level);
            for byte in dst.iter_mut() {
                let even = levels.next().unwrap_or(0);
                let odd = levels.next().unwrap_or(0);
                grey |= [even, odd].iter().any(|l| (1..7).contains(l));
                *byte = (even << 4) | odd;
            }
        }
        Ok(grey)
    }
}

/// Conversions after this can't fail on input.
pub fn decode<'s>(
    png: &[u8],
    width: usize,
    height: usize,
    scratch: &'s mut [u8],
) -> Result<Image<'s>, PngError> {
    let body = png.strip_prefix(&SIGNATURE).ok_or(PngError::NotPng)?;
    let info = header(body)?;
    if (info.width, info.height) != (width, height) {
        return Err(PngError::SizeMismatch {
            width: info.width,
            height: info.height,
        });
    }
    let depth = usize::from(info.bit_depth);
    let stride = (width * depth).div_ceil(8);
    let len = raw_len(width, height, depth);
    let raw = scratch.get_mut(..len).ok_or(PngError::ScratchTooSmall)?;
    let idat = Chunks(body)
        .map_while(Result::ok)
        .take_while(|(kind, _)| kind != b"IEND")
        .filter(|(kind, _)| kind == b"IDAT")
        .map(|(_, data)| data);
    if miniz_oxide::inflate::decompress_slice_iter_to_slice(raw, idat, true, false) != Ok(len) {
        return Err(PngError::Inflate);
    }
    let mut prev: Option<&[u8]> = None;
    for row in raw.chunks_exact_mut(stride + 1) {
        let (filter, cur) = row.split_first_mut().ok_or(PngError::Inflate)?;
        let filter = Filter::new(*filter)?;
        match prev {
            Some(up) => filter.undo(cur, up.iter().copied()),
            None => filter.undo(cur, core::iter::repeat(0)),
        }
        prev = Some(cur);
    }
    Ok(Image { info, stride, raw })
}

fn header(body: &[u8]) -> Result<PngInfo, PngError> {
    let mut chunks = Chunks(body);
    let (kind, data) = chunks.next().ok_or(PngError::Truncated)??;
    if kind != *b"IHDR" {
        return Err(PngError::BadHeader);
    }
    let info = parse_ihdr(data)?;
    for chunk in chunks {
        match &chunk?.0 {
            b"IEND" => return Ok(info),
            b"IHDR" => return Err(PngError::BadHeader),
            // Only ancillary (lower-case) chunks may be skipped.
            b"IDAT" | [b'a'..=b'z', ..] => {}
            _ => return Err(PngError::Unsupported),
        }
    }
    Err(PngError::Truncated)
}

fn parse_ihdr(data: &[u8]) -> Result<PngInfo, PngError> {
    let &[
        w0,
        w1,
        w2,
        w3,
        h0,
        h1,
        h2,
        h3,
        bit_depth,
        colour,
        compression,
        filter,
        interlace,
    ] = <&[u8; 13]>::try_from(data)
        .ok()
        .ok_or(PngError::BadHeader)?;
    let width = usize::try_from(u32::from_be_bytes([w0, w1, w2, w3])).unwrap_or(0);
    let height = usize::try_from(u32::from_be_bytes([h0, h1, h2, h3])).unwrap_or(0);
    if width == 0 || height == 0 || compression != 0 || filter != 0 {
        return Err(PngError::BadHeader);
    }
    if colour != 0 || !matches!(bit_depth, 1 | 2 | 4 | 8) || interlace != 0 {
        return Err(PngError::Unsupported);
    }
    Ok(PngInfo {
        width,
        height,
        bit_depth,
    })
}

/// At ≤ 8 bits greyscale, "left" is one byte back.
#[derive(Clone, Copy)]
enum Filter {
    None,
    Sub,
    Up,
    Average,
    Paeth,
}

impl Filter {
    const fn new(byte: u8) -> Result<Self, PngError> {
        match byte {
            0 => Ok(Self::None),
            1 => Ok(Self::Sub),
            2 => Ok(Self::Up),
            3 => Ok(Self::Average),
            4 => Ok(Self::Paeth),
            _ => Err(PngError::BadFilter),
        }
    }

    fn undo(self, cur: &mut [u8], ups: impl Iterator<Item = u8>) {
        let (mut left, mut upleft) = (0, 0);
        for (byte, up) in cur.iter_mut().zip(ups) {
            let predicted = match self {
                Self::None => 0,
                Self::Sub => left,
                Self::Up => up,
                Self::Average => left / 2 + up / 2 + (left & up & 1),
                Self::Paeth => paeth(left, up, upleft),
            };
            *byte = byte.wrapping_add(predicted);
            (left, upleft) = (*byte, up);
        }
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    // Distances from a + b - c, unsigned.
    let pa = u16::from(b.abs_diff(c));
    let pb = u16::from(a.abs_diff(c));
    let pc = (u16::from(a) + u16::from(b)).abs_diff(2 * u16::from(c));
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// (type, data), CRC checked; stops after the first error.
struct Chunks<'a>(&'a [u8]);

impl<'a> Iterator for Chunks<'a> {
    type Item = Result<([u8; 4], &'a [u8]), PngError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let item = split_chunk(self.0);
        self.0 = item.as_ref().map_or(&[], |&(_, _, rest)| rest);
        Some(item.map(|(kind, data, _)| (kind, data)))
    }
}

type Split<'a> = ([u8; 4], &'a [u8], &'a [u8]);

fn split_chunk(buf: &[u8]) -> Result<Split<'_>, PngError> {
    let (len, rest) = buf.split_first_chunk::<4>().ok_or(PngError::Truncated)?;
    let len = usize::try_from(u32::from_be_bytes(*len)).unwrap_or(usize::MAX);
    let (kind, rest) = rest.split_first_chunk::<4>().ok_or(PngError::Truncated)?;
    let (data, rest) = rest.split_at_checked(len).ok_or(PngError::Truncated)?;
    let (crc, rest) = rest.split_first_chunk::<4>().ok_or(PngError::Truncated)?;
    if crc32(crc32(u32::MAX, kind), data) ^ u32::MAX != u32::from_be_bytes(*crc) {
        return Err(PngError::BadCrc);
    }
    Ok((*kind, data, rest))
}

fn crc32(crc: u32, bytes: &[u8]) -> u32 {
    bytes.iter().fold(crc, |crc, &byte| {
        (0..8).fold(crc ^ u32::from(byte), |c, _| {
            if c & 1 == 0 {
                c >> 1
            } else {
                0xEDB8_8320 ^ (c >> 1)
            }
        })
    })
}
