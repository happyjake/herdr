#![cfg(unix)]

pub mod support;

#[path = "cli/debrief.rs"]
mod cases;
#[path = "cli/harness.rs"]
#[allow(dead_code, unused_imports)] // Shared headless-server harness.
mod harness;
