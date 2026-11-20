//! About: what it says, its links, the licence status, and the third-party notices built in.

use filewinch_license::Status;
use rune::{Env, Runtime};

use super::{AboutUi, Page, SOURCE_URL, notices, view};

fn runtime(s: AboutUi) -> Runtime<AboutUi> {
    let mut rt = Runtime::new(s, view, (460.0, 480.0), Env::new(false, 1.0));
    rt.frame(0.0);
    rt
}

fn click(rt: &mut Runtime<AboutUi>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is on screen"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    rt.frame(1.0 / 60.0);
}

#[test]
fn about_names_the_version_licence_and_source() {
    let mut rt = runtime(AboutUi::new());
    for key in ["about-name", "about-version", "about-status", "about-copyright", "about-license", "about-url"] {
        assert!(rt.find(key).is_some(), "{key}");
    }
    assert_eq!(crate::license_ui::status_line(&rt.state.license), "Registered to Alex Morgan (alex@example.com)");
    rt.state.license = Status::Unregistered;
    assert_eq!(crate::license_ui::status_line(&rt.state.license), "Unregistered");
    click(&mut rt, "about-source");
    assert_eq!(rt.take_url_requests(), vec![SOURCE_URL.to_owned()]);
    click(&mut rt, "about-notices");
    assert_eq!(rt.state.page, Page::Notices);
    click(&mut rt, "lic-0");
    assert_eq!(rt.state.open, Some(0));
    assert!(rt.find("lic-text-0").is_some(), "the licence text shows");
    click(&mut rt, "notices-back");
    assert_eq!(rt.state.page, Page::About);
}

#[test]
fn the_built_in_notices_cover_the_dependencies() {
    let n = notices::get();
    assert!(n.components > 100, "{}", n.components);
    assert!(n.groups.len() > 10);
    let names: Vec<&String> = n.groups.iter().flat_map(|g| &g.used_by).collect();
    assert!(names.iter().any(|u| u.starts_with("serde ")), "serde is listed");
    assert!(names.iter().any(|u| u.starts_with("rune ")), "Rune is listed");
    assert!(!names.iter().any(|u| u.starts_with("filewinch")), "Filewinch's own crates aren't third-party");
    for g in &n.groups {
        assert!(!g.used_by.is_empty() && !g.text.trim().is_empty(), "{}", g.title);
        assert!(!g.text.contains("```"), "{}", g.title);
    }
}

#[test]
fn notices_parse_the_rendered_layout() {
    let md = "# T\n\n## Components (2)\n\n| a |\n\n## Licence texts (1)\n\n### MIT License (MIT)\n\nUsed by: a 1.0, b 2.0.\n\n```text\nPermission is granted\n\nfreely\n```\n";
    let n = notices::parse(md);
    assert_eq!(n.components, 2);
    assert_eq!(n.groups.len(), 1);
    assert_eq!(n.groups[0].title, "MIT License (MIT)");
    assert_eq!(n.groups[0].used_by, ["a 1.0", "b 2.0"]);
    assert_eq!(n.groups[0].text, "Permission is granted\n\nfreely");
}
