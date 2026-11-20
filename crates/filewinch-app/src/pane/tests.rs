use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use filewinch_core::{Entry, EntryKind, RPath};

use super::model::*;

fn e(name: &str, kind: EntryKind, size: u64, age: u64) -> Entry {
    Entry {
        name: name.into(),
        path: RPath::new("/d").join(name),
        kind,
        size: (kind == EntryKind::File).then_some(size),
        modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 - age)),
        mode: Some(0o644),
        owner: None,
        group: None,
        link_target: None,
        extra: Vec::new(), checksums: Vec::new(), shared: None,
    }
}

fn names(entries: &[Entry], rows: &[u32]) -> Vec<String> {
    rows.iter().map(|&i| entries[i as usize].name.clone()).collect()
}

fn sample() -> Vec<Entry> {
    vec![
        e("file10.txt", EntryKind::File, 10, 5),
        e("b-folder", EntryKind::Dir, 0, 1),
        e("file2.txt", EntryKind::File, 300, 9),
        e(".hidden", EntryKind::File, 1, 1),
        e("A-folder", EntryKind::Dir, 0, 3),
        e("Zeta.png", EntryKind::File, 50, 2),
    ]
}

#[test]
fn folders_first_then_natural_name_order() {
    let es = sample();
    let rows = arrange(&es, Sort::default(), "", false);
    assert_eq!(names(&es, &rows), ["A-folder", "b-folder", "file2.txt", "file10.txt", "Zeta.png"]);
}

#[test]
fn hidden_files_and_filter() {
    let es = sample();
    assert_eq!(arrange(&es, Sort::default(), "", true).len(), 6);
    let rows = arrange(&es, Sort::default(), "FILE", false);
    assert_eq!(names(&es, &rows), ["file2.txt", "file10.txt"]);
    assert!(arrange(&es, Sort::default(), "nothing-matches", true).is_empty());
}

#[test]
fn sort_by_size_and_date_both_ways() {
    let es = sample();
    let by_size = arrange(&es, Sort { key: SortKey::Size, ascending: false }, "", false);
    // Folders still lead; descending reverses within each group.
    assert_eq!(names(&es, &by_size), ["b-folder", "A-folder", "file2.txt", "Zeta.png", "file10.txt"]);
    let by_date = arrange(&es, Sort { key: SortKey::Modified, ascending: true }, "", false);
    assert_eq!(names(&es, &by_date)[2..], ["file2.txt", "file10.txt", "Zeta.png"]);
}

#[test]
fn click_shift_and_cmd_selection() {
    let order = ["a", "b", "c", "d", "e"];
    let mut s = Selection::default();
    s.pick(&order, 1, Pick::Only);
    s.pick(&order, 3, Pick::Extend);
    assert_eq!(s.names, ["b", "c", "d"].iter().map(|x| x.to_string()).collect::<HashSet<_>>());
    s.pick(&order, 2, Pick::Toggle);
    assert!(!s.is_selected("c") && s.is_selected("b") && s.is_selected("d"));
    // Shift-click extends from the last plain or Cmd click.
    s.pick(&order, 4, Pick::Extend);
    assert_eq!(s.names.len(), 3, "c..e");
    s.pick(&order, 0, Pick::Only);
    assert_eq!(s.names.len(), 1);
}

#[test]
fn arrows_move_and_extend_and_clamp() {
    let order = ["a", "b", "c"];
    let mut s = Selection::default();
    assert_eq!(s.step(&order, 1, false), Some(0), "the first Down lands on the first row");
    assert_eq!(s.step(&order, 1, true), Some(1));
    assert_eq!(s.names.len(), 2);
    assert_eq!(s.step(&order, 10, false), Some(2), "clamped at the end");
    assert_eq!(s.names.len(), 1);
    assert_eq!(s.step(&[], 1, false), None);
    let mut fresh = Selection::default();
    assert_eq!(fresh.step(&order, -1, false), Some(2), "the first Up lands on the last row");
}

