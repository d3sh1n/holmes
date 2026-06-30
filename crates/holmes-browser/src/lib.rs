pub mod error;
pub mod manager;
pub mod profile;

pub use error::{BrowserError, Result};
pub use manager::{action_is_read_only, ActionOutcome, BrowserManager, PageSnapshot, Screenshot};
pub use profile::profile_dir_for;
