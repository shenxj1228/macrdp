//! macrdp-core: unified RDP server library

pub mod bitrate_controller;
pub mod callbacks;
pub mod config;
pub mod cursor;
pub mod display;
pub mod handler;
pub mod log_bridge;
pub mod permissions;
pub mod perf_stats;
pub mod server;
pub mod tls;

pub use callbacks::*;
pub use config::{config_dir, default_config_dir, AudioConfig, ClipboardConfig, ServerConfig};
pub use log_bridge::{LogBridgeLayer, init_log_file, log_file_path};
pub use perf_stats::{new_shared as new_shared_perf_stats, SharedPerfStats};
pub use server::{start_server, start_server_with_perf, resolve_resolution, ServerHandle};
