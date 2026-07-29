//! `asset.raw` — emit in-memory text or bytes verbatim, fingerprinted and
//! content-addressed by the data itself.

use super::{AssetSpec, Built, OutputReq, emit_or_request, sha256};
use ecow::EcoString;
use typst::diag::HintedStrResult;
use typst::foundations::{Bytes, Packed, ShowFn, Str, StyleChain, cast, elem, func, scope};

/// Emit inline data verbatim as an asset.
///
/// Use [`asset.file`](super::file::FileAsset) for a path that should be read
/// and watched; `asset.raw` never touches the filesystem.
#[elem(scope, name = "raw")]
pub struct RawAsset {
    /// Text or bytes to emit verbatim.
    #[required]
    pub data: RawData,

    /// Output extension, without a leading dot. This drives the generated
    /// filename and static-server content type.
    #[default(EcoString::from("bin"))]
    pub extension: EcoString,

    /// Bundle-relative output path policy.
    #[default(OutputReq::Auto)]
    pub output: OutputReq,
}

asset_methods!(RawAsset, spec, output, None);

fn spec(elem: &Packed<RawAsset>, styles: StyleChain) -> HintedStrResult<AssetSpec> {
    Ok(AssetSpec::Raw {
        bytes: elem.data.clone().into_bytes(),
        ext: Some(elem.extension.get_cloned(styles)),
    })
}

fn output(elem: &Packed<RawAsset>, styles: StyleChain) -> HintedStrResult<OutputReq> {
    Ok(elem.output.get_cloned(styles))
}

/// Default show: a bare `asset.raw(..)` emits and renders nothing.
pub const SHOW_RULE: ShowFn<RawAsset> = |elem, engine, styles| {
    emit_or_request(
        engine,
        spec(elem, styles),
        elem.span(),
        output(elem, styles),
    )
};

/// Inline verbatim data. Unlike processing constructors, strings here are
/// data, not paths: the `raw` constructor makes that intent explicit.
#[derive(Clone, Debug, PartialEq, Hash)]
pub enum RawData {
    Str(Str),
    Bytes(Bytes),
}

impl RawData {
    fn into_bytes(self) -> Bytes {
        match self {
            Self::Str(value) => Bytes::new(value.as_str().as_bytes().to_vec()),
            Self::Bytes(value) => value,
        }
    }
}

cast! {
    RawData,
    self => match self {
        Self::Str(v) => v.into_value(),
        Self::Bytes(v) => v.into_value(),
    },
    v: Str => Self::Str(v),
    v: Bytes => Self::Bytes(v),
}

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
        elem_id: None,
    }
}
