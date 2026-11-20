//! Static Large Object uploads: segments, manifest, unknown sizes, cleanup on drop and
//! failure, and manifest deletion.

mod mock;

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use filewinch_core::{ErrorKind, RPath, Remote};
use filewinch_swift::Tuning;
use mock::Mock;

const SEG: u64 = 64 * 1024;

fn slo_tuning() -> Tuning {
    Tuning { large_object_threshold: 100 * 1024, segment_size: SEG, ..Tuning::default() }
}

fn data(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect()
}

fn read_all(r: &impl Remote, p: &str) -> Vec<u8> {
    let mut buf = Vec::new();
    r.read(&RPath::new(p), 0).unwrap().read_to_end(&mut buf).unwrap();
    buf
}

/// Writes in odd-sized pieces so segment boundaries fall mid-write.
fn write_pieces(sink: &mut dyn Write, d: &[u8]) {
    for piece in d.chunks(10_007) {
        sink.write_all(piece).unwrap();
    }
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn slo_upload_download_and_delete() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(slo_tuning());
    let d = data(160 * 1024); // 3 segments: 64 + 64 + 32 KiB
    let mut sink = r.write(&RPath::new("/c/dir/big file.bin"), 0, Some(d.len() as u64)).unwrap();
    write_pieces(&mut sink, &d);
    sink.finish().unwrap();
    let segs = m.keys("c_segments");
    assert_eq!(segs.len(), 3, "{segs:?}");
    assert!(segs.iter().all(|s| s.starts_with("dir/big file.bin/slo/")), "{segs:?}");
    assert!(m.st().containers["c"]["dir/big file.bin"].manifest.is_some());
    assert_eq!(read_all(&r, "/c/dir/big file.bin"), d);
    let e = r.stat(&RPath::new("/c/dir/big file.bin")).unwrap();
    assert_eq!(e.size, Some(d.len() as u64));
    // Ranged read across a segment boundary.
    let mut tail = Vec::new();
    r.read(&RPath::new("/c/dir/big file.bin"), SEG - 10).unwrap().read_to_end(&mut tail).unwrap();
    assert_eq!(tail, d[SEG as usize - 10..]);
    // Listing shows only the file, not the segments.
    let list = r.list(&RPath::new("/c/dir")).unwrap();
    assert_eq!(list.len(), 1);
    // An SLO's ETag is the MD5 of its segments' MD5s, not of the content: no checksum.
    assert!(list[0].checksums.is_empty(), "{:?}", list[0].checksums);
    assert!(e.checksums.is_empty(), "{:?}", e.checksums);
    r.remove_file(&RPath::new("/c/dir/big file.bin")).unwrap();
    assert!(!m.has("c", "dir/big file.bin"));
    assert!(m.keys("c_segments").is_empty());
}

#[test]
fn unknown_size_uses_segments_only_when_needed() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(slo_tuning());
    let small = data(1000);
    let mut sink = r.write(&RPath::new("/c/small"), 0, None).unwrap();
    write_pieces(&mut sink, &small);
    sink.finish().unwrap();
    assert!(m.st().containers["c"]["small"].manifest.is_none());
    assert_eq!(read_all(&r, "/c/small"), small);
    wait_until("the temporary segment to go", || m.keys("c_segments").is_empty());

    let big = data(200 * 1024);
    let mut sink = r.write(&RPath::new("/c/big"), 0, None).unwrap();
    write_pieces(&mut sink, &big);
    sink.finish().unwrap();
    assert!(m.st().containers["c"]["big"].manifest.is_some());
    assert_eq!(m.keys("c_segments").len(), 4);
    assert_eq!(read_all(&r, "/c/big"), big);

    let sink = r.write(&RPath::new("/c/empty"), 0, None).unwrap();
    sink.finish().unwrap();
    assert_eq!(read_all(&r, "/c/empty"), b"");
}

