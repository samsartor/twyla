//! `AssetSpec::Raw` — emit in-memory bytes verbatim, fingerprinted and
//! content-addressed by the bytes themselves.
//!
//! There is no user-facing `asset.raw()` constructor yet; the only producer
//! today is the native image rule ([`crate::rules`]), which routes inline /
//! byte-source images (those with no project `FileId`) here. The bytes are
//! already in memory (typst loaded them), so this just fingerprints and wraps
//! them — no World, no disk read.

use super::{Built, sha256};
use ecow::EcoString;
use typst::foundations::Bytes;

/// Wrap already-loaded bytes as a verbatim asset. No `upstream`: the bytes are
/// inline in (or derived from) the source, which the typst `World` already
/// tracks, and the spec is keyed by the bytes' content, so any change yields a
/// different spec rather than needing mtime invalidation.
pub(crate) fn build(bytes: Bytes, ext: Option<EcoString>) -> Built {
    Built {
        sha256: sha256(&bytes),
        ext: ext.map(|e| e.to_string()),
        stem: None,
        upstream: Vec::new(),
        emit: crate::render::Emit::Bytes(bytes),
        dimensions: None,
    }
}
