#![cfg(unix)]

pub mod support;

#[path = "cli/input_activity.rs"]
mod cases;
#[path = "cli/harness.rs"]
// Reuse the headless CLI harness without enabling the macOS-disabled suite.
#[allow(dead_code, unused_imports)]
mod harness;
