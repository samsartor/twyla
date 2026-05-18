//! `twyla-extract <selector> <input.html>` — print the inner HTML of the
//! first element matching `<selector>` to stdout. Used in the porting
//! workflow to isolate comparable regions (e.g. just the post body) before
//! handing both sides to `twyla-diff`.
//!
//! Exit codes: 0 on success, 1 if the selector matched nothing, 2 on usage
//! or I/O error.
//!
//! Selectors: `class:<token>`, `tag:<name>`. See `twyla::diff::select`.

use std::process::ExitCode;

use twyla::diff::{Selector, find_inner, parse_html, serialize_fragment};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(sel_str), Some(path)) = (args.next(), args.next()) else {
        eprintln!("usage: twyla-extract <selector> <input.html>");
        eprintln!("  selector: class:<name> | tag:<name>");
        return ExitCode::from(2);
    };

    let Some(sel) = Selector::parse(&sel_str) else {
        eprintln!("error: unrecognized selector form: {sel_str}");
        eprintln!("supported: class:<name>, tag:<name>");
        return ExitCode::from(2);
    };

    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {path}: {e}");
            return ExitCode::from(2);
        }
    };

    let doc = parse_html(&src);
    match find_inner(&doc, &sel) {
        Some(children) => {
            print!("{}", serialize_fragment(children));
            ExitCode::from(0)
        }
        None => {
            eprintln!("error: selector matched nothing: {sel_str}");
            ExitCode::from(1)
        }
    }
}
