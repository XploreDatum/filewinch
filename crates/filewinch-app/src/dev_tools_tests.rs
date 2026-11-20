use crate::dev_tools::requested;

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn finds_a_dev_flag_with_or_without_a_value() {
    assert_eq!(requested(args(&["--snap", "out.png", "--scene", "local"])), Some("--snap".into()));
    assert_eq!(requested(args(&["--look=windows"])), Some("--look".into()));
    assert_eq!(requested(args(&["--list-scenes"])), Some("--list-scenes".into()));
}

#[test]
fn ordinary_launches_pass() {
    // No arguments, a URL handed over by the desktop (`Exec=filewinch %U`), and the
    // `-psn_...` argument old Finder launches add.
    assert_eq!(requested(args(&[])), None);
    assert_eq!(requested(args(&["sftp://deploy@web1.example.com/var/www"])), None);
    assert_eq!(requested(args(&["-psn_0_12345"])), None);
    // A flag that merely starts like one of them isn't one.
    assert_eq!(requested(args(&["--snapshot-dir"])), None);
}