#[test]
fn selection_survives_refresh_by_name() {
    let mut s = Selection::default();
    s.select_all(&["a", "b", "c"]);
    let listed: HashSet<&str> = ["a", "c"].into_iter().collect();
    s.retain_listed(&listed);
    assert_eq!(s.names.len(), 2);
    s.cursor = Some("b".into());
    s.retain_listed(&listed);
    assert_eq!(s.cursor, None);
}

#[test]
fn type_to_select() {
    let order = ["alpha", "beta", "bravo", "charlie"];
    let mut t = TypeAhead::default();
    assert_eq!(t.type_char("b", 10.0, &order), Some(1));
    assert_eq!(t.type_char("r", 10.3, &order), Some(2));
    assert_eq!(t.type_char("c", 12.0, &order), Some(3), "a pause starts over");
    assert_eq!(t.type_char("z", 20.0, &order), None);
    assert_eq!(t.type_char("B", 30.0, &order), Some(1), "any case");
}

#[test]
fn unique_and_invalid_names() {
    let taken: HashSet<&str> = ["untitled folder", "untitled folder 2", "a.txt"].into_iter().collect();
    assert_eq!(unique_name("untitled folder", &taken), "untitled folder 3");
    assert_eq!(unique_name("a.txt", &taken), "a 2.txt");
    assert_eq!(unique_name("new", &taken), "new");
    assert!(invalid_name("", false).is_some());
    assert!(invalid_name("a/b", false).is_some());
    assert!(invalid_name("..", false).is_some());
    assert!(invalid_name("ok name.txt", false).is_none());
    // Remote names follow the server, not this OS; local ones follow this OS's rules.
    assert!(invalid_name("12:30.txt", false).is_none());
    assert_eq!(invalid_name("12:30.txt", true).is_some(), cfg!(windows));
}

mod layout {
    use filewinch_store::{ColumnId, ColumnPref};

    use crate::pane::layout::*;

    const SEP: f32 = 12.0;
    const DOTS: f32 = 18.0;

    #[test]
    fn crumbs_show_in_full_while_they_fit() {
        let w = [90.0, 50.0, 60.0, 70.0, 80.0];
        let all = 350.0 + 4.0 * SEP;
        assert_eq!(fit_crumbs(&w, SEP, DOTS, all), 0);
        assert_eq!(fit_crumbs(&w, SEP, DOTS, 10_000.0), 0);
    }

    #[test]
    fn crumbs_give_way_from_the_left_into_an_ellipsis() {
        let w = [90.0, 50.0, 60.0, 70.0, 80.0];
        let all = 350.0 + 4.0 * SEP;
        // One point short: the root goes, and "…" takes its place.
        assert_eq!(fit_crumbs(&w, SEP, DOTS, all - 1.0), 1);
        // The last two plus "…".
        let two = 70.0 + 80.0 + SEP + DOTS + SEP;
        assert_eq!(fit_crumbs(&w, SEP, DOTS, two), 3);
        assert_eq!(fit_crumbs(&w, SEP, DOTS, two - 0.5), 4);
    }

    #[test]
    fn the_last_crumb_always_shows() {
        assert_eq!(fit_crumbs(&[90.0, 400.0], SEP, DOTS, 100.0), 1);
        assert_eq!(fit_crumbs(&[400.0], SEP, DOTS, 100.0), 0);
        assert_eq!(fit_crumbs(&[], SEP, DOTS, 100.0), 0);
        assert_eq!(fit_crumbs(&[10.0, 10.0], SEP, DOTS, 0.0), 1);
    }

    fn cols(ids: &[ColumnId]) -> Vec<ColumnPref> {
        ids.iter().map(|&id| ColumnPref { id, width: None }).collect()
    }

    #[test]
    fn permissions_show_only_when_turned_on() {
        let all = ColumnPref::defaults();
        assert!(!visible(&all, false).iter().any(|c| c.id == ColumnId::Permissions));
        assert_eq!(visible(&all, true).len(), 5);
    }

