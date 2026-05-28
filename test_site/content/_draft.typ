#import "/templates/page.typ": *
#show: page-template.with(
  path: "draft",
  title: "Underscore-prefixed draft",
  description: "Never scanned, never compiled, never routed.",
  date: datetime(year: 2026, month: 5, day: 28),
)

`scan_pages` skips `_`-prefixed stems (except `_index`), so this file is
never compiled and no `/draft/` route is emitted. The integration test
asserts its absence.
