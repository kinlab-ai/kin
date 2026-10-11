// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Print the functions of a product's deciding sources that no deciding code
//! names, as `[[excluded_items]]` groups a person can review and paste.
//!
//! cargo run -p kin-derivation-inputs --example suggest_exclusions -- <composer crate dir> [product]

fn main() {
    let mut args = std::env::args().skip(1);
    let composer = args.next().expect("the composer crate's directory");
    let product = args.next().unwrap_or_else(|| "graph".to_string());
    match kin_derivation_inputs::suggest_exclusions(std::path::Path::new(&composer), &product) {
        Ok(suggested) => {
            for (input, paths) in suggested {
                println!("# {input}");
                println!("items = [");
                for path in paths {
                    println!("    {path:?},");
                }
                println!("]\n");
            }
        }
        Err(problems) => {
            for problem in problems {
                eprintln!("{problem}");
            }
            std::process::exit(1);
        }
    }
}
