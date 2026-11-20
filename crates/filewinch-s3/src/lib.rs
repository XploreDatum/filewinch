//! Filewinch's object-store backends.
//!
//! - [`S3Connector`] ([`Protocol::S3`](filewinch_core::Protocol::S3)): Amazon S3 and
//!   S3-compatible endpoints (Cloudflare R2, MinIO, Wasabi, DigitalOcean Spaces, Backblaze B2's
//!   S3 API), signed with AWS Signature Version 4.
//! - [`B2Connector`] ([`Protocol::B2`](filewinch_core::Protocol::B2)): Backblaze B2's native API.
//!
//! Both are flat stores: the root lists buckets, folders are key prefixes, and `mkdir` writes
//! a marker (`name/` on S3, `name/.bzEmpty` on B2). Server-side copy is supported; rename is
//! not (the engine copies, then deletes). Downloads resume; uploads restart.

mod b2;
mod body;
mod checksum;
#[cfg(test)]
mod mock;
mod s3;
mod sigv4;
mod time;
mod xml;

pub use b2::{B2Connector, B2Remote};
pub use s3::{S3Connector, S3Remote};
