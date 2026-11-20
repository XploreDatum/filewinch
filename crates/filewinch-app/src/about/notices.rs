//! THIRD-PARTY-NOTICES.md (written by `scripts/licenses.sh`), built into the app and read
//! once: the component count and each licence text with the components that use it.

use std::sync::OnceLock;

const FILE: &str = include_str!("../../../../THIRD-PARTY-NOTICES.md");

pub struct Group {
    /// "MIT License (MIT)".
    pub title: String,
    /// "serde 1.0.228", ...
    pub used_by: Vec<String>,
    pub text: String,
}

pub struct Notices {
    pub components: usize,
    pub groups: Vec<Group>,
}

pub fn get() -> &'static Notices {
    static N: OnceLock<Notices> = OnceLock::new();
    N.get_or_init(|| parse(FILE))
}

/// Reads the layout `render.py` writes: `## Components (N)`, then per licence `### Title`,
/// `Used by: a 1.0, b 2.0.` and the text in a ```text fence.
pub fn parse(md: &str) -> Notices {
    let components = md.lines().find_map(|l| l.strip_prefix("## Components (")?.strip_suffix(')')?.parse().ok()).unwrap_or(0);
    let mut groups = Vec::new();
    for section in md.split("\n### ").skip(1) {
        let mut lines = section.lines();
        let title = lines.next().unwrap_or("").trim().to_owned();
        let rest: Vec<&str> = lines.collect();
        let used_by = rest.iter().find_map(|l| l.strip_prefix("Used by: "))
            .map(|u| u.trim_end_matches('.').split(", ").map(str::to_owned).collect())
            .unwrap_or_default();
        let start = rest.iter().position(|l| *l == "```text").map_or(rest.len(), |i| i + 1);
        let end = rest.iter().rposition(|l| *l == "```").filter(|e| *e >= start).unwrap_or(rest.len());
        let text = rest[start..end].join("\n");
        if !title.is_empty() {
            groups.push(Group { title, used_by, text });
        }
    }
    Notices { components, groups }
}
