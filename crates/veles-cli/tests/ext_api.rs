//! The extension API's drift guard (PLAN-extension-api.md D7).
//!
//! The published spec (`docs/extension-api.json`), its changelog
//! (`docs/extension-api-changes.md`) and the author doc
//! (`docs/extending.md`) must agree with the code and with each other.
//! A change to what a plugin can call therefore cannot pass the gate
//! without a new version, a changelog entry and updated docs: each
//! failure prints what moved and the steps that publish it.
//!
//! The docs site is checked by `scripts/check-ext-api-site.sh` in the
//! precommit gate (it lives outside this repo).

use veles_cli::extapi;

#[test]
fn the_published_spec_matches_the_code() {
    if let Err(e) = extapi::check_spec(&extapi::repo_root()) {
        panic!("{e}");
    }
}

#[test]
fn every_spec_change_has_a_changelog_entry() {
    if let Err(e) = extapi::check_changelog(&extapi::repo_root()) {
        panic!("{e}");
    }
}

#[test]
fn the_docs_match_the_spec() {
    if let Err(e) = extapi::check_docs(&extapi::repo_root()) {
        panic!("{e}");
    }
}
