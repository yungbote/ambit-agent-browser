pub mod chrome;
pub mod client;
pub mod discovery;
pub mod lightpanda;
mod pointer;
pub(crate) mod profiles;
#[cfg(test)]
mod round_trip_e2e;
#[cfg(target_os = "linux")]
mod system_theme;
pub mod types;
#[cfg(windows)]
mod windows_process;
