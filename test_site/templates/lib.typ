// test_site shared primitives.
//
// Deliberately independent of ~/Src/site's templates — test_site exists
// to prove twyla's primitives (bundle routing, sys.inputs.base_url, the
// raw-html resolution pass) work for a site that is NOT samsartor.com.
// Anything here that looks like a twyla feature (post enumeration,
// heading slugs, raw-html) is author-side typst, exercised end-to-end.

// Project base URL, injected by twyla via `sys.inputs.base_url`
// (`--base-url` / `TWYLA_BASE_URL`). Unset → empty → relative URLs.
#let base-url = sys.inputs.at("base_url", default: "")

#let format-date(d) = d.display("[year]-[month]-[day]")

// Plain-text of arbitrary heading content, for slug derivation.
#let text-of(body) = {
  if type(body) == str { body }
  else if body.has("text") { body.text }
  else if body.has("children") { body.children.map(text-of).join("") }
  else if body.has("body") { text-of(body.body) }
  else { "" }
}

// "Hybrid Mode!" -> "hybrid-mode". Matches the form typst-html derives
// for label-based intra-doc links, so `#link(<lbl>)` resolves to `#slug`.
#let slugify(s) = {
  let out = lower(s)
  out = out.replace(regex("[^a-z0-9]+"), "-")
  out.trim("-")
}

// Splice raw HTML through twyla's resolution pass: typst has no
// `html.raw`, so the render binary rewrites `<script
// type="x-twyla-raw-html">BODY</script>` back to inline BODY.
#let raw-html(content) = html.elem(
  "script",
  attrs: (type: "x-twyla-raw-html"),
  str(content),
)

// Inline an SVG colocated under content/, exercising both `read()` and
// the raw-html pass. (The same file is also copied as a colocated asset
// by `twyla build`.)
#let inline-svg(asset) = html.div(
  class: "figure",
  raw-html(read("/content/" + asset)),
)

// Shared show/set rules: smart quotes, external-link target, and the
// heading auto-slugifier (`= Heading` -> `<hN id="heading">`).
#let apply-base-rules(body) = {
  set smartquote(enabled: true)
  show heading: it => {
    let level = it.depth
    let slug = slugify(text-of(it.body))
    html.elem("h" + str(level), attrs: (id: slug), it.body)
  }
  body
}
