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

#import "/grabber.typ": grabber-canvas, grabber-grad

// The `<head>`: charset/viewport, title + description from this page's
// `document` metadata, the Iosevka fonts, the compiled stylesheet, and the
// theme script.
#let site-head = context html.elem("head", {
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
  html.elem("link", attrs: (rel: "stylesheet", href: asset.sass("/sass/main.sass").url()))
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

// The landing-page hero: a simple proposition inside the procedural orbit,
// followed immediately by a small source → result proof.
#let hero = html.elem("section", attrs: (class: "hero"), {
  html.div(class: "orbit-field", {
    html.div(class: "orbit-ring orbit-ring-one")
    html.div(class: "orbit-ring orbit-ring-two")
    for (i, p) in (18deg, 31deg, 46deg, 62deg, 38deg, 24deg).enumerate() {
      html.div(
        class: "orbit-tool orbit-tool-" + str(i + 1),
        html.frame(grabber-canvas(
          theta: i * 43deg - 82deg,
          phi: p,
          color: grabber-grad,
        )),
      )
    }
  })
  html.div(class: "hero-copy", {
    html.elem("h1", attrs: (class: "hero-title"), [
      Everything is #html.span(class: "hero-accent", "Typst").
    ])
    html.elem("p", attrs: (class: "hero-tagline"), [
      Write static site content, templates, and logic in one expressive language.
    ])
    html.div(class: "hero-actions", {
      html.a(class: "hero-button", href: "#getting-started", [Start with one file])
      html.a(class: "hero-link", href: "/reference/", [Explore the API →])
    })
  })
  html.div(class: "hero-specimen", {
    html.div(class: "specimen-source", {
      html.div(class: "specimen-bar", {
        html.span(class: "window-dots", [● ● ●])
        html.span([content/main.typ])
      })
      html.elem("pre", attrs: (class: "specimen-code"), {
        html.span(class: "line-no", "1")
        html.span(class: "syntax-muted", "#set document(")
        "\n"
        html.span(class: "line-no", "2")
        "  "
        html.span(class: "syntax-key", "title")
        ": "
        html.span(class: "syntax-value", "\"Field Notes\"")
        ",\n"
        html.span(class: "line-no", "3")
        html.span(class: "syntax-muted", ")")
        "\n"
        html.span(class: "line-no", "4")
        "\n"
        html.span(class: "line-no", "5")
        html.span(class: "syntax-mark", "=")
        " Things worth keeping\n"
        html.span(class: "line-no", "6")
        "Written entirely in "
        html.span(class: "syntax-key", "#emph[Typst]")
        "."
      })
      html.div(class: "specimen-status", {
        html.span([twyla serve])
        html.span(class: "build-ok", [built in 42ms])
      })
    })
    html.div(class: "specimen-result", {
      html.div(class: "result-nav", {
        html.b([Field Notes])
        html.span([Archive  About])
      })
      html.div(class: "result-rule")
      html.div(class: "result-date", [Issue 04 · July 2026])
      html.div(class: "result-title", [Things worth keeping])
      html.p([Written entirely in #html.em([Typst]).])
      html.div(class: "result-tongs", html.frame(grabber-canvas(
        theta: -18deg,
        phi: 42deg,
        color: grabber-grad,
      )))
    })
  })
})
