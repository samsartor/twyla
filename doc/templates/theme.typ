#import "@preview/frame-it:2.0.0": *
#import "/grabber.typ": grabber-canvas, grabber-grad
#import "boxdraw.typ": boxdraw

// Site chrome for the Twyla docs: the `<head>`, the top header bar (brand +
// nav + theme toggle), the footer, and the home-page hero. The page templates
// in `lib.typ` stitch these around a body.
//
// Theming is plain modern CSS: `:root { color-scheme: light dark }` gives
// system detection for free, colors use `light-dark()`, and the toggle below
// just pins `color-scheme` on <html> (persisted in localStorage). No build
// step and no flash-of-wrong-theme — the early script runs before first paint.

// Inline early-applied theme script. Defines the toggle and restores a saved
// choice before paint. Kept free of `<`, `>`, and `&` so it needs no escaping
// or raw-html wrapper.
#let theme-script = "const KEY = 'twyla-theme';
function twylaToggleTheme() {
  const root = document.documentElement;
  const explicit = root.style.colorScheme;
  const dark = explicit ? explicit === 'dark' : matchMedia('(prefers-color-scheme: dark)').matches;
  const next = dark ? 'light' : 'dark';
  root.style.colorScheme = next;
  try { localStorage.setItem(KEY, next); } catch (e) {}
}
try {
  const saved = localStorage.getItem(KEY);
  if (saved) document.documentElement.style.colorScheme = saved;
} catch (e) {}"

// The `<head>`: charset/viewport, title + description from this page's
// `document` metadata, the Iosevka fonts, the compiled stylesheet, and the
// theme script.
#let site-head(slides: false) = context html.elem("head", {
  html.elem("meta", attrs: (charset: "utf-8"))
  html.elem("meta", attrs: (name: "viewport", content: "width=device-width, initial-scale=1"))

  let title = document.title
  html.elem("title", if title == none { "Twyla" } else { [#title — Twyla] })
  if document.description != none {
    html.elem("meta", attrs: (name: "description", content: plain-text(document.description)))
  }
  context html.link(
    rel: "icon",
    type: "image/x-icon",
    href: asset.typst([
      #set page(width: auto, height: auto, fill: none, margin: 0mm)
      #let thing = grabber-canvas(
        theta: -40deg,
        phi: 40deg,
        color: grabber-grad,
        rad: 0.5,
        hw: 0.5,
        ht: 3mm,
        st: 4mm,
        gt: 3mm
      )
      #context rect(fill: black, height: auto, width: auto, inset: 5mm, radius: 20mm, thing)
      
    ], format: "svg", output: "/favicon.svg").url(),
  )
  html.elem("link", attrs: (rel: "stylesheet", href: "https://bin.samsartor.com/iosevka_27.3.3/iosevka.css"))
  html.elem("link", attrs: (rel: "stylesheet", href: "https://bin.samsartor.com/iosevka_27.3.3/iosevka-aile.css"))
  // Slides use a self-contained stylesheet (it vendors the base/prose/boxdraw
  // rules it needs from main.sass) so that ongoing work on the docs' main.sass
  // can't silently break the presentation. Normal pages get main.sass.
  if slides {
    html.elem("link", attrs: (rel: "stylesheet", href: asset.sass("/sass/slides.sass").url()))
  } else {
    html.elem("link", attrs: (rel: "stylesheet", href: asset.sass("/sass/main.sass").url()))
  }
  html.elem("script", theme-script)
  html.elem("script", attrs: (data-goatcounter: "https://twyla.goatcounter.com/count", async: "", src: "//gc.zgo.at/count.js"))
})

#let nav-link(href, label, ..args) = html.a(class: "site-nav-link", href: href, ..args, label)

// The top header bar: brand, nav links, and the theme toggle.
#let site-header = html.elem("header", attrs: (class: "site-header"), {
  html.a(class: "site-brand", href: "/")[Twyla]
  html.elem("nav", attrs: (class: "site-nav"), {
    nav-link("/", "Guide")
    nav-link("/reference/", "Reference")
    nav-link("https://github.com/samsartor/twyla", "GitHub", target: "_blank")
    html.elem(
      "button",
      attrs: (
        class: "theme-toggle",
        type: "button",
        onclick: "twylaToggleTheme()",
        "aria-label": "Toggle color theme",
      ),
      html.span(class: "theme-toggle-icon", "◐"),
    )
  })
})

#let site-footer = html.elem("footer",
  attrs: (class: "site-footer"),
  [Built with #html.a(href: "https://github.com/samsartor/twyla")[Twyla]]
)

// The home-page hero banner.
#let hero = html.elem("section", attrs: (class: "hero"), {
  html.div(class: "heroicon", for p in (70deg, 40deg, 20deg) {
    html.frame(grabber-canvas(
      theta: 0deg,
      phi: p,
      color: grabber-grad,
    ))
  })
  html.elem("h1", attrs: (class: "hero-title"), "Twyla")
  html.elem("p", attrs: (class: "hero-tagline"), [The static site generator where everything is Typst.])
})

#let note = frame("Note", blue)

// Content show rules shared by every page: callout quotes, the `tree` and
// `example` code fences, and the (always-dark) syntax theme for code blocks.
//
// TODO: code-block colors are baked in as inline `<span style="color:…">` by
// typst's HTML export (see typst-html html_span_filled), so they can't follow
// the light/dark toggle. We pin code panels to a dark theme for now; revisit
// once twyla ships its own CSS-class-based `raw` show rule.
#let content-rules(body) = {
  show quote.where(block: true): it => note(it.body)
  show raw.where(lang: "tree"): it => boxdraw(it.text)
  show raw.where(lang: "boxdraw"): it => boxdraw(it.text)
  show: frame-style(styles.hint)
  // `--test-examples`: compile-check each `example` block and highlight as typst.
  show raw.where(lang: "example"): twyla-examples.compile-example
  set raw(theme: "/templates/Dracula.tmTheme")
  body
}
