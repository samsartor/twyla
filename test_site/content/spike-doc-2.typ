// SPIKE fixture #2 — a different `extra` + draft:true, to prove the
// harvest isolates per-document (no style-chain bleed across docs).
#set document(extra: (color: "red"), draft: true)

= Spike Two

extra-color=#context document.extra.color
