//! Reflection builtin for twyla's own reference docs.
//!
//! Twyla defines its native builtins (`document`, `asset`, `raw-html`, …) with
//! the same `#[func]`/`#[elem]`/`#[ty]` macros typst uses, so each one already
//! carries its `///` doc comments, title, and parameter docs *baked into the
//! binary* (`Func::docs()`, `NativeParamInfo::docs`, `Type::docs()`). Nothing
//! re-parses the Rust source. This module is the bridge that hands that
//! metadata back to typst markup as plain dicts, so a `.typ` page under
//! `content/` can render the reference for twyla itself — the same trick the
//! upstream `typst-docs` crate plays with its `stdx` module.
//!
//! It is **opt-in**: [`install`] only runs when `--reflect` / `TWYLA_REFLECT`
//! is set (threaded via [`TwylaContext::reflect`](crate::project::TwylaContext)),
//! so the normal site build never sees a `twyla-reflect` module in scope. That
//! keeps twyla self-hosting: the docs site is just a twyla site built with the
//! flag on, deployable to Pages/Cloudflare like any other.
//!
//! The reference page reflects on the handful of builtins it documents
//! explicitly (`#twyla-reflect.describe(raw-html)`); there's no auto-enumeration
//! of the global scope — twyla's additions are few enough to list by hand.
//!
//! Mirrors upstream `docs/src/reflect.rs`. Deliberately omitted: `def-site` (the
//! file-path + key used for hot-reloading doc comments from the Rust sources)
//! and symbol/cast plumbing beyond what the builtins need. The baked-in `docs`
//! string is enough to render a complete reference; live reload is a follow-up.

use comemo::{Track, TrackedMut};
use typst::diag::SourceResult;
use typst::engine::Engine;
use typst::foundations::{
    Array, CastInfo, Context, Dict, Func, IntoValue, Module, NativeParamInfo, Scope, Symbol, Type,
    Value, dict, func,
};
use typst::introspection::EmptyIntrospector;
use typst::routines::SpanMode;
use typst::syntax::{Span, SyntaxMode};
use typst_utils::DefSite;

/// Evaluate a baked-in `///` doc string as markup — the same thing the
/// reference page's `eval(.., mode: "markup")` does, but in Rust, so a failure
/// can be tagged with the builtin name and its `def_site` Rust file instead of
/// surfacing as an opaque error pinned to the `eval` call site in
/// `components.typ`.
///
/// Returns evaluated content (or `none` for empty docs). This is a near-copy of
/// the stdlib `eval` builtin's body (`foundations::eval`); the only additions
/// are the empty-string short-circuit and the error-tagging `map_err`.
fn eval_docs(
    engine: &mut Engine,
    label: &str,
    def_site: Option<DefSite>,
    docs: &str,
) -> SourceResult<Value> {
    if docs.trim().is_empty() {
        return Ok(Value::None);
    }
    // SPIKE: `Uniform(detached)` means eval errors carry no real span — we lean
    // on the appended hint to point at the Rust source. The `SpanMode::Mapped`
    // variant exists precisely to map ranges back into a named file (so the
    // error would render with a snippet "at src/asset/mod.rs:45"); wiring that
    // up needs a synthetic `FileId` + `RangeMapper` and the file's text in the
    // world, which is the obvious follow-up if this proves worthwhile.
    (engine.library.routines.eval_string)(
        engine.world,
        engine.library,
        TrackedMut::reborrow_mut(&mut engine.sink),
        EmptyIntrospector.track(),
        Context::none().track(),
        docs,
        SpanMode::Uniform(Span::detached()),
        SyntaxMode::Markup,
        Scope::new(),
    )
    .map_err(|errors| {
        let origin = match def_site {
            Some(d) => format!("{} ({})", d.path, d.key),
            None => "<unknown rust source>".into(),
        };
        errors
            .into_iter()
            .map(|e| {
                e.with_hint(format!(
                    "while evaluating docs for `{label}`, defined at {origin}"
                ))
            })
            .collect()
    })
}

