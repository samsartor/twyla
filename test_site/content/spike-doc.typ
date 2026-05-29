// SPIKE fixture — exercises the overloaded `document` element.
//
// `#set document(extra: ..)` targets twyla's element (the global
// `document` binding is shadowed); `#context document.extra` reads it
// back off the style chain via the access_field → field_from_styles
// fallback. The set + read live in the *same* file so include-scoping is
// satisfied (a sibling emitted by generate_main after the #include would
// NOT see the set — that's what the harvest pass exists for).

#set document(extra: (color: "blue", tag: "spike"))

= Spike

extra-color=#context document.extra.color
extra-tag=#context document.extra.tag
draft=#context repr(document.draft)