    #[test]
    fn name_takes_the_room_the_others_leave() {
        let c = cols(&[ColumnId::Name, ColumnId::Size, ColumnId::Modified, ColumnId::Kind]);
        assert_eq!(resolve(&c, Some(600.0)), vec![600.0 - 80.0 - 138.0 - 120.0, 80.0, 138.0, 120.0]);
        // Never under its minimum, even in a narrow pane.
        assert_eq!(resolve(&c, Some(200.0))[0], min_width(ColumnId::Name));
        // Saved widths are kept in range.
        let c = vec![ColumnPref { id: ColumnId::Size, width: Some(10) }, ColumnPref { id: ColumnId::Kind, width: Some(900) }];
        assert_eq!(resolve(&c, None), vec![min_width(ColumnId::Size), MAX_WIDTH]);
    }

    #[test]
    fn a_divider_resizes_the_column_with_a_width() {
        let c = cols(&[ColumnId::Name, ColumnId::Size, ColumnId::Modified]);
        // Between Name and Size: Size grows as the divider moves left.
        assert_eq!(resize_target(&c, 0), Some((1, -1.0)));
        // Between Size and Modified: Size grows as it moves right.
        assert_eq!(resize_target(&c, 1), Some((1, 1.0)));
        assert_eq!(resize_target(&c, 2), None);
        assert_eq!(resized(&c, 1, -1.0, 80.0, -40.0, Some(800.0)), 120.0);
        assert_eq!(resized(&c, 1, 1.0, 80.0, 25.4, Some(800.0)), 105.0);
        // Not under the minimum, not past the room Name needs, not past the maximum.
        assert_eq!(resized(&c, 1, 1.0, 80.0, -500.0, Some(800.0)), min_width(ColumnId::Size));
        assert_eq!(resized(&c, 1, 1.0, 80.0, 500.0, Some(500.0)), 500.0 - 138.0 - min_width(ColumnId::Name));
        assert_eq!(resized(&c, 1, 1.0, 80.0, 5000.0, Some(5000.0)), MAX_WIDTH);
    }

    #[test]
    fn fitting_a_column_adds_padding_and_stays_in_range() {
        assert_eq!(fitted(ColumnId::Kind, 100.2, 16.0), 117.0);
        assert_eq!(fitted(ColumnId::Size, 3.0, 16.0), min_width(ColumnId::Size));
        assert_eq!(fitted(ColumnId::Kind, 3000.0, 16.0), MAX_WIDTH);
    }

    #[test]
    fn a_dragged_header_lands_past_the_centres_it_crossed() {
        let w = [200.0, 80.0, 140.0, 100.0];
        assert_eq!(drop_index(&w, 1, 0.0), 1);
        assert_eq!(drop_index(&w, 1, 30.0), 1);
        // Size's centre (240) passes Modified's (350).
        assert_eq!(drop_index(&w, 1, 111.0), 2);
        assert_eq!(drop_index(&w, 1, 1000.0), 3);
        assert_eq!(drop_index(&w, 1, -150.0), 0);
        assert_eq!(drop_index(&w, 0, 10_000.0), 3);
    }

    #[test]
    fn reordering_keeps_hidden_columns_beside_their_neighbours() {
        let all = ColumnPref::defaults(); // Name, Size, Modified, Kind, Permissions
        let shown = visible(&all, false);
        let ids = |v: Vec<ColumnPref>| v.into_iter().map(|c| c.id).collect::<Vec<_>>();
        use ColumnId::*;
        assert_eq!(ids(reorder(&all, &shown, 1, 3)), [Name, Modified, Kind, Size, Permissions]);
        assert_eq!(ids(reorder(&all, &shown, 3, 0)), [Kind, Name, Size, Modified, Permissions]);
        assert_eq!(ids(reorder(&all, &shown, 0, 0)), [Name, Size, Modified, Kind, Permissions]);
        let shown = visible(&all, true);
        assert_eq!(ids(reorder(&all, &shown, 4, 1)), [Name, Permissions, Size, Modified, Kind]);
        let w = with_width(&all, Kind, 151.6);
        assert_eq!(w.iter().find(|c| c.id == Kind).and_then(|c| c.width), Some(152));
    }
}
