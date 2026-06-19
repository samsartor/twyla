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

use typst::foundations::{
    Array, CastInfo, Dict, Func, IntoValue, Module, NativeParamInfo, Scope, Symbol, Type, Value,
    dict, func,
};

/// Describe a twyla builtin (function, type, or symbol) as a dict the docs
/// markup can render: `name`, `title`, `docs`, `params`, `returns`, …
///
/// Pass the value itself, e.g. `#twyla-reflect.describe(raw-html)`.
#[func]
pub fn describe(
    /// The builtin to describe — a function, type, or symbol value.
    value: Value,
) -> Option<Dict> {
    match &value {
        Value::Func(func) => Some(describe_func(func)),
        Value::Type(ty) => Some(describe_ty(ty)),
        Value::Symbol(symbol) => Some(describe_symbol(symbol)),
        _ => None,
    }
}

/// Metadata for a native function: name, title, docs, and its parameters.
fn describe_func(func: &Func) -> Dict {
    dict! {
        "name" => func.name(),
        "title" => func.title(),
        "docs" => func.docs(),
        "element" => func.to_element().is_some(),
        "contextual" => func.contextual(),
        "params" => func
            .params()
            .filter_map(|info| info.to_native().map(|n| describe_param(n).into_value()))
            .collect::<Array>(),
        "returns" => func.returns().map(describe_cast_info),
        "keywords" => func.keywords(),
        // A function's associated scope (e.g. `asset.file`) becomes a nested
        // module the page can recurse into.
        "scope" => func.scope().map(|s| Module::anonymous(s.clone())),
    }
}

/// Metadata for one parameter of a native function.
fn describe_param(param: &NativeParamInfo) -> Dict {
    let mut dict = dict! {
        "name" => param.name,
        "docs" => param.docs,
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
    dict
}

/// Metadata for a native type (e.g. twyla's `DocumentSink`).
fn describe_ty(ty: &Type) -> Dict {
    dict! {
        "short-name" => ty.short_name(),
        "long-name" => ty.long_name(),
        "title" => ty.title(),
        "docs" => ty.docs(),
        "keywords" => ty.keywords(),
        "constructor" => ty.constructor().ok(),
        "scope" => Module::anonymous(ty.scope().clone()),
    }
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
