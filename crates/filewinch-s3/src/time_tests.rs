use super::*;

#[test]
fn formats_amz_date() {
    let t = UNIX_EPOCH + Duration::from_secs(1_369_353_600); // 2013-05-24T00:00:00Z
    assert_eq!(amz_date(t), "20130524T000000Z");
    let t = UNIX_EPOCH + Duration::from_secs(1_440_938_160); // 2015-08-30T12:36:00Z
    assert_eq!(amz_date(t), "20150830T123600Z");
}

#[test]
fn parses_iso8601() {
    let t = parse_iso8601("2009-10-12T17:50:30.000Z").unwrap();
    assert_eq!(t, UNIX_EPOCH + Duration::from_secs(1_255_369_830));
    assert_eq!(parse_iso8601("2013-05-24T00:00:00Z").unwrap(), UNIX_EPOCH + Duration::from_secs(1_369_353_600));
    assert_eq!(
        parse_iso8601("2024-02-29T00:00:00.5Z").unwrap(),
        UNIX_EPOCH + Duration::new(1_709_164_800, 500_000_000)
    );
    assert!(parse_iso8601("yesterday").is_none());
    assert!(parse_iso8601("2024-13-01T00:00:00Z").is_none());
    assert!(parse_iso8601("2024-01-01T00:00:00+05:30").is_none());
}
