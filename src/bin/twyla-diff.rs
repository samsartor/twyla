//! `twyla-diff <expected.html> <actual.html>` — exits 0 if the parsed HTML
//! trees are structurally equivalent under the default normalization rules,
//! 1 if they diverge, 2 on a usage / I/O error.
//!
//! Flags (prefix-positional, may appear before the two paths):
//!   --textonly-pre        relax `<pre>` blocks to text-only equality (skip span
//!                         structure / inline styles produced by syntax highlighters).
//!   --ignore-attr T:A     ignore attribute `A` on every `<T>` element. Repeat
//!                         for multiple. Example: `--ignore-attr td:style`.

use std::process::ExitCode;

use twyla::diff::{Matcher, RelaxConfig, RelaxationRule, diff, parse_html};

fn main() -> ExitCode {
    let mut cfg = RelaxConfig::new();
    let mut positional: Vec<String> = Vec::new();
    let mut args_iter = std::env::args().skip(1);

    while let Some(arg) = args_iter.next() {
        match arg.as_str() {
            "--textonly-pre" => {
                cfg = cfg.relax(Matcher::Tag("pre".to_string()), RelaxationRule::TextOnly);
            }
            "--ignore-attr" => {
                let Some(spec) = args_iter.next() else {
                    eprintln!("--ignore-attr requires a <tag>:<attr> argument");
                    return ExitCode::from(2);
                };
                let Some((tag, attr)) = spec.split_once(':') else {
                    eprintln!("--ignore-attr expects <tag>:<attr>, got {spec:?}");
                    return ExitCode::from(2);
                };
                cfg = cfg.relax(
                    Matcher::Tag(tag.to_string()),
                    RelaxationRule::IgnoreAttribute(attr.to_string()),
                );
            }
            "-h" | "--help" => {
                print_usage();
                return ExitCode::from(0);
            }
            _ if arg.starts_with("--") => {
                eprintln!("unknown flag: {arg}");
                print_usage();
                return ExitCode::from(2);
            }
            _ => positional.push(arg),
        }
    }

    let (expected_path, actual_path) = match positional.as_slice() {
        [a, b] => (a.clone(), b.clone()),
        _ => {
            print_usage();
            return ExitCode::from(2);
        }
    };

    let expected_src = match std::fs::read_to_string(&expected_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {expected_path}: {e}");
            return ExitCode::from(2);
        }
    };
    let actual_src = match std::fs::read_to_string(&actual_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {actual_path}: {e}");
            return ExitCode::from(2);
        }
    };

    let expected = parse_html(&expected_src);
    let actual = parse_html(&actual_src);

    match diff(&expected, &actual, &cfg) {
        Ok(()) => {
            println!("match: {expected_path} == {actual_path}");
            ExitCode::from(0)
        }
        Err(d) => {
            println!("{d}");
            ExitCode::from(1)
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: twyla-diff [--textonly-pre] [--ignore-attr <tag>:<attr>]... \
         <expected.html> <actual.html>"
    );
}
