//! The v2 and v3 backends must answer the same questions the same way.
//!
//! The point of the v3 backend is that storage changed, not semantics: a query answer that
//! starts depending on which backend is loaded is a regression that no type system catches.
//! This test builds the same archive twice, once as the rkyv trie and once as a published
//! snap object, and compares every query shape the API exposes.

use std::collections::BTreeMap;

use ipnet::IpNet;
use wayback_rpki::roas_trie::{RoaRecord, RoasTrie, RoasTrieMut};
use wayback_rpki::snap_backend::SnapBackend;
use wayback_rpki::snap_export::{build_from_rows, RoaSet};

const DAY: i64 = 86_400;
const BASE: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z

/// One fixture row: prefix, max_len, origin, and the day ranges it was in effect.
struct Row(&'static str, u8, u32, Vec<(i64, i64)>);

/// The rows both backends get.
fn rows() -> Vec<Row> {
    vec![
        // a supernet with two origins, and a subnet that only one of them covers
        Row("10.0.0.0/8", 24, 64500, vec![(BASE, BASE + 10 * DAY)]),
        Row(
            "10.0.0.0/8",
            16,
            64501,
            vec![(BASE + 5 * DAY, BASE + 20 * DAY)],
        ),
        Row("10.1.0.0/16", 24, 64500, vec![(BASE, BASE + 3 * DAY)]),
        // an expired announcement: its last day is before the archive's last day
        Row("192.0.2.0/24", 24, 64502, vec![(BASE, BASE + 4 * DAY)]),
        // two disjoint ranges for the same record
        Row(
            "198.51.100.0/24",
            24,
            64503,
            vec![(BASE, BASE + 2 * DAY), (BASE + 8 * DAY, BASE + 9 * DAY)],
        ),
        // a second family
        Row("2001:db8::/32", 48, 64504, vec![(BASE, BASE + 30 * DAY)]),
        Row(
            "2001:db8:1::/48",
            48,
            64504,
            vec![(BASE + 2 * DAY, BASE + 6 * DAY)],
        ),
    ]
}

fn jsonl() -> String {
    rows()
        .into_iter()
        .map(|Row(prefix, max_len, origin, dates)| {
            format!(
                r#"{{"p":"{prefix}","m":{max_len},"o":{origin},"r":{}}}"#,
                serde_json::to_string(&dates).unwrap()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_v2(dir: &std::path::Path) -> RoasTrie {
    let jsonl_path = dir.join("fixture.jsonl");
    std::fs::write(&jsonl_path, jsonl()).unwrap();
    let rkyv_path = dir.join("fixture.rkyv");
    let mut builder = RoasTrieMut::from_jsonl(jsonl_path.to_str().unwrap()).unwrap();
    builder.dump(rkyv_path.to_str().unwrap()).unwrap();
    RoasTrie::open(rkyv_path.to_str().unwrap()).unwrap()
}

fn build_v3() -> SnapBackend {
    let mut by_prefix: BTreeMap<IpNet, Vec<RoaRecord>> = BTreeMap::new();
    for Row(prefix, max_len, origin, dates) in rows() {
        by_prefix
            .entry(prefix.parse().unwrap())
            .or_default()
            .push(RoaRecord {
                max_len,
                origin,
                dates,
            });
    }
    let records = by_prefix.values().map(|v| v.len() as u64).sum();
    let built = build_from_rows(by_prefix, records).unwrap();
    // the value type is what the publisher writes; build_from_rows is the same writer
    let _ = std::marker::PhantomData::<RoaSet>;
    SnapBackend::from_bytes(&built.bytes).unwrap()
}

fn day(n: i64) -> chrono::NaiveDate {
    chrono::DateTime::from_timestamp(BASE + n * DAY, 0)
        .unwrap()
        .date_naive()
}

#[test]
fn search_answers_match_across_backends() {
    let dir = std::env::temp_dir().join(format!("wayback-equivalence-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let v2 = build_v2(&dir);
    let v3 = build_v3();

    assert_eq!(v2.counts(), v3.counts(), "prefix counts per family");
    assert_eq!(v2.latest_date_ts(), v3.latest_date_ts(), "latest day");

    // a vacuous pass is worse than no test: pin what the fixture must answer
    let all = v3.search(None, None, None, None, None, false);
    assert_eq!(all.len(), 7, "every record in the fixture, and no others");
    let supernet = v3.search(
        Some("10.0.0.0/8".parse().unwrap()),
        None,
        None,
        None,
        None,
        false,
    );
    assert_eq!(supernet.len(), 3, "the supernet and the subnet below it");
    assert_eq!(v3.counts(), (4, 2), "four v4 prefixes, two v6");
    assert_eq!(
        v3.search(None, None, None, Some(day(5)), None, false).len(),
        4,
        "day five: both supernet origins and the v6 pair are in effect; the subnet ended on \
         day three, the disjoint record is in its gap, and the expired record ended on day four"
    );

    let queries = [
        (None, None, None, None, None, false),
        (Some("10.0.0.0/8"), None, None, None, None, false),
        (Some("10.0.0.0/8"), None, None, None, None, true),
        // a prefix inside the supernet, without exact: the v2 rule selects the subtree of
        // the shortest covering prefix, so this returns the supernet and its descendants
        (Some("10.1.2.0/24"), None, None, None, None, false),
        (Some("10.1.2.0/24"), None, None, None, None, true),
        // a prefix nobody holds: the subtree is empty
        (Some("203.0.113.0/24"), None, None, None, None, false),
        (Some("10.0.0.0/8"), Some(64500), None, None, None, false),
        (Some("10.0.0.0/8"), Some(64501), None, None, None, false),
        (Some("10.0.0.0/8"), Some(64599), None, None, None, false),
        (Some("10.0.0.0/8"), None, Some(24), None, None, false),
        (Some("10.0.0.0/8"), None, Some(16), None, None, false),
        (Some("2001:db8::/32"), None, None, None, None, false),
        (Some("2001:db8:1::/48"), None, None, None, None, true),
    ];
    for (prefix, origin, max_len, date, current, exact) in queries {
        let prefix = prefix.map(|p: &str| p.parse::<IpNet>().unwrap());
        assert_eq!(
            v2.search(prefix, origin, max_len, date, current, exact),
            v3.search(prefix, origin, max_len, date, current, exact),
            "search({prefix:?}, origin={origin:?}, max_len={max_len:?}, current={current:?}, \
             exact={exact})"
        );
    }

    // dated queries: inside a range, in a gap between two ranges, before everything
    for offset in [0, 4, 5, 9, 10, 11, 25] {
        let date = day(offset);
        assert_eq!(
            v2.search(None, None, None, Some(date), None, false),
            v3.search(None, None, None, Some(date), None, false),
            "dated search at day {offset}"
        );
        assert_eq!(
            v2.search(
                Some("198.51.100.0/24".parse().unwrap()),
                None,
                None,
                Some(date),
                None,
                false
            ),
            v3.search(
                Some("198.51.100.0/24".parse().unwrap()),
                None,
                None,
                Some(date),
                None,
                false
            ),
            "dated search on the disjoint record at day {offset}"
        );
    }

    // current=true is the object's last day, current=false is "expired before it"
    for current in [Some(true), Some(false), None] {
        assert_eq!(
            v2.search(None, None, None, None, current, false),
            v3.search(None, None, None, None, current, false),
            "current={current:?}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validation_answers_match_across_backends() {
    let dir = std::env::temp_dir().join(format!("wayback-equivalence-val-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let v2 = build_v2(&dir);
    let v3 = build_v3();

    let cases = [
        // (prefix, origin, day offset)
        ("10.1.2.0/24", 64500, 1), // covered by the supernet, same origin
        ("10.1.2.0/24", 64500, 6), // still covered, both origins in effect
        ("10.1.2.0/24", 64501, 6), // the other origin, max_len 16 does not admit a /24
        ("10.1.2.0/24", 64501, 1), // that origin is not in effect yet
        ("10.9.0.0/16", 64500, 1), // covered, origin matches, max_len admits
        ("203.0.113.0/24", 64500, 1), // nothing covers it
        ("198.51.100.0/24", 64503, 5), // in the gap between the two ranges
        ("198.51.100.0/24", 64503, 8), // back in effect
        ("2001:db8:2::/48", 64504, 3), // v6, covered by the /32
    ];
    for (prefix, origin, offset) in cases {
        let prefix: IpNet = prefix.parse().unwrap();
        let ts = BASE + offset * DAY;
        assert_eq!(
            v2.validate(&prefix, origin, ts),
            v3.validate(&prefix, origin, ts),
            "validate({prefix}, {origin}, day {offset})"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}
