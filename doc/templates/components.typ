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
// indented lines, rendered in an always-dark code panel.

#let render-signature(d) = html.elem("pre", attrs: (class: "signature"), {
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
    let flags = ()
    if p.required { flags.push("required") }
    if p.positional { flags.push("positional") }
    if p.named { flags.push("named") }
    if p.variadic { flags.push("variadic") }
    if p.settable { flags.push("settable") }
    for f in flags { html.span(class: "param-flag", f) }
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

#let render-item(name, value, depth: 2, prefix: "") = {
  let anchor = item-anchor(name, prefix)

  // A bare module (e.g. `asset`): a section heading, then its members.
  if type(value) == module {
    ref-heading(name, anchor, depth)
    for (member-name, member) in dictionary(value) {
      render-item(member-name, member, depth: depth + 1, prefix: anchor)
    }
    return
  }

  let d = describe(value)
  if d == none { return }

  ref-heading(name, anchor, depth)
  render-signature(d)

  if d.docs != none and d.docs != "" {
    eval(d.docs, mode: "markup")
  }

  for p in visible-params(d) {
    render-param(p)
  }

  if d.at("returns", default: none) != none {
    html.elem("div", attrs: (class: "returns"), { strong("Returns") + [ ]; render-type(d.returns) })
  }

  // Associated scope (e.g. `document.url`, `asset.file`'s `.url`/`.read`).
  if d.at("scope", default: none) != none {
    for (member-name, member) in dictionary(d.scope) {
      render-item(member-name, member, depth: depth + 1, prefix: anchor)
    }
  }
}

// --- table of contents ----------------------------------------------------
//
// Walks the same items as `render-item` to collect (name, anchor, children)
// triples, then renders them as a nested nav list linking to the anchors.

#let toc-entries(name, value, prefix: "") = {
  let anchor = item-anchor(name, prefix)
  let children = ()
  if type(value) == module {
    for (member-name, member) in dictionary(value) {
      children += toc-entries(member-name, member, prefix: anchor)
    }
  } else {
    let d = describe(value)
    if d == none { return () }
    if d.at("scope", default: none) != none {
      for (member-name, member) in dictionary(d.scope) {
        children += toc-entries(member-name, member, prefix: anchor)
      }
    }
  }
  ((name: name, anchor: anchor, children: children),)
}

#let toc-list(entries) = html.elem("ul", {
  for e in entries {
    html.elem("li", {
      html.a(href: "#" + e.anchor, html.elem("code", e.name))
      if e.children.len() > 0 { toc-list(e.children) }
    })
  }
})

// `items` is an array of (name, value) pairs — the binding name plus the
// reflected builtin.
#let reference-toc(items) = {
  html.div(class: "toc-title", "Reference")
  toc-list(items.map(((name, value)) => toc-entries(name, value)).join())
}

#let reference-body(items) = {
  html.elem("h1", attrs: (id: "reference"), "Reference")
  [The functions and types Twyla adds to Typst's standard library.]
  for (name, value) in items { render-item(name, value) }
}
