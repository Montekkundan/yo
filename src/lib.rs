//! Core library for the Yo terminal assistant.
//!
//! The binary is intentionally thin: command routing, gateway access, local
//! persistence, memory, sandboxing, diagnostics, and updates live in these
//! modules so their behavior can be exercised directly by integration tests.
//! This testable library surface is not currently a versioned Rust SDK.

pub mod cli;
pub mod commands;
pub mod config;
pub mod db;
pub mod diagnostics;
pub mod doctor;
pub mod evals;
pub mod gateway;
pub mod memory;
pub mod personalize;
pub mod render;
pub mod sandbox;
pub mod terminal;
pub mod tui;
pub mod update;