/// Describe a twyla builtin (function, type, or symbol) as a dict the docs
/// markup can render: `name`, `title`, `docs`, `params`, `returns`, …
///
/// Pass the value itself, e.g. `#twyla-reflect.describe(raw-html)`.
#[func]
pub fn describe(
    engine: &mut Engine,
    /// The builtin to describe — a function, type, or symbol value.
    value: Value,
) -> SourceResult<Option<Dict>> {
    Ok(match &value {
        Value::Func(func) => Some(describe_func(engine, func)?),
        Value::Type(ty) => Some(describe_ty(engine, ty)?),
        Value::Symbol(symbol) => Some(describe_symbol(symbol)),
        _ => None,
    })
}

/// Metadata for a native function: name, title, docs, and its parameters.
fn describe_func(engine: &mut Engine, func: &Func) -> SourceResult<Dict> {
    let fn_name = func.name().unwrap_or("?");
    let docs = eval_docs(
        engine,
        fn_name,
        func.def_site(),
        func.docs().unwrap_or_default(),
    )?;

    // Eval each param's docs up front (mutably borrows `engine`, so it can't sit
    // inside the `dict!` iterator closure).
    let mut params = Array::new();
    for native in func.params().filter_map(|info| info.to_native()) {
        params.push(describe_param(engine, fn_name, native)?.into_value());
    }

    Ok(dict! {
        "name" => func.name(),
        "title" => func.title(),
        "docs" => docs,
        "element" => func.to_element().is_some(),
        "contextual" => func.contextual(),
        "params" => params,
        "returns" => func.returns().map(describe_cast_info),
        "keywords" => func.keywords(),
        // A function's associated scope (e.g. `asset.file`) becomes a nested
        // module the page can recurse into.
        "scope" => func.scope().map(|s| Module::anonymous(s.clone())),
    })
}

/// Metadata for one parameter of a native function.
fn describe_param(
    engine: &mut Engine,
    fn_name: &str,
    param: &NativeParamInfo,
) -> SourceResult<Dict> {
    let docs = eval_docs(
        engine,
        &format!("{fn_name}.{}", param.name),
        param.def_site,
        param.docs,
    )?;
    let mut dict = dict! {
        "name" => param.name,
        "docs" => docs,
        "input" => describe_cast_info(&param.input),
        "positional" => param.positional,
        "named" => param.named,
        "variadic" => param.variadic,
        "required" => param.required,
        "settable" => param.settable,
    };
    // A default can be any value (including `none`), so signal "no default" by
    // absence from the dict rather than a `none` entry.
    if let Some(make) = param.default {
        dict.insert("default".into(), make());
    }
    Ok(dict)
}

/// Metadata for a native type (e.g. twyla's `DocumentSink`).
fn describe_ty(engine: &mut Engine, ty: &Type) -> SourceResult<Dict> {
    let docs = eval_docs(engine, ty.long_name(), Some(ty.def_site()), ty.docs())?;
    Ok(dict! {
        "short-name" => ty.short_name(),
        "long-name" => ty.long_name(),
        "title" => ty.title(),
        "docs" => docs,
        "keywords" => ty.keywords(),
        "constructor" => ty.constructor().ok(),
        "scope" => Module::anonymous(ty.scope().clone()),
    })
}

/// Metadata for a symbol: its variants and their values.
fn describe_symbol(symbol: &Symbol) -> Dict {
    let variants = symbol
        .variants()
        .map(|(variant, value, _deprecation)| {
            dict! { "variant" => variant.as_str(), "value" => value }.into_value()
        })
        .collect::<Array>();
    dict! { "variants" => variants }
}

/// Describe the values a parameter (or return) accepts. Lets the docs render
/// the accepted-types line under each parameter.
fn describe_cast_info(info: &CastInfo) -> Dict {
    match info {
        CastInfo::Any => dict! { "kind" => "any" },
        CastInfo::Value(value, details) => dict! {
            "kind" => "value",
            "value" => value.clone(),
            "details" => *details,
        },
        CastInfo::Type(ty) => dict! { "kind" => "type", "ty" => *ty },
        CastInfo::Union(infos) => dict! {
            "kind" => "union",
            "infos" => infos.iter().map(describe_cast_info).map(Value::Dict).collect::<Array>(),
        },
    }
}

/// Install the `twyla-reflect` module (exposing [`describe`]) into the global
/// scope. Called from [`crate::render::install_stdlib`] only under the
/// `--reflect` opt-in.
pub fn install(global: &mut Scope) {
    let mut scope = Scope::new();
    scope.define_func::<describe>();
    global.define("twyla-reflect", Module::new("twyla-reflect", scope));
}
