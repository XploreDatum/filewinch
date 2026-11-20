use super::*;

fn t(y: i64, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> SystemTime {
    to_time(y, mo, d, h, mi, s).unwrap()
}

fn dir() -> RPath {
    RPath::new("/pub")
}

#[test]
fn calendar_roundtrip() {
    for days in [-1000i64, 0, 1, 59, 60, 365, 11_016, 20_000, 30_000] {
        let (y, m, d) = civil_from_days(days);
        assert_eq!(days_from_civil(y, m, d), days);
    }
    assert_eq!(t(2000, 3, 1, 0, 0, 0), UNIX_EPOCH + Duration::from_secs(951_868_800));
}

#[test]
fn timevals() {
    let v = parse_timeval("20240229123456").unwrap();
    assert_eq!(v, t(2024, 2, 29, 12, 34, 56));
    assert_eq!(format_timeval(v), "20240229123456");
    assert_eq!(parse_timeval("20240229123456.5").unwrap(), v + Duration::from_millis(500));
    assert!(parse_timeval("2024").is_none());
    assert!(parse_timeval("20241332000000").is_none());
}

#[test]
fn mlsd() {
    let e = parse_mlsd_line(&dir(), "type=file;size=1234;modify=20230102030405;UNIX.mode=0644;UNIX.owner=ftp; my file.txt").unwrap();
    assert_eq!(e.name, "my file.txt");
    assert_eq!(e.path.as_str(), "/pub/my file.txt");
    assert_eq!(e.kind, EntryKind::File);
    assert_eq!(e.size, Some(1234));
    assert_eq!(e.mode, Some(0o644));
    assert_eq!(e.modified, Some(t(2023, 1, 2, 3, 4, 5)));
    assert_eq!(e.owner.as_deref(), Some("ftp"));
    let d = parse_mlsd_line(&dir(), "Type=dir;Modify=20230102030405; sub\r\n").unwrap();
    assert_eq!(d.kind, EntryKind::Dir);
    assert!(parse_mlsd_line(&dir(), "type=cdir; .").is_none());
    assert!(parse_mlsd_line(&dir(), "type=pdir; ..").is_none());
    let l = parse_mlsd_line(&dir(), "type=OS.unix=slink:/etc/x;size=5; lnk").unwrap();
    assert_eq!(l.kind, EntryKind::Symlink);
    assert_eq!(l.link_target.as_deref(), Some("/etc/x"));
}

#[test]
fn unix_list() {
    let now = t(2024, 6, 15, 12, 0, 0);
    let e = parse_list_line(&dir(), "-rw-r--r--    1 deploy   www-data     4096 Mar  3 14:05 index  page.html", now).unwrap();
    assert_eq!(e.name, "index  page.html");
    assert_eq!(e.size, Some(4096));
    assert_eq!(e.mode, Some(0o644));
    assert_eq!(e.owner.as_deref(), Some("deploy"));
    assert_eq!(e.group.as_deref(), Some("www-data"));
    assert_eq!(e.modified, Some(t(2024, 3, 3, 14, 5, 0)));

    // date in the "future" → last year
    let e = parse_list_line(&dir(), "-rw-r--r-- 1 a b 1 Dec 24 10:00 x", now).unwrap();
    assert_eq!(e.modified, Some(t(2023, 12, 24, 10, 0, 0)));

    let e = parse_list_line(&dir(), "drwxr-sr-x   12 root root 4096 Jan  9  2019 old dir", now).unwrap();
    assert_eq!(e.kind, EntryKind::Dir);
    assert_eq!(e.size, None);
    assert_eq!(e.mode, Some(0o2755));
    assert_eq!(e.modified, Some(t(2019, 1, 9, 0, 0, 0)));

    let e = parse_list_line(&dir(), "lrwxrwxrwx 1 u g 11 Feb 29 01:02 current -> releases/42", now).unwrap();
    assert_eq!(e.kind, EntryKind::Symlink);
    assert_eq!(e.name, "current");
    assert_eq!(e.link_target.as_deref(), Some("releases/42"));

    // no group column (some servers), sticky bit
    let e = parse_list_line(&dir(), "drwxrwxrwt 2 ftp 512 May 5 2020 tmp", now).unwrap();
    assert_eq!(e.owner.as_deref(), Some("ftp"));
    assert_eq!(e.group, None);
    assert_eq!(e.mode, Some(0o1777));

    // a name that looks like a date is still the name
    let e = parse_list_line(&dir(), "-rw------- 1 u g 7 Apr 1 2021 Jan 1 2020", now).unwrap();
    assert_eq!(e.name, "Jan 1 2020");

    assert!(parse_list_line(&dir(), "total 48", now).is_none());
    assert!(parse_list_line(&dir(), "drwxr-xr-x 2 u g 4096 Jan 1 2020 .", now).is_none());
    assert!(parse_list_line(&dir(), "garbage", now).is_none());
}

#[test]
fn dos_list() {
    let now = t(2024, 6, 15, 12, 0, 0);
    let e = parse_list_line(&dir(), "01-15-20  03:04PM       <DIR>          My Documents", now).unwrap();
    assert_eq!(e.kind, EntryKind::Dir);
    assert_eq!(e.name, "My Documents");
    assert_eq!(e.modified, Some(t(2020, 1, 15, 15, 4, 0)));
    let e = parse_list_line(&dir(), "11-02-2023  12:30AM             1,234,567 report.pdf", now).unwrap();
    assert_eq!(e.size, Some(1_234_567));
    assert_eq!(e.modified, Some(t(2023, 11, 2, 0, 30, 0)));
    let e = parse_list_line(&dir(), "07-04-99  23:59                 10 legacy.txt", now).unwrap();
    assert_eq!(e.modified, Some(t(1999, 7, 4, 23, 59, 0)));
}
