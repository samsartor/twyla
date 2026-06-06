#import "/templates/lib.typ": page-template
#show: page-template
#set document(title: "document")

= document
<document>

Twyla overloads Typst's `document` element to carry per-page metadata. Set it
once, near the top of a page, with a `set` rule:

```typ
#set document(
  title: "Rewriting My Blog",
  date: datetime(year: 2026, month: 4, day: 12),
  description: [Why I moved off Markdown.],
  kind: "post",
)
```

Every field is then readable on this page inside a `#context` block (e.g.
`#context document.title`), and the whole site's metadata is available through
`documents()`.

== Fields
<fields>

/ title: The page's title (content).
/ date: The publication date (a `datetime`).
/ description: A short description or summary (content).
/ kind: What kind of page this is — used to group pages in listings and to
  pick the `KIND-template` a `convert` draft shows. If unset, it defaults from
  the source filename: `"root"` for `content/main.typ`, `"dir"` for
  `content/<dir>/main.typ`, and `"page"` for anything else.
/ extra: Arbitrary data for your own use, available as `document.extra` and as
  each entry's `extra` in `documents()`.
/ draft: Whether the page is a draft (`false` by default). Drafts are still
  built, but are conventionally excluded from listings and feeds.
/ output: The output location, e.g. `"foo/index.html"`. Overrides the route
  Twyla derives from the filename.

== documents()
<documents>

`documents()` returns the whole site's metadata: an array with one dictionary
per page. It is contextual — call it inside `#context`.

```typ
#context for doc in documents() {
  if doc.kind == "post" and not doc.draft [
    == #link(doc.url, doc.title)
    #doc.date.display()

    #doc.description
  ]
}
```

Each entry carries `url` (the page's user-facing URL), `output` (the written
location, e.g. `"hello/index.html"`), and the metadata the page set:
`title`, `date`, `description`, `kind`, `draft`, and `extra`.
