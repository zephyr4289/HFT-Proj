#![allow(clippy::disallowed_types, clippy::disallowed_methods)]

use nf_engine::audit_stream;
use std::env;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::process;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: audit <file.itch | ->");
        process::exit(1);
    }

    let path_arg = &args[1];
    let (mut reader, path_display): (Box<dyn Read>, String) = if path_arg == "-" {
        (Box::new(BufReader::new(io::stdin())), "stdin".to_string())
    } else {
        match File::open(path_arg) {
            Ok(file) => (Box::new(BufReader::new(file)), path_arg.clone()),
            Err(e) => {
                eprintln!("Error opening file {}: {}", path_arg, e);
                process::exit(1);
            }
        }
    };

    match audit_stream(&mut reader) {
        Ok(report) => {
            report.print_report(&path_display);
            if report.violations > 0 {
                process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("I/O Error during audit: {}", e);
            process::exit(1);
        }
    }
}
