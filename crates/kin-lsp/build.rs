// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Firelock, LLC

//! Embeds the content identity of Kin's enrichment producer code.
//!
//! A proof context names what made a proof: the resolver, its configuration
//! and its environment. Kin's own code that asks a language server and turns
//! its answers into call-site ledgers and relations is part of that too. When
//! it changes, a proof an earlier build recorded is no longer what this build
//! would record, and the context must say so, so the sweep asks those call
//! sites again rather than trusting the old proofs. The version dials beside
//! it are kept by hand and are not moved on every change inside one release,
//! so the identity is taken over the source bytes themselves.
//!
//! It covers the sources that decide what a proof says and no others, without
//! their tests: `build/producer_identity.rs` names every file of this crate as
//! deciding or not and says why, and this build fails on a file it does not
//! name. Inside Kin's workspace it also covers the daemon's call-site ledger,
//! which records what each answer proved. A packaged build of this crate alone
//! has no ledger beside it and covers only this crate.

use std::path::PathBuf;

#[path = "build/producer_identity.rs"]
mod producer_identity;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/producer_identity.rs");
    for path in producer_identity::watched(&manifest_dir) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    match producer_identity::identity(&manifest_dir) {
        Ok(identity) => println!("cargo:rustc-env=KIN_ENRICHMENT_PRODUCER_IDENTITY={identity}"),
        Err(problems) => {
            for problem in &problems {
                println!("cargo:warning={problem}");
            }
            panic!(
                "the enrichment producer identity cannot be taken (see \
                 build/producer_identity.rs): {}",
                problems.join("; ")
            );
        }
    }
}
