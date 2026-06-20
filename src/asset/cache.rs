//! Filesystem cache for built assets.
//!
//! Key: SHA-256 of a deterministic spec encoding.
//! Value: serialised [`Built`] (only `Emit::Bytes` assets are stored).
//! No automatic invalidation — delete the cache directory to clear.

use std::path::{Path, PathBuf};
use std::{fs, str};

use sha2::{Digest, Sha256};
use typst::foundations::Bytes;

use super::image;
use super::typst_doc::{Format as TypstFormat, TypstInput};
use super::{AssetSpec, Built, ImageSource, sha256 as sha256_bytes};
use crate::render::Emit;
use crate::resolver::Upstream;

const MAGIC: &[u8; 4] = b"TWYC";
const VERSION: u8 = 1;

/// Returns the cache key for `spec`, or `None` if the spec is trivially fast
/// or not stably serializable (File, Raw, Typst with inline Content).
pub fn cache_key(spec: &AssetSpec) -> Option<[u8; 32]> {
    let mut h = Sha256::new();
    match spec {
        AssetSpec::Sass { file, minify } => {
            h.update(b"sass\0");
            h.update(file.vpath().get_without_slash().as_bytes());
            h.update(if *minify { b"\x01" as &[u8] } else { b"\x00" as &[u8] });
        }
        AssetSpec::Image {
            source,
            width,
            height,
            fit,
            filter,
            format,
            quality,
        } => {
            h.update(b"image\0");
            match source {
                ImageSource::File(file) => {
                    h.update(b"f\0");
                    h.update(file.vpath().get_without_slash().as_bytes());
                }
                ImageSource::Bytes(bytes) => {
                    h.update(b"b\0");
                    h.update(sha256_bytes(bytes));
                }
            }
            h.update(opt_u32_key(*width));
            h.update(opt_u32_key(*height));
            h.update([fit_disc(*fit)]);
            h.update([filter_disc(*filter)]);
            h.update([img_format_disc(*format)]);
            h.update([*quality]);
        }
        AssetSpec::Typst { input, format, ppi } => match input {
            TypstInput::File(file) => {
                h.update(b"typst\0");
                h.update(file.vpath().get_without_slash().as_bytes());
                h.update(b"\0");
                h.update([typst_format_disc(*format)]);
                h.update(ppi.to_le_bytes());
            }
            TypstInput::Content(_) => return None,
        },
        // File: trivially fast (read + sha256 a file already on disk).
        // Raw: bytes are already in memory.
        _ => return None,
    }
    Some(h.finalize().into())
}

fn opt_u32_key(v: Option<u32>) -> [u8; 5] {
    match v {
        None => [0, 0, 0, 0, 0],
        Some(n) => {
            let [a, b, c, d] = n.to_le_bytes();
            [1, a, b, c, d]
        }
    }
}

fn fit_disc(f: image::Fit) -> u8 {
    match f {
        image::Fit::Contain => 0,
        image::Fit::Cover => 1,
        image::Fit::Stretch => 2,
    }
}

fn filter_disc(f: image::Filter) -> u8 {
    match f {
        image::Filter::Nearest => 0,
        image::Filter::Triangle => 1,
        image::Filter::CatmullRom => 2,
        image::Filter::Gaussian => 3,
        image::Filter::Lanczos => 4,
    }
}

fn img_format_disc(f: Option<image::Format>) -> u8 {
    match f {
        None => 0,
        Some(image::Format::Png) => 1,
        Some(image::Format::Jpeg) => 2,
        Some(image::Format::Gif) => 3,
        Some(image::Format::Webp) => 4,
        Some(image::Format::Avif) => 5,
    }
}

fn typst_format_disc(f: TypstFormat) -> u8 {
    match f {
        TypstFormat::Svg => 0,
        TypstFormat::Png => 1,
        TypstFormat::Pdf => 2,
        TypstFormat::Html => 3,
    }
}

