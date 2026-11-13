//! One integration-test binary for ccid.
//!
//! Every former `tests/*.rs` file is a module here (build-efficiency rule 1),
//! so the crate and its dependency closure are linked once per test run instead
//! of once per file. Test function names and inner modules are unchanged; a
//! test path gains the former file name as its first segment (for example
//! `cache_acceptance::...`), which is what `tests/it/cache_acceptance.rs` and
//! `tests/it/native_compile_cache.rs` select in `.ci/ccid.toml`.
//!
//! A test file stays separate only for process-global state, `harness = false`
//! or a tier that must run in its own process. None of ccid's files needs any
//! of that: they share no global state, and the two `#[ignore]`d acceptance
//! tests only spawn their own subprocesses.

#![forbid(unsafe_code)]

mod advisories;
mod cache_acceptance;
mod cargo_target;
mod channel;
mod cli;
mod content_pins;
#[cfg(unix)]
mod coverage_run;
mod dependency_resolution;
#[cfg(unix)]
mod fake_forge;
mod first_party;
mod fleet;
mod float;
mod freshness;
mod job_execution;
#[cfg(unix)]
mod job_runtime;
#[cfg(unix)]
mod measure_inputs;
mod native_compile_cache;
mod prose_cli;
mod quality_cli;
#[cfg(unix)]
mod release;
#[cfg(unix)]
mod release_render;
mod render;
mod render_golden;
mod resolve_lock;
mod resolver_config;
mod retired_mirror;
mod security;
#[cfg(unix)]
mod surface;
#[cfg(unix)]
mod toolchain;
#[cfg(unix)]
mod upstream_sync;
