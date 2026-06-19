// Reference-page rendering. Everything here turns the reflected stdlib (via
// `twyla-reflect.describe`, see src/reflect.rs) into typst-reference-style HTML:
// a left table-of-contents, a code-block function signature with colored type
// pills, and a parameter list. Imported by `lib.typ`, so it's only in scope
// when the build runs with `--reflect` (which `serve-doc`/`build-doc` do).

#let describe = twyla-reflect.describe

// --- type pills -----------------------------------------------------------
//
// Each accepted type renders as a small colored chip, grouped into a handful
// of color categories. The chips are intentionally theme-independent pastels
// (dark text on a pale fill) so they read both on the light/dark page body and
// on the always-dark signature/code panels.

#let type-category(name) = {
  if name in ("none", "auto", "any") { "neutral" }
  else if name == "bool" { "bool" }
  else if name in ("int", "float", "decimal", "length", "ratio", "relative", "angle", "fraction", "duration") { "num" }
  else if name in ("str", "bytes", "label", "regex", "symbol") { "str" }
  else if name == "content" { "content" }
  else if name in ("array", "dictionary", "arguments") { "coll" }
  else if name in ("function", "module", "type") { "func" }
  else if name in ("color", "gradient", "tiling", "stroke") { "color" }
  else if name in ("datetime", "version") { "date" }
  else { "other" }
}

#let type-pill-raw(label, category: "other") = html.span(
  class: "type type-" + category,
  label,
)

#let type-pill(name) = type-pill-raw(name, category: type-category(name))

// Render a CastInfo dict (a parameter's accepted types, or a return type) as a
// run of type pills. A union becomes several adjacent pills (CSS spaces them).
#let render-type(info) = {
  if info == none {
    return
  } else if info.kind == "any" {
    type-pill("any")
  } else if info.kind == "type" {
    // repr wraps the bare-value types (`type(none)`, `type(auto)`); unwrap to
    // the plain name. Ordinary types (`int`, `str`, `content`) pass through.
    let name = repr(info.ty)
    if name.starts-with("type(") and name.ends-with(")") {
      name = name.slice(5, -1)
    }
    type-pill(name)
  } else if info.kind == "value" {
    let v = info.value
    let cat = if type(v) == str { "str" } else if type(v) in (int, float) { "num" } else { "neutral" }
    type-pill-raw(repr(v), category: cat)
  } else if info.kind == "union" {
    info.infos.map(render-type).join()
  }
}

// Drop the implicit instance parameter that method-style funcs (e.g.
// `document.url`) carry — it's machinery, not a user-facing argument.
#let visible-params(d) = d.at("params", default: ()).filter(p => p.name not in ("self", "this"))

// --- the function signature code block ------------------------------------
//
// Mirrors typst's reference: `name(arg: type, …) -> type`, params on their own
// indented lines, rendered in an always-dark code panel. `receiver` prefixes
// the name to spell out how the item is reached — `asset.` for a constructor
// (`asset.sass(..)`), `asset.*.` for a method shared by every asset type
// (`asset.*.read(..)`), `document.` for a type's own method (`document.url()`).

#let render-signature(d, receiver: "") = html.elem("pre", attrs: (class: "signature"), {
  if receiver != "" { html.span(class: "sig-recv", receiver) }
  html.span(class: "sig-fn", d.name)
  "("
  let params = visible-params(d)
  for p in params {
    "\n  "
    if p.variadic { ".." }
    html.span(class: "sig-arg", p.name)
    ": "
    render-type(p.input)
    ","
  }
  if params.len() > 0 { "\n" }
  ")"
  if d.at("returns", default: none) != none {
    " -> "
    render-type(d.returns)
  }
})

// --- one parameter --------------------------------------------------------

#let render-param(p) = html.elem("div", attrs: (class: "param"), {
  html.elem("div", attrs: (class: "param-head"), {
    html.span(class: "param-name", p.name)
    render-type(p.input)
    if "default" in p {
      html.span(class: "param-default", "= " + repr(p.default))
    }
  })
  // The baked `///` docs are twyla's own builtin comments, authored as typst
  // markup — eval as markup so prose, lists, inline code, fenced blocks, and
  // tables all render. A doc comment that fails to parse fails the build.
  if p.docs != none and p.docs != "" {
    html.elem("div", attrs: (class: "param-body"), eval(p.docs, mode: "markup"))
  }
})

// --- one reflected item (function / type / module) ------------------------
//
// Driven by the binding `name` (not `describe`'s, which modules lack), so a
// module like `asset` gets its own section heading and its members are
// namespaced under it. `prefix` builds stable, collision-free anchor ids:
// top-level items use their bare name (`document`, `asset`), nested ones are
// namespaced under their parent (`asset-file`, `asset-file-url`).

#let item-anchor(name, prefix) = if prefix == "" { name } else { prefix + "-" + name }

// The heading carries both the explicit `id` (so the TOC and external links use
// a stable, predictable anchor) and a typst `label`, so the builtins' own doc
// comments can cross-reference each other with `#link(<documents>)`. Typst's
// HTML export reuses the pre-set `id` for the label's anchor (see typst-html
// link.rs `assign`), so the two always agree.
#let ref-heading(name, anchor, depth) = {
  let head = html.elem("h" + str(calc.min(depth, 6)), attrs: (id: anchor, class: "ref-item"), html.elem("code", name))
  [#head#label(anchor)]
}

