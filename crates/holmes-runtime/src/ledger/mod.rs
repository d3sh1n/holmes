//! Runtime-facing Hypothesis Ledger v2 facade.
//!
//! Phase 1 keeps the reducer in `holmes-core` so the SQLite store and runtime
//! share one pure implementation. Runtime control actions and validators are
//! added behind this facade in later phases.

pub use holmes_core::ledger::{
    apply_ledger_event, reduce_ledger_events, LedgerReduceError, LedgerSnapshot,
};

pub mod commit;
pub mod resolution_verifier;
pub mod validator;
