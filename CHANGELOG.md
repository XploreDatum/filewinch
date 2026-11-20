# Changelog

All notable changes to Filewinch are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.9.0] - 2026-11-20

The first public release.

### Added

- **Two-pane browser.** Your computer on one side, a server or cloud drive on the other, with
  back/forward history, filter, sort, in-place rename, hidden files, Copy URL, and Get Info with
  a permissions editor (octal and checkboxes kept in step).
- **SFTP:** password and keyboard-interactive sign-in, private keys (ed25519, RSA with SHA-2,
  ECDSA) with passphrases kept in the system secret store, ssh-agent (including the Windows
  OpenSSH agent and Pageant), `~/.ssh/config` (Host, HostName, User, Port, IdentityFile,
  IdentitiesOnly), and host keys checked against known_hosts, with a SHA-256 fingerprint to
  confirm for a new or changed key.
- **FTP and FTPS:** explicit and implicit TLS, passive (with the NAT workaround) and active
  mode, MLSD or Unix/IIS-style LIST listings, UTF-8 names, resumed downloads and uploads, and
  modification times and permissions where the server supports them.
- **WebDAV and WebDAV over HTTPS:** Digest (SHA-256 or MD5) and Basic sign-in, base paths in
  the address, resumed downloads.
- **Amazon S3 and S3-compatible services** (Cloudflare R2, MinIO, Wasabi, DigitalOcean Spaces,
  Backblaze B2's S3 API) with AWS Signature Version 4, and **Backblaze B2's native API**.
- **Azure Blob Storage:** account key (Shared Key), SAS tokens and URLs, anonymous public
  containers, and path-style endpoints such as Azurite.
- **OpenStack Swift and Rackspace Cloud Files:** Keystone v3 and v2, TempAuth, regions.
- **Google Drive, Dropbox, OneDrive and Box**, signed in through the browser (OAuth 2 with
  PKCE). Deleting a shared cloud folder leaves the share instead of emptying it for everyone;
  emptying it for everyone takes a separate, explicit confirmation.
- **Certificate trust:** a self-signed or otherwise untrusted TLS certificate shows its
  SHA-256 fingerprint and can be trusted for that site.
- **Transfer queue:** several jobs at once and several files per job; pause, resume, cancel
  and retry; automatic retries with backoff that resume partial files where both sides allow
  it; conflict handling (ask, overwrite, skip, keep both, resume, overwrite if newer, apply to
  all); one bandwidth limit for all transfers; live speed and time remaining.
- **Folder sync:** compare two folders by size and date, size only, or checksum; copy one way
  or mirror; optionally delete extra items; exclude patterns; review the plan item by item
  before anything changes. Comparing can be cancelled and shows its progress.
- **Drag and drop** between panes (copy, or move with a modifier), in from Finder, File
  Explorer or Files to upload, and out to Finder on macOS; Download to… everywhere.
- **Open and Edit remote files** in their app or your external editor; with Edit, each save
  is uploaded back.
- **Favorites:** saved sites in groups, search and recent connections. Passwords, passphrases
  and OAuth tokens live in the system secret store (macOS Keychain, Windows Credential
  Manager, Secret Service on Linux), never in Filewinch's files. A settings file that can't
  be read is set aside with a warning instead of being lost.
- **Settings:** transfers (concurrency, retries, bandwidth, conflicts), browsing (hidden files,
  permissions, delete confirmation), appearance and the external editor; changes apply at once
  in every window.
- **macOS, Windows and Linux:** each with its own look (the macOS system accent; Windows 11
  Fluent with the Windows accent; libadwaita, or Breeze under KDE, with the desktop's accent),
  its own keyboard shortcuts, and light and dark appearance. On Windows, file names Windows
  doesn't allow are saved under look-alike names.
- **Free to use, with an optional licence:** every feature works without registering, with no
  time limit; a slim banner offers a one-time US$14 licence (sold through Polar) that removes
  it. Licence keys activate per device (up to 5), stay valid offline, and can be deactivated
  from Settings. About Filewinch shows the version, the AGPL notice, the source link and every
  third-party licence.
- **Downloads:** a signed and notarized universal macOS disk image (macOS 13 or later), a
  Windows installer (`.msi`) and portable `.zip`, and Linux AppImages and `.tar.gz` packages
  for x86_64 and ARM64, with SHA-256 checksums.

[0.9.0]: https://github.com/XploreDatum/filewinch/releases/tag/v0.9.0