/// Load a cached `Built` from disk. Returns `None` on any error (cache miss,
/// corrupt data, version mismatch) — the caller falls through to a full build.
pub fn load(cache_dir: &Path, key: [u8; 32]) -> Option<Built> {
    let data = fs::read(cache_path(cache_dir, key)).ok()?;
    decode(&data)
}

/// Persist a built asset. Only `Emit::Bytes` assets are stored; `Emit::Copy`
/// (plain file copies) are skipped. Failures are silently ignored.
pub fn store(cache_dir: &Path, key: [u8; 32], built: &Built) {
    let bytes = match &built.emit {
        Emit::Bytes(b) => b.clone(),
        Emit::Copy(_) => return,
    };
    let data = encode(built, &bytes);
    let path = cache_path(cache_dir, key);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, data);
}

fn cache_path(cache_dir: &Path, key: [u8; 32]) -> PathBuf {
    cache_dir.join(hex::encode(key))
}

// ---------------------------------------------------------------------------
// Binary format
// ---------------------------------------------------------------------------

fn encode(built: &Built, bytes: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&built.sha256);

    push_opt_str(&mut buf, built.ext.as_deref());
    push_opt_str(&mut buf, built.stem.as_deref());

    match built.dimensions {
        None => buf.push(0),
        Some((w, h)) => {
            buf.push(1);
            buf.extend_from_slice(&w.to_le_bytes());
            buf.extend_from_slice(&h.to_le_bytes());
        }
    }

    push_u32(&mut buf, built.upstream.len() as u32);
    for u in &built.upstream {
        push_str(&mut buf, &u.path.to_string_lossy());
    }

    push_u32(&mut buf, u32::try_from(bytes.len()).expect("payload > 4 GiB"));
    buf.extend_from_slice(bytes);
    buf
}

fn decode(data: &[u8]) -> Option<Built> {
    let mut r = Reader::new(data);

    if r.bytes(4)? != MAGIC.as_slice() {
        return None;
    }
    if r.u8()? != VERSION {
        return None;
    }
    let sha256: [u8; 32] = r.bytes(32)?.try_into().ok()?;

    let ext = r.opt_str()?;
    let stem = r.opt_str()?;

    let dimensions = if r.u8()? == 0 {
        None
    } else {
        let w = u32::from_le_bytes(r.bytes(4)?.try_into().ok()?);
        let h = u32::from_le_bytes(r.bytes(4)?.try_into().ok()?);
        Some((w, h))
    };

    let count = r.u32()? as usize;
    let mut upstream = Vec::with_capacity(count);
    for _ in 0..count {
        let s = r.str16()?;
        upstream.push(Upstream { path: PathBuf::from(s), mtime: None });
    }

    let payload = r.bytes_u32()?.to_vec();

    Some(Built {
        emit: Emit::Bytes(Bytes::new(payload)),
        upstream,
        sha256,
        ext,
        stem,
        dimensions,
    })
}

// -- write helpers ----------------------------------------------------------

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_str(buf: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    push_u16(buf, u16::try_from(b.len()).expect("string > 64 KiB"));
    buf.extend_from_slice(b);
}

fn push_opt_str(buf: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => buf.push(0),
        Some(s) => {
            buf.push(1);
            push_str(buf, s);
        }
    }
}

// -- read helpers -----------------------------------------------------------

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos + n;
        if end > self.data.len() {
            return None;
        }
        let s = &self.data[self.pos..end];
        self.pos = end;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn str16(&mut self) -> Option<&'a str> {
        let len = u16::from_le_bytes(self.bytes(2)?.try_into().ok()?) as usize;
        str::from_utf8(self.bytes(len)?).ok()
    }

    fn opt_str(&mut self) -> Option<Option<String>> {
        if self.u8()? == 0 {
            Some(None)
        } else {
            Some(Some(self.str16()?.to_string()))
        }
    }

    fn bytes_u32(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.bytes(len)
    }
}
