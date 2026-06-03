//! richclip — Wayland clipboard history manager (library core).
//!
//! This crate provides the storage layer and IPC types shared by `richclip`
//! (CLI) and `richclipd` (daemon).
//!
//! # Public API
//!
//! - [`model`] — [`Item`], [`Format`], [`ItemWithFormats`].
//! - [`error`] — [`Error`], [`Result`].
//! - [`store`] — [`Store`]: the primary read/write API.
//! - [`ipc`] — newline-delimited JSON IPC types (Phase 2 networking).
//! - [`backend`] — [`ClipboardBackend`] trait (Phase 2 Wayland seam).
//! - [`paths`] — XDG path helpers.

pub mod backend;
pub mod error;
pub mod ipc;
pub mod model;
pub mod paths;
pub mod store;

// Convenience re-exports so callers don't have to spell out sub-modules.
pub use error::{Error, Result};
pub use model::{Format, Item, ItemWithFormats};
pub use store::Store;

/// Compute the blake3 hex digest of `bytes`.
///
/// Re-exported from [`store::blob_hash`] so downstream crates (e.g.
/// `richclip-wayland`) can hash bytes without adding a `blake3` dependency.
pub use store::blob_hash;
