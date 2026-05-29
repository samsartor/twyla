#figure(
  circle(),
  caption: [
    `scan_pages` skips `_`-prefixed stems (except `_index`), so this file is
    never compiled and no `/picture/` route is emitted. The integration test
    asserts its absence.
  ]
)
