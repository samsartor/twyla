#let code = ```typst
= Hello, World

This is typst #emoji.hand.wave
```

#side-by-side[
  #code
][
  #eval(
    code.text,
    mode: "markup",
    scope: (side-by-side: side-by-side)
  )
]
