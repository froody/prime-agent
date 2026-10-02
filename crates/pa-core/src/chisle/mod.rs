//! Chisle (a native port of JayPokale/Chisle, MIT): maximum-efficiency mode.
//!
//! - [`mode`]: the ruleset (zero-fluff prose, the YAGNI code ladder) and the
//!   natural-language toggle.
//! - [`session`]: per-session mode, the hidden rule rows each admitted turn
//!   owes, and tool-output compression at the `after_tool_call` seam.
//! - [`compress`]: the deterministic scrub/elide/dedup text transforms.
//! - [`command`]: `/chisle` (toggle, status, help, audit, review).
//! - [`config`]: `chisle.json` and the `CHISLE_*` environment overrides.

pub(crate) mod command;
pub(crate) mod compress;
pub mod config;
pub(crate) mod mode;
pub mod session;

pub use command::TASK_CUSTOM_TYPE;
pub use config::ChisleConfig;
pub use session::ChisleSession;
