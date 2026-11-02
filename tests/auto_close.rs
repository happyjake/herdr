#![cfg(unix)]

pub mod support;

#[path = "cli/auto_close.rs"]
mod cases;
#[path = "cli/harness.rs"]
// Reuse the CLI harness without enabling its macOS-disabled suite.
#[allow(dead_code, unused_imports)]
mod harness;
