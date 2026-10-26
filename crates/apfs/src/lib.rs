//! Read access to Apple File System (APFS) containers.
//!
//! Written from Apple's published "Apple File System Reference". No code is
//! taken from other APFS implementations, so the crate can carry a permissive
//! license.
//!
//! This milestone is deliberately read-only: [`BlockDevice`] has no write
//! method, so nothing in this crate can modify a disk.

mod bytes;
pub mod btree;
pub mod btree_write;
pub mod check;
pub mod checksum;
pub mod decmpfs;
pub mod dev;
pub mod error;
pub mod fs;
pub mod gpt;
pub mod name_hash;
pub mod nx;
pub mod obj;
pub mod omap;
pub mod spaceman;
pub mod write;

pub use bytes::format_uuid;
pub use dev::{BlockDevice, FileDevice, ImageFile, WriteDevice};
pub use error::{Error, Result};
pub use fs::{DirEntry, Extent, FileKind, Inode, Volume};
pub use nx::Container;
