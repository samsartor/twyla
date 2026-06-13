#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "Reference")

// Auto-generated reference. Everything below is reflected out of the binary
// via `twyla-reflect.describe` (see src/reflect.rs) — name, title, the baked
// `///` docs, parameters, and return type. No hand-written prose yet; this is
// a raw dump of the stdlib additions to start turning into real docs.
//
// Build with the `--reflect` / `TWYLA_REFLECT` opt-in, otherwise the
// `twyla-reflect` module is not in scope.

#let describe = twyla-reflect.describe

// Render a CastInfo dict (a parameter's accepted types, or a return type) as
// a short inline type expression.
#let render-type(info) = {
  if info == none {
    return
  } else if info.kind == "any" {
    raw("any")
  } else if info.kind == "type" {
    // repr wraps the bare-value types (`type(none)`, `type(auto)`); unwrap to
    // the plain name. Ordinary types (`int`, `str`, `content`) pass through.
    let name = repr(info.ty)
    if name.starts-with("type(") and name.ends-with(")") {
      name = name.slice(5, -1)
    }
    raw(name)
  } else if info.kind == "value" {
    raw(repr(info.value))
  } else if info.kind == "union" {
    info.infos.map(render-type).join([ or ])
  }
}

// Render one parameter dict. Built in code mode so a default like `= auto`
// is never misparsed as a heading, and the doc string is inserted verbatim.
#let render-param(p) = {
  let flags = ()
  if p.required { flags.push("required") }
  if p.positional { flags.push("positional") }
  if p.named { flags.push("named") }
  if p.variadic { flags.push("variadic") }
  if p.settable { flags.push("settable") }

  let sig = strong(raw(p.name))
  sig += [ #render-type(p.input)]
  if "default" in p {
    sig += [ (default: #raw(repr(p.default)))]
  }
  if flags.len() > 0 {
    sig += [ — #emph(flags.join(", "))]
  }

  block(inset: (left: 1.2em), {
    sig
    if p.docs != none and p.docs != "" {
      parbreak()
      eval(p.docs, mode: "markup")
    }
  })
}

// Render a function/type/symbol value. Modules (and associated scopes such as
// `asset.file`'s `.url`/`.read`) are recursed into.
#let render-item(value, depth: 2) = {
  if type(value) == module {
    for (name, member) in dictionary(value) {
      render-item(member, depth: depth)
    }
    return
  }

  let d = describe(value)
  if d == none {
    return
  }

  heading(level: depth, raw(d.name))

  // The baked `///` docs are twyla's own builtin comments — authored as typst
  // markup, so eval them as markup to render prose, lists, inline code, and
  // fenced code blocks. (Unlike upstream typst-docs, there are no `$func$`
  // reference cross-refs to special-case here.) A doc comment that fails to
  // parse fails the build, which keeps the docs honest.
  if d.docs != none and d.docs != "" {
    eval(d.docs, mode: "markup")
  }

  if d.at("params", default: ()).len() > 0 {
    for p in d.params {
      render-param(p)
    }
  }

  if d.at("returns", default: none) != none {
    block[Returns #render-type(d.returns)]
  }

  if d.at("scope", default: none) != none {
    render-item(d.scope, depth: depth + 1)
  }
}

= Reference
<reference>

#render-item(document)
#render-item(documents)
#render-item(asset)
#render-item(raw-html)
#render-item(plain-text)