// Members are rendered in one of two styles, set by their parent:
//   • constructor — a callable item reached by a path (`asset.sass`, `document`)
//   • method      — a function called on an instance (`.url()`, `.read()`)
// `display` is the heading/TOC label, `receiver` the signature name prefix.
#let constructor-style(name, parent) = (
  display: if parent == "" { name } else { parent + "." + name },
  receiver: if parent == "" { "" } else { parent + "." },
)
#let method-style(name, receiver) = (
  display: "." + name + "()",
  receiver: receiver,
)

// `twyla-reflect.describe` only knows about native functions, types, and
// symbols. A few reference entries are plain values (currently
// `sys.twyla_version`), so render those explicitly instead of silently dropping
// them when `describe` returns `none`.
#let is-reference-value(value) = type(value) == version

#let render-value-signature(name, value) = html.elem("pre", attrs: (class: "signature"), {
  html.span(class: "sig-fn", name)
  ": "
  type-pill(repr(type(value)))
  " = "
  repr(value)
})

#let render-item(name, value, depth: 2, prefix: "", recurse-scope: true, display: none, receiver: "", docs: none) = {
  let anchor = item-anchor(name, prefix)
  let head-label = if display == none { name } else { display }

  // A bare module (e.g. `asset`): a section heading, then its members.
  if type(value) == module {
    ref-heading(head-label, anchor, depth)
    // The `asset` types share `.url()`/`.read()`: promote them to the head of
    // the section (reflected off the first type, `asset.file`, their canonical
    // source) as methods on any asset (`asset.*.`), then render each type as a
    // constructor without its (now-promoted) scope.
    let promote-scope = name == "asset"
    if promote-scope {
      let shared = describe(dictionary(value).values().first()).scope
      for (m-name, m) in dictionary(shared) {
        render-item(m-name, m, depth: depth + 1, prefix: anchor, ..method-style(m-name, "asset.*."))
      }
    }
    for (member-name, member) in dictionary(value) {
      render-item(member-name, member, depth: depth + 1, prefix: anchor,
        recurse-scope: not promote-scope, ..constructor-style(member-name, name))
    }
    return
  }

  let d = describe(value)
  if d == none {
    if is-reference-value(value) {
      ref-heading(head-label, anchor, depth)
      render-value-signature(head-label, value)
      if docs != none { docs } else { render-value-docs(name, value) }
    }
    return
  }

  ref-heading(head-label, anchor, depth)
  render-signature(d, receiver: receiver)

  if d.docs != none and d.docs != "" {
    eval(d.docs, mode: "markup")
  }

  for p in visible-params(d) {
    render-param(p)
  }

  if d.at("returns", default: none) != none {
    html.elem("div", attrs: (class: "returns"), { strong("Returns") + [ ]; render-type(d.returns) })
  }

  // Associated scope (e.g. `document.url`) — rendered as methods on this item.
  // Suppressed for the `asset` types, whose shared `.url`/`.read` are promoted
  // to the section head instead.
  if recurse-scope and d.at("scope", default: none) != none {
    for (member-name, member) in dictionary(d.scope) {
      render-item(member-name, member, depth: depth + 1, prefix: anchor, ..method-style(member-name, d.name + "."))
    }
  }
}

// --- table of contents ----------------------------------------------------
//
// Walks the same items as `render-item` to collect (name, anchor, children)
// triples, then renders them as a nested nav list linking to the anchors.

// Mirrors `render-item`'s walk, carrying the same `display` label so the TOC
// reads `.url()` for methods and `asset.file` for constructors. Takes the same
// `..constructor-style`/`..method-style` spread as `render-item`, so it also
// accepts `receiver` — unused here (the TOC renders no signature).
#let toc-entries(name, value, prefix: "", recurse-scope: true, display: none, receiver: "") = {
  let anchor = item-anchor(name, prefix)
  let children = ()
  if type(value) == module {
    // The `asset` types' shared scope is promoted to the section head (as
    // methods), and not listed under each type.
    let promote-scope = name == "asset"
    if promote-scope {
      let shared = describe(dictionary(value).values().first()).scope
      for (m-name, m) in dictionary(shared) {
        children += toc-entries(m-name, m, prefix: anchor, ..method-style(m-name, "asset.*."))
      }
    }
    // Constructors keep their bare name in the sidebar — they're already nested
    // under the module (`sass`, not `asset.sass`); the body heading qualifies.
    for (member-name, member) in dictionary(value) {
      children += toc-entries(member-name, member, prefix: anchor, recurse-scope: not promote-scope)
    }
  } else {
    let d = describe(value)
    if d == none and not is-reference-value(value) { return () }
    if d != none and recurse-scope and d.at("scope", default: none) != none {
      for (member-name, member) in dictionary(d.scope) {
        children += toc-entries(member-name, member, prefix: anchor, ..method-style(member-name, d.name + "."))
      }
    }
  }
  ((name: if display == none { name } else { display }, anchor: anchor, children: children),)
}

#let toc-list(entries) = html.elem("ul", {
  for e in entries {
    html.elem("li", {
      html.a(href: "#" + e.anchor, html.elem("code", e.name))
      if e.children.len() > 0 { toc-list(e.children) }
    })
  }
})

// `items` is an array of (name, value[, docs]) entries — the binding name plus
// the reflected builtin, with optional prose for plain value entries.
#let reference-toc(items) = {
  html.div(class: "toc-title", "Reference")
  toc-list(items.map(item => toc-entries(item.at(0), item.at(1))).join())
}

#let reference-body(items) = {
  html.elem("h1", attrs: (id: "reference"), "Reference")
  [The functions and types Twyla adds to Typst's standard library.]
  for item in items { render-item(item.at(0), item.at(1), docs: item.at(2, default: none)) }
}
