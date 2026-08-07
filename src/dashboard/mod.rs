pub mod event_log;
pub mod server;

#[cfg(test)]
#[path = "ui_layout_tests.rs"]
mod ui_layout_tests;

#[cfg(test)]
#[path = "quant_agent_tests.rs"]
mod quant_agent_tests;

#[cfg(test)]
#[path = "event_log_tests.rs"]
mod event_log_tests;
