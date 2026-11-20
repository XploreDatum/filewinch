//! Which protocols this build can open: one connector per backend crate, each behind a cargo
//! feature of the same name (see Cargo.toml). Adding a backend is one optional dependency and
//! feature in Cargo.toml and one line here.

use std::sync::OnceLock;

use filewinch_core::{Connector, Protocol};

fn connectors() -> &'static [Box<dyn Connector>] {
    static ALL_CONNECTORS: OnceLock<Vec<Box<dyn Connector>>> = OnceLock::new();
    ALL_CONNECTORS.get_or_init(|| {
        vec![
            Box::new(filewinch_local::LocalConnector),
            #[cfg(feature = "sftp")]
            Box::new(filewinch_sftp::SftpConnector),
            #[cfg(feature = "ftp")]
            Box::new(filewinch_ftp::FtpConnector),
            #[cfg(feature = "webdav")]
            Box::new(filewinch_webdav::WebDavConnector),
            #[cfg(feature = "s3")]
            Box::new(filewinch_s3::S3Connector),
            #[cfg(feature = "azure")]
            Box::new(filewinch_azure::AzureConnector),
            #[cfg(feature = "swift")]
            Box::new(filewinch_swift::SwiftConnector),
            #[cfg(feature = "cloud")]
            Box::new(filewinch_cloud::GoogleDriveConnector::new()),
            #[cfg(feature = "cloud")]
            Box::new(filewinch_cloud::DropboxConnector::new()),
            #[cfg(feature = "cloud")]
            Box::new(filewinch_cloud::OneDriveConnector::new()),
            #[cfg(feature = "cloud")]
            Box::new(filewinch_cloud::BoxConnector::new()),
        ]
    })
}

/// The connector for `p`, if this build includes its backend.
pub fn connector(p: Protocol) -> Option<&'static dyn Connector> {
    connectors().iter().map(|c| &**c).find(|c| c.protocols().contains(&p))
}

pub fn available(p: Protocol) -> bool {
    connector(p).is_some()
}

/// Every protocol the connect sheet offers, in the order it lists them.
pub const ALL: [Protocol; 13] = [
    Protocol::Sftp,
    Protocol::Ftp,
    Protocol::Ftps,
    Protocol::WebDav,
    Protocol::WebDavs,
    Protocol::S3,
    Protocol::B2,
    Protocol::AzureBlob,
    Protocol::Swift,
    Protocol::GoogleDrive,
    Protocol::Dropbox,
    Protocol::OneDrive,
    Protocol::Box,
];