#[test]
fn dropped_slo_sink_leaves_no_object_and_cleans_segments() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(slo_tuning());
    let d = data(160 * 1024);
    let mut sink = r.write(&RPath::new("/c/big"), 0, Some(d.len() as u64)).unwrap();
    write_pieces(&mut sink, &d[..140 * 1024]); // two segments done, third in flight
    assert_eq!(m.keys("c_segments").len(), 2);
    drop(sink);
    wait_until("segments to be deleted", || m.keys("c_segments").is_empty());
    assert!(!m.has("c", "big"));

    // All segments written, dropped before finish: nothing committed either.
    let mut sink = r.write(&RPath::new("/c/big"), 0, Some(d.len() as u64)).unwrap();
    write_pieces(&mut sink, &d);
    drop(sink);
    wait_until("segments to be deleted", || m.keys("c_segments").is_empty());
    assert!(!m.has("c", "big"));
}

#[test]
fn failed_manifest_cleans_segments() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(slo_tuning());
    let d = data(130 * 1024);
    let mut sink = r.write(&RPath::new("/c/big"), 0, Some(d.len() as u64)).unwrap();
    write_pieces(&mut sink, &d);
    m.st().fail.push_back((507, vec![], "Insufficient Storage".into()));
    let e = sink.finish().unwrap_err();
    assert_eq!(e.kind, ErrorKind::NoSpace, "{e}");
    wait_until("segments to be deleted", || m.keys("c_segments").is_empty());
    assert!(!m.has("c", "big"));
}

#[test]
fn damaged_segment_fails_and_is_cleaned() {
    let m = Mock::start();
    m.put("c", "seed", b"");
    let r = m.connect(slo_tuning());
    m.st().bad_etag = true;
    let d = data(130 * 1024);
    let mut sink = r.write(&RPath::new("/c/big"), 0, Some(d.len() as u64)).unwrap();
    let mut failed = None;
    for piece in d.chunks(10_007) {
        if let Err(e) = sink.write_all(piece) {
            failed = Some(e);
            break;
        }
    }
    let e = filewinch_http::from_io(failed.expect("a damaged segment fails the write"));
    assert_eq!(e.kind, ErrorKind::Network);
    drop(sink);
    wait_until("segments to be deleted", || m.keys("c_segments").is_empty());
    assert!(!m.has("c", "big"));
}

#[test]
fn dlo_and_old_style_slo_have_no_checksums() {
    let m = Mock::start();
    m.put("c", "segs/1", b"hello ");
    m.put("c", "segs/2", b"world");
    m.put("c", "plain", b"hello world");
    {
        let mut st = m.st();
        let objs = st.containers.get_mut("c").unwrap();
        let dlo = mock::Obj {
            data: Vec::new(),
            content_type: "text/plain".into(),
            etag: format!("{:x}", md5::compute(b"")),
            manifest: None,
            dlo: Some("c/segs/".into()),
        };
        objs.insert("dlo".into(), dlo);
        // Older Swift lists an SLO with `swift_bytes=` on its content type (and no slo_etag).
        let old = mock::Obj {
            data: b"manifest json".to_vec(),
            content_type: "application/octet-stream;swift_bytes=11".into(),
            etag: format!("{:x}", md5::compute(b"manifest json")),
            manifest: None,
            dlo: None,
        };
        objs.insert("old-slo".into(), old);
    }
    let r = m.connect(Tuning::default());
    assert_eq!(read_all(&r, "/c/dlo"), b"hello world");
    let list = r.list(&RPath::new("/c")).unwrap();
    let sums = |n: &str| list.iter().find(|e| e.name == n).unwrap().checksums.clone();
    assert_eq!(sums("plain").len(), 1);
    assert!(sums("dlo").is_empty());
    assert!(sums("old-slo").is_empty());
    let dlo = r.stat(&RPath::new("/c/dlo")).unwrap();
    assert_eq!(dlo.size, Some(11));
    assert!(dlo.checksums.is_empty(), "{:?}", dlo.checksums);
}
