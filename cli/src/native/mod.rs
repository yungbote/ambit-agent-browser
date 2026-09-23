#[allow(dead_code)]
pub mod a11y;
#[allow(dead_code)]
pub mod actions;
pub(crate) mod activity;
#[allow(dead_code)]
pub mod auth;
#[allow(dead_code)]
pub mod browser;
pub(crate) mod browser_control;
pub(crate) mod browser_files;
#[allow(dead_code)]
pub mod cdp;
#[allow(dead_code)]
pub mod cookies;
#[allow(dead_code)]
pub mod daemon;
#[allow(dead_code)]
pub mod diff;
pub(crate) mod display;
pub(crate) mod downloads;
#[allow(dead_code)]
pub mod element;
pub(crate) mod feedback;
pub(crate) mod input;
#[allow(dead_code)]
pub mod inspect_server;
#[allow(dead_code)]
pub mod interaction;
#[allow(dead_code)]
pub mod network;
pub(crate) mod playwright;
#[allow(dead_code)]
pub mod policy;
#[allow(dead_code)]
pub mod providers;
#[allow(dead_code)]
pub mod react;
#[allow(dead_code)]
pub mod recording;
#[allow(dead_code)]
pub mod screenshot;
pub(crate) mod selection;
#[allow(dead_code)]
pub mod snapshot;
#[allow(dead_code)]
pub mod state;
#[allow(dead_code)]
pub mod storage;
#[allow(dead_code)]
pub mod stream;
#[allow(dead_code)]
pub mod tab_binding;
#[allow(dead_code)]
pub mod tracing;
#[allow(dead_code)]
pub mod webdriver;
#[allow(dead_code)]
pub mod webmcp;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod parity_tests;

#[cfg(test)]
mod downloads_e2e_tests;

#[cfg(test)]
mod browser_files_e2e_tests;
