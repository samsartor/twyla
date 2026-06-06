// `#context`-generated documents — the pagination pattern.
//
// This page emits one extra bundle output per *post*, generated inside a
// `#context` block that reads `documents()`. These documents don't exist at
// eval time; twyla's discovery loop surfaces them by re-realizing this body
// with the `documents()` list injected, then routes each as its own output.
// Generated pages default to kind "page", so they don't re-match the filter —
// the loop settles after one round.

= Context Documents

#context for p in documents().filter(d => d.kind == "post") {
  document(output: "ctx/" + p.output, title: p.title)[
    == Context summary

    ctx-summary-for #p.title
  ]
}
