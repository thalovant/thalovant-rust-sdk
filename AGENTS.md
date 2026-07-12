# Repository instructions

This repository owns the published Rust client and agent SDK crate for supported Thalovant public API and HiveMind runtime contracts. Read the platform contracts in `../infra-manifests/docs/thalovant-platform/` when available.

Rules:

- Preserve semantic-version compatibility with the documented Rust and Thalovant API support window.
- Update public types, implementation, examples, tests, changelog, version, and public documentation together for observable contract changes.
- Consume additive server behavior only after compatible server support exists.
- Never publish credentials, crates.io tokens, identity files, or generated secrets.
- Do not create a release for internal platform changes with no Rust SDK impact; record `no SDK impact` in the coordinated change instead.
- Validate package contents and an install from crates.io before declaring a release complete.
- Update affected `docs.thalovant.com` SDK pages in the same release train.

Validate with `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --all-features`, and `cargo package --allow-dirty --list`. A published release also requires a clean crate to resolve `thalovant@<version>` from crates.io and pass `cargo check`.

Rollback by yanking the broken crate version when necessary and publishing a corrected patch release. Never replace an existing crate artifact.
