//! Filewinch's own icon set (16-point grid, drawn for this app), and the file-type mapping.

use filewinch_core::{EntryKind, Protocol};
use rune::{Color, Icon};

macro_rules! icon {
    ($name:ident, $id:literal, $body:literal) => {
        pub static $name: Icon = Icon { name: concat!("fw-", $id), view: 16.0, body: $body };
    };
}

icon!(FOLDER, "folder", r#"<path d="M1.5 4.25C1.5 3.56 2.06 3 2.75 3H6.1L7.6 4.5H13.25C13.94 4.5 14.5 5.06 14.5 5.75V12C14.5 12.69 13.94 13.25 13.25 13.25H2.75C2.06 13.25 1.5 12.69 1.5 12V4.25Z" fill="currentColor"/>"#);
icon!(FILE, "file", r#"<path d="M4 1.75H9.5L12.75 5V14.25H4V1.75Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/><path d="M9.25 1.9V5.25H12.6" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#);
icon!(FILE_TEXT, "file-text", r#"<path d="M4 1.75H9.5L12.75 5V14.25H4V1.75Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/><path d="M6 8H10.75M6 10.25H10.75M6 12.5H9" stroke="currentColor" stroke-width="1.1" stroke-linecap="round"/>"#);
icon!(FILE_IMAGE, "file-image", r#"<rect x="2" y="2.75" width="12" height="10.5" rx="1.5" stroke="currentColor" stroke-width="1.2" fill="none"/><circle cx="5.75" cy="6.25" r="1.25" fill="currentColor"/><path d="M2.5 12L6.5 8.5L9 10.75L11 9L13.5 11.25" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#);
icon!(FILE_CODE, "file-code", r#"<path d="M4 1.75H9.5L12.75 5V14.25H4V1.75Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/><path d="M7 8L5.75 9.5L7 11M9.75 8L11 9.5L9.75 11" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(FILE_ARCHIVE, "file-archive", r#"<rect x="2.5" y="2" width="11" height="12" rx="1.5" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 2V4M8 5V6M8 7V8" stroke="currentColor" stroke-width="1.4"/><rect x="6.75" y="9" width="2.5" height="2.75" rx=".6" fill="currentColor"/>"#);
icon!(FILE_MEDIA, "file-media", r#"<rect x="2" y="2.75" width="12" height="10.5" rx="1.5" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M6.5 5.75V10.25L10.25 8L6.5 5.75Z" fill="currentColor"/>"#);
icon!(LINK, "link", r#"<path d="M6.75 9.25L9.25 6.75M5.5 7.5L4.25 8.75A2.12 2.12 0 0 0 7.25 11.75L8.5 10.5M10.5 8.5L11.75 7.25A2.12 2.12 0 0 0 8.75 4.25L7.5 5.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" fill="none"/>"#);

icon!(BACK, "back", r#"<path d="M10 3.5L5.5 8L10 12.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(FORWARD, "forward", r#"<path d="M6 3.5L10.5 8L6 12.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(UP, "up", r#"<path d="M8 13V3.5M3.75 7.5L8 3.25L12.25 7.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(REFRESH, "refresh", r#"<path d="M12.75 8A4.75 4.75 0 1 1 11.3 4.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/><path d="M11.75 1.75V4.9H8.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(MINUS, "minus", r#"<path d="M3 8H13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#);
icon!(PLUS, "plus", r#"<path d="M8 3V13M3 8H13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#);
icon!(SEARCH, "search", r#"<circle cx="7" cy="7" r="4.25" stroke="currentColor" stroke-width="1.4" fill="none"/><path d="M10.25 10.25L13.5 13.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#);
icon!(TRANSFERS, "transfers", r#"<path d="M5 13V3M2.5 5.5L5 3L7.5 5.5M11 3V13M8.5 10.5L11 13L13.5 10.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
// Transfer directions: up out of the Mac's tray, down into it, across between two servers.
icon!(UPLOAD, "upload", r#"<path d="M8 10.25V2.75M4.75 6L8 2.75L11.25 6M2.75 10V12.5C2.75 13.05 3.2 13.5 3.75 13.5H12.25C12.8 13.5 13.25 13.05 13.25 12.5V10" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(DOWNLOAD, "download", r#"<path d="M8 2.75V10.25M4.75 7L8 10.25L11.25 7M2.75 10V12.5C2.75 13.05 3.2 13.5 3.75 13.5H12.25C12.8 13.5 13.25 13.05 13.25 12.5V10" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(ACROSS, "across", r#"<path d="M2.75 5.25H12.5M10 2.75L12.5 5.25L10 7.75M13.25 10.75H3.5M6 8.25L3.5 10.75L6 13.25" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(TRASH, "trash", r#"<path d="M2.75 4.25H13.25M6.25 4.25V2.75H9.75V4.25M4.25 4.25L5 13.25H11L11.75 4.25M6.75 6.75V10.75M9.25 6.75V10.75" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(SYNC, "sync", r#"<path d="M2.75 6.5A5.25 5.25 0 0 1 12.6 5M13.25 9.5A5.25 5.25 0 0 1 3.4 11" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/><path d="M12.75 2.25V5.25H9.75M3.25 13.75V10.75H6.25" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(GEAR, "gear", r#"<circle cx="8" cy="8" r="2.25" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M8 1.75V3.5M8 12.5V14.25M1.75 8H3.5M12.5 8H14.25M3.6 3.6L4.8 4.8M11.2 11.2L12.4 12.4M3.6 12.4L4.8 11.2M11.2 4.8L12.4 3.6" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#);
icon!(SERVER, "server", r#"<rect x="2" y="2.5" width="12" height="4.5" rx="1.25" stroke="currentColor" stroke-width="1.2" fill="none"/><rect x="2" y="9" width="12" height="4.5" rx="1.25" stroke="currentColor" stroke-width="1.2" fill="none"/><circle cx="4.75" cy="4.75" r=".9" fill="currentColor"/><circle cx="4.75" cy="11.25" r=".9" fill="currentColor"/>"#);
icon!(CLOUD, "cloud", r#"<path d="M4.5 12.5H11.75A2.75 2.75 0 0 0 12.1 7.03A4 4 0 0 0 4.4 6.6A3 3 0 0 0 4.5 12.5Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="none"/>"#);
icon!(BUCKET, "bucket", r#"<ellipse cx="8" cy="4" rx="5.5" ry="1.75" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M2.5 4L3.75 12.25C3.9 13.1 5.8 13.75 8 13.75C10.2 13.75 12.1 13.1 12.25 12.25L13.5 4" stroke="currentColor" stroke-width="1.2" fill="none"/>"#);
icon!(LAPTOP, "laptop", r#"<rect x="3" y="3" width="10" height="7.25" rx="1" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M1.5 12.5H14.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/>"#);
icon!(PAUSE, "pause", r#"<path d="M5.5 3.5V12.5M10.5 3.5V12.5" stroke="currentColor" stroke-width="2" stroke-linecap="round"/>"#);
icon!(PLAY, "play", r#"<path d="M5 3.25V12.75L12.5 8L5 3.25Z" fill="currentColor"/>"#);
icon!(STOP, "stop", r#"<path d="M4.5 4.5L11.5 11.5M11.5 4.5L4.5 11.5" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#);
icon!(RETRY, "retry", r#"<path d="M3.25 8A4.75 4.75 0 1 0 4.7 4.6" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" fill="none"/><path d="M4.25 1.75V4.9H7.4" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);
icon!(INFO, "info", r#"<circle cx="8" cy="8" r="6" stroke="currentColor" stroke-width="1.3" fill="none"/><path d="M8 7.25V11.25" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><circle cx="8" cy="5" r=".9" fill="currentColor"/>"#);
icon!(WARNING, "warning", r#"<path d="M8 2.25L14.25 13.25H1.75L8 2.25Z" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" fill="none"/><path d="M8 6.5V9.5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round"/><circle cx="8" cy="11.4" r=".85" fill="currentColor"/>"#);
icon!(EJECT, "eject", r#"<path d="M8 3L13 9H3L8 3Z" fill="currentColor"/><path d="M3 12H13" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/>"#);
icon!(STAR, "star", r#"<path d="M8 1.9L9.8 5.6L13.85 6.2L10.9 9.05L11.6 13.1L8 11.2L4.4 13.1L5.1 9.05L2.15 6.2L6.2 5.6L8 1.9Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" fill="none"/>"#);
// Two people: an item shared with others.
icon!(SHARED, "shared", r#"<circle cx="6" cy="5.25" r="2.25" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M1.75 13.25C1.75 10.9 3.65 9.25 6 9.25C8.35 9.25 10.25 10.9 10.25 13.25" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/><path d="M10.25 3.2A2.25 2.25 0 0 1 10.25 7.3M11.75 9.5C13.3 9.95 14.25 11.4 14.25 13.25" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/>"#);
icon!(LOCK, "lock", r#"<rect x="3.25" y="7" width="9.5" height="7" rx="1.25" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M5.25 7V5A2.75 2.75 0 0 1 10.75 5V7" stroke="currentColor" stroke-width="1.2" fill="none"/>"#);
icon!(MENU, "menu", r#"<path d="M2.5 4.25H13.5M2.5 8H13.5M2.5 11.75H13.5" stroke="currentColor" stroke-width="1.3" stroke-linecap="round"/>"#);
icon!(WINCH, "winch", r#"<circle cx="8" cy="6" r="4" stroke="currentColor" stroke-width="1.4" fill="none"/><circle cx="8" cy="6" r="1.25" fill="currentColor"/><path d="M12 6V13.25M10.5 11.75L12 13.25L13.5 11.75" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" fill="none"/>"#);

// Volumes in the location menu: a disk inside the computer, one on a cable, a share on the
// network, and a disc (a disk image, or a CD or DVD).
icon!(DRIVE, "drive", r#"<rect x="1.75" y="4.75" width="12.5" height="6.5" rx="1.75" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M4.25 8H7.75" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/><circle cx="11.25" cy="8" r=".85" fill="currentColor"/>"#);
icon!(DRIVE_EXTERNAL, "drive-external", r#"<rect x="4.25" y="3.25" width="10" height="7" rx="1.5" stroke="currentColor" stroke-width="1.2" fill="none"/><circle cx="11.5" cy="6.75" r=".85" fill="currentColor"/><path d="M4.25 6.75H3.25C2.42 6.75 1.75 7.42 1.75 8.25V12.75" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" fill="none"/><rect x="0.75" y="12" width="2" height="2.25" rx=".5" fill="currentColor"/>"#);
icon!(NETWORK_SHARE, "network-share", r#"<rect x="3.25" y="1.75" width="9.5" height="6.5" rx="1.25" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 8.25V12.5M1.75 12.5H6.75M9.25 12.5H14.25" stroke="currentColor" stroke-width="1.2" stroke-linecap="round"/><circle cx="8" cy="12.5" r="1.25" stroke="currentColor" stroke-width="1.1" fill="none"/>"#);
icon!(DISC, "disc", r#"<circle cx="8" cy="8" r="6" stroke="currentColor" stroke-width="1.2" fill="none"/><circle cx="8" cy="8" r="1.6" stroke="currentColor" stroke-width="1.2" fill="none"/><path d="M8 3.9A4.1 4.1 0 0 1 12.1 8" stroke="currentColor" stroke-width="1.1" stroke-linecap="round" fill="none" opacity=".6"/>"#);

/// The icon for a mounted volume in the location menu and pane header.
pub fn volume(kind: filewinch_os::volumes::VolumeKind) -> &'static Icon {
    use filewinch_os::volumes::VolumeKind as K;
    match kind {
        K::Internal => &DRIVE,
        K::External => &DRIVE_EXTERNAL,
        K::Network => &NETWORK_SHARE,
        K::DiskImage | K::Optical => &DISC,
    }
}

/// The icon for a protocol in the sidebar and pane header.
pub fn protocol(p: Protocol) -> &'static Icon {
    match p {
        Protocol::Local => &LAPTOP,
        Protocol::Sftp | Protocol::Ftp | Protocol::Ftps | Protocol::WebDav | Protocol::WebDavs => &SERVER,
        Protocol::S3 | Protocol::B2 | Protocol::AzureBlob | Protocol::Swift => &BUCKET,
        Protocol::GoogleDrive | Protocol::Dropbox | Protocol::OneDrive | Protocol::Box => &CLOUD,
    }
}

/// What kind of file a name is, for its icon, tint and Kind column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    Folder,
    Link,
    Image,
    Code,
    Text,
    Archive,
    Media,
    Document,
    Other,
}

pub fn kind_of(name: &str, kind: EntryKind) -> FileKind {
    match kind {
        EntryKind::Dir => return FileKind::Folder,
        EntryKind::Symlink => return FileKind::Link,
        _ => {}
    }
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "tif" | "tiff" | "avif" | "heic" | "psd" => FileKind::Image,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "py" | "go" | "swift" | "c" | "h" | "cc" | "cpp" | "hpp" | "m" | "java" | "kt"
        | "rb" | "php" | "cs" | "sh" | "zsh" | "bash" | "html" | "htm" | "css" | "scss" | "vue" | "sql" | "lua" | "pl" => FileKind::Code,
        "txt" | "md" | "markdown" | "json" | "toml" | "yaml" | "yml" | "xml" | "csv" | "ini" | "conf" | "cfg" | "log" | "env" => {
            FileKind::Text
        }
        "zip" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "tar" | "dmg" | "pkg" | "iso" => FileKind::Archive,
        "mp3" | "m4a" | "wav" | "flac" | "aac" | "ogg" | "mp4" | "mov" | "m4v" | "mkv" | "webm" | "avi" => FileKind::Media,
        "pdf" | "doc" | "docx" | "pages" | "key" | "numbers" | "xls" | "xlsx" | "ppt" | "pptx" | "rtf" => FileKind::Document,
        _ => FileKind::Other,
    }
}

impl FileKind {
    pub fn icon(self) -> &'static Icon {
        match self {
            FileKind::Folder => &FOLDER,
            FileKind::Link => &LINK,
            FileKind::Image => &FILE_IMAGE,
            FileKind::Code => &FILE_CODE,
            FileKind::Text | FileKind::Document => &FILE_TEXT,
            FileKind::Archive => &FILE_ARCHIVE,
            FileKind::Media => &FILE_MEDIA,
            FileKind::Other => &FILE,
        }
    }

    /// The Kind column: "Folder", "PNG image", "Plain text"…
    pub fn describe(self, name: &str) -> String {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_uppercase()).filter(|e| !e.is_empty() && e.len() <= 8);
        match (self, ext) {
            (FileKind::Folder, _) => "Folder".into(),
            (FileKind::Link, _) => "Alias".into(),
            (FileKind::Image, Some(e)) => format!("{e} image"),
            (FileKind::Code, Some(e)) => format!("{e} source"),
            (FileKind::Text, Some(e)) if e == "TXT" => "Plain text".into(),
            (FileKind::Text, Some(e)) => format!("{e} text"),
            (FileKind::Archive, Some(e)) => format!("{e} archive"),
            (FileKind::Media, Some(e)) => format!("{e} media"),
            (FileKind::Document, Some(e)) => format!("{e} document"),
            (_, Some(e)) => format!("{e} file"),
            (_, None) => "Document".into(),
        }
    }

    /// The icon's tint: folders take the accent; files a quiet hue by type.
    pub fn tint(self, accent: Color, muted: Color, dark: bool) -> Color {
        let l = if dark { 0.76 } else { 0.58 };
        match self {
            FileKind::Folder => accent,
            FileKind::Image => rune::oklch(l, 0.12, 330.0),
            FileKind::Code => rune::oklch(l, 0.11, 255.0),
            FileKind::Archive => rune::oklch(l, 0.10, 75.0),
            FileKind::Media => rune::oklch(l, 0.12, 25.0),
            FileKind::Document => rune::oklch(l, 0.10, 145.0),
            _ => muted,
        }
    }
}
