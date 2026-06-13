doc2readme:
    typlite doc/readme.typ README.md

doc2pdf:
    typst compile doc/*.typ --features html --format pdf
    typst compile doc/planning.typ --features html --format pdf

serve-doc:
    cargo run --release serve --root doc/ --reflect --test-examples

build-doc:
    cargo run --release serve --root doc/ --reflect --test-examples
