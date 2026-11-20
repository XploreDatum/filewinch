//! App preferences.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// What to do when a transfer's destination already exists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDefault {
    #[default]
    Ask,
    Overwrite,
    OverwriteIfNewer,
    Resume,
    Rename,
    Skip,
}

impl ConflictDefault {
    pub const ALL: [ConflictDefault; 6] = [
        ConflictDefault::Ask,
        ConflictDefault::Overwrite,
        ConflictDefault::OverwriteIfNewer,
        ConflictDefault::Resume,
        ConflictDefault::Rename,
        ConflictDefault::Skip,
    ];
    pub fn label(self) -> &'static str {
        match self {
            ConflictDefault::Ask => "Ask each time",
            ConflictDefault::Overwrite => "Replace",
            ConflictDefault::OverwriteIfNewer => "Replace if newer",
            ConflictDefault::Resume => "Resume, else replace",
            ConflictDefault::Rename => "Keep both",
            ConflictDefault::Skip => "Skip",
        }
    }
}

/// A column of the file lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnId {
    Name,
    Size,
    Modified,
    Kind,
    Permissions,
    /// A column this version doesn't know (written by a newer one); dropped on load.
    #[serde(other)]
    Unknown,
}

impl ColumnId {
    /// Every column, in the order a new window shows them.
    pub const ALL: [ColumnId; 5] = [ColumnId::Name, ColumnId::Size, ColumnId::Modified, ColumnId::Kind, ColumnId::Permissions];
}

/// One column's place and width. Columns are listed in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnPref {
    pub id: ColumnId,
    /// Width in points; `None` is the column's standard width. Name ignores it (it takes the
    /// room the others leave).
    #[serde(default)]
    pub width: Option<u32>,
}

impl ColumnPref {
    /// The narrowest and widest a saved width may be (the app applies its own per-column
    /// minimums on top).
    pub const WIDTH_RANGE: (u32, u32) = (24, 1200);

    /// The standard layout: every column in its usual place at its standard width.
    pub fn defaults() -> Vec<ColumnPref> {
        ColumnId::ALL.iter().map(|&id| ColumnPref { id, width: None }).collect()
    }

    /// `cols` made whole: unknown and repeated columns dropped, missing ones added at the end in
    /// their usual order, widths kept in range.
    pub fn normalized(cols: &[ColumnPref]) -> Vec<ColumnPref> {
        let mut out: Vec<ColumnPref> = Vec::new();
        for c in cols {
            if c.id != ColumnId::Unknown && !out.iter().any(|o| o.id == c.id) {
                let (lo, hi) = ColumnPref::WIDTH_RANGE;
                out.push(ColumnPref { id: c.id, width: c.width.map(|w| w.clamp(lo, hi)) });
            }
        }
        for id in ColumnId::ALL {
            if !out.iter().any(|o| o.id == id) {
                out.push(ColumnPref { id, width: None });
            }
        }
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// Which defaults this file has been brought up to ([`Prefs::VERSION`]); 0 in files from
    /// before it existed.
    #[serde(default)]
    pub version: u32,
    /// Jobs that run at once.
    pub max_jobs: u32,
    /// Files a job moves at once.
    pub files_per_job: u32,
    /// Connections one large download may use (1 is off). A settings file from before this
    /// option has none, and loads with the default, 3.
    pub segments_per_file: u32,
    /// Retries for a dropped connection or rate limit, with backoff.
    pub retries: u32,
    /// Bytes per second across all transfers; `None` is unlimited.
    pub bandwidth_limit: Option<u64>,
    pub conflict: ConflictDefault,
    pub show_hidden: bool,
    /// The Permissions column in file lists.
    pub show_permissions: bool,
    /// The file lists' columns: order and widths (both panes share them).
    pub columns: Vec<ColumnPref>,
    pub confirm_deletes: bool,
    pub appearance: Appearance,
    /// The app external edits open in (`open -a`); `None` uses the file's default app.
    pub editor: Option<String>,
    pub transfers_open: bool,
    pub sidebar_open: bool,
    /// The Inspector panel at the window's right edge, its width in points
    /// ([`Prefs::INSPECTOR_WIDTH`]), and the ids of its sections folded shut ("preview", "info",
    /// "permissions", "checksums", "details").
    pub inspector_open: bool,
    pub inspector_width: u32,
    pub inspector_collapsed: Vec<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            version: Prefs::VERSION,
            max_jobs: 3,
            files_per_job: 3,
            segments_per_file: 3,
            retries: 4,
            bandwidth_limit: None,
            conflict: ConflictDefault::Ask,
            show_hidden: false,
            show_permissions: false,
            columns: ColumnPref::defaults(),
            confirm_deletes: true,
            appearance: Appearance::System,
            editor: None,
            transfers_open: true,
            sidebar_open: true,
            inspector_open: false,
            inspector_width: 280,
            inspector_collapsed: Vec::new(),
        }
    }
}

impl Prefs {
    /// The most connections one download may use.
    pub const MAX_SEGMENTS: u32 = 16;
    /// Above this, Settings warns that servers may limit or refuse parallel downloads.
    pub const SAFE_SEGMENTS: u32 = 3;

    /// The narrowest and widest the Inspector may be, in points.
    pub const INSPECTOR_WIDTH: (u32, u32) = (240, 420);

    /// The current settings-file version. 1: "Files per transfer" defaults to 3 (was 4).
    pub const VERSION: u32 = 1;

    /// The same prefs brought up to [`Prefs::VERSION`], with every number in a sane range (a
    /// hand-edited file can hold anything). Version 0 saved every value, chosen or not, so a
    /// "Files per transfer" of 4 (the old default) becomes 3; any other value was chosen and
    /// stays.
    pub fn clamped(mut self) -> Prefs {
        if self.version < 1 && self.files_per_job == 4 {
            self.files_per_job = 3;
        }
        self.version = self.version.max(Prefs::VERSION);
        self.max_jobs = self.max_jobs.clamp(1, 16);
        self.files_per_job = self.files_per_job.clamp(1, 32);
        self.segments_per_file = self.segments_per_file.clamp(1, Prefs::MAX_SEGMENTS);
        self.retries = self.retries.min(20);
        self.bandwidth_limit = self.bandwidth_limit.filter(|b| *b > 0);
        self.columns = ColumnPref::normalized(&self.columns);
        self.inspector_width = self.inspector_width.clamp(Prefs::INSPECTOR_WIDTH.0, Prefs::INSPECTOR_WIDTH.1);
        self
    }
}
