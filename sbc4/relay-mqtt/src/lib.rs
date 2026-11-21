//! relay-mqtt: bridges `rack/relay/<ch>/set` on the Slate broker to
//! rack_controller's local Unix socket and publishes the retained relay state.
#![forbid(unsafe_code)]

pub mod bridge;
pub mod config;
pub mod controller;
pub mod fake_controller;
pub mod logging;
pub mod mqtt;
pub mod protocol;
pub mod worker;
