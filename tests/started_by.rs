#![cfg(unix)]

pub mod support;

#[path = "cli/started_by.rs"]
mod cases;
#[path = "cli/harness.rs"]
// Reuse the CLI harness on Unix without enabling its macOS-disabled suite.
#[allow(dead_code)]
mod harness;
