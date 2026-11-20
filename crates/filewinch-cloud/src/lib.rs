//! Filewinch's cloud drives: Google Drive, Dropbox, Microsoft OneDrive and Box.
//!
//! Every drive signs in through the browser with OAuth 2 (Authorization Code + PKCE on a
//! loopback redirect, see [`oauth`]) and maps Filewinch paths ([`filewinch_core::RPath`]) onto
//! the provider's own items. Each module exports a connector the app puts in its
//! `Protocol -> &dyn Connector` table:
//!
//! | Module | Connector | Protocol |
//! |---|---|---|
//! | [`gdrive`] | [`GoogleDriveConnector`] | `GoogleDrive` |
//! | [`dropbox`] | [`DropboxConnector`] | `Dropbox` |
//! | [`onedrive`] | [`OneDriveConnector`] | `OneDrive` |
//! | [`boxcom`] | [`BoxConnector`] | `Box` |
//!
//! Providers that rotate refresh tokens (Box always, Microsoft sometimes) hand back a new one
//! while a session runs; the session saves it through the [`filewinch_core::Secrets`] it was
//! connected with (the system secret store in the app).

mod cache;
mod http;
mod setup;
mod time;
mod upload;

pub mod boxcom;
pub mod dropbox;
pub mod gdrive;
pub mod oauth;
pub mod onedrive;

pub use boxcom::BoxConnector;
pub use dropbox::DropboxConnector;
pub use gdrive::GoogleDriveConnector;
pub use oauth::{CancelToken, ClientConfig, Provider, SignInOptions, SignedIn, sign_in};
pub use onedrive::OneDriveConnector;
pub use setup::{ConnectorOptions, Endpoints, Tuning};
