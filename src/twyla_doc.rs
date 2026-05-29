//! SPIKE — overloading `document` with a twyla element.
//!
//! Proves two things the `pages`/`documents` design hinges on:
//! 1. A custom `#[elem]` can be defined *outside* the typst crates and
//!    bound to the global identifier `document`, shadowing the native
//!    element — while the native `DocumentElem` stays reachable (rebound
//!    as `__std_document`) so `generate_main` can still emit it as the
//!    routing primitive (bundle routing keys on the native type via
//!    `to_packed::<DocumentElem>()`, not the binding name).
//! 2. The custom element's fields are settable (`#set document(extra: …)`)
//!    and so flow onto the style chain — making them both contextually
//!    readable on the current page (`#context document.extra`, via the
//!    `access_field` → `field_from_styles` fallback) and harvestable from
//!    each document body's resolved style chain (see `crate::harvest`).
//!
//! For the spike the field surface is intentionally tiny: `extra` (the
//! hard arbitrary-`Value` case) and `draft`. Standard fields (title/date/
//! description) are left to the native element for now; the real design
//! decides whether they migrate here too.

use typst::foundations::{Value, elem};

/// Twyla's overload of the `document` identifier. Carries twyla-only page
/// metadata on the style chain. Routing is unaffected — `generate_main`
/// constructs the native element via the rebound `__std_document`.
#[elem(name = "document")]
pub struct TwylaDocument {
    /// Arbitrary user data, surfaced per-page (`page.extra`).
    #[default(Value::None)]
    pub extra: Value,

    /// Whether this page is a draft (built, excluded from listings/feed).
    #[default(false)]
    pub draft: bool,
}
