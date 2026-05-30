#import "/templates/page.typ": *
#set document(
  title: "Hello, twyla",
  description: "The baseline post: headings, slugs, anchors, lists, code.",
  date: datetime(year: 2026, month: 5, day: 20),
  kind: "post",
)
#show: page-template

A baseline post. Jump straight to #link(<anchors>)[the anchors section]
to see an intra-document link resolve to a fragment.

== Prose and marks <prose>

Text with #emph[emphasis], #strong[strong], and `inline code`. Smart
quotes too: "like this".

- first item
- second item
- third item

```rust
fn main() {
    println!("fenced code block");
}
```

== Anchors <anchors>

This heading is labeled, and the intro links to it. typst-html resolves
the label to this heading's slug id, so the link is fragment-only
(`#anchors`) rather than an absolutized URL.
