//! ai-commander library crate: the TUI binary, the headless modes and the
//! crash-test helper all share these modules.

// v5 #9: no unwrap() in production code (tests are exempt)
#![cfg_attr(not(test), deny(clippy::unwrap_used))]

pub mod backend;
pub mod config;
pub mod i18n;
pub mod kanban;
pub mod logging;
pub mod policy;
pub mod ui;
