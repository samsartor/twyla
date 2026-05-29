readme2doc:
    pandoc README.md --template=doc/template.typ -o doc/index.typ

doc2pdf:
    typst compile doc/index.typ --features html --format pdf
    typst compile doc/planning.typ --features html --format pdf
