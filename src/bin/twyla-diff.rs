//! `twyla-diff <expected.html> <actual.html>` — exits 0 if the parsed HTML
//! trees are structurally equivalent under the default normalization rules,
//! 1 if they diverge, 2 on a usage / I/O error.

use std::process::ExitCode;

use twyla::diff::{RelaxConfig, diff, parse_html};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (Some(expected_path), Some(actual_path)) = (args.next(), args.next()) else {
        eprintln!("usage: twyla-diff <expected.html> <actual.html>");
        return ExitCode::from(2);
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

    match diff(&expected, &actual, &RelaxConfig::default()) {
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
