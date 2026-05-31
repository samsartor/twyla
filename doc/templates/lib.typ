#import "@preview/frame-it:2.0.0": *
#import "@preview/dtree:0.1.1": dtree

#let note = frame("Note", blue)
#let horizontalrule =context { if target() == "html" { html.hr() } else { line(length: 100%) } }

#let page-template(body) = {
	show quote.where(block: true): it => note(it.body)
	show: frame-style(styles.hint)
	show raw.where(lang: "tree"): it => if target() == "html" { it } else { dtree(raw(it.text.replace("├", " ").replace("└", " "))) }
	body
}

