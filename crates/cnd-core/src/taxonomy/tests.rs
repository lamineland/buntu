//! Golden tests on real category labels (PRD-33 lot 33.1).
//!
//! `tests/fixtures/category_labels.tsv` holds `kind  label  country  themes` for every label of a
//! real catalogue plus common Xtream labels. After a deliberate change of the rules, rewrite it with
//! `TAXONOMY_BLESS=1 cargo test -p cnd-core --lib taxonomy::tests`, review the diff, then raise
//! `TAXONOMY_VERSION`.

use super::*;
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/category_labels.tsv")
}

fn describe(kind: &str, label: &str) -> (String, String) {
    let c = classify(&ClassifyInput { kind, group_title: Some(label), tvg_id: None, name: "", country_attr: None });
    let country = c.country_code().unwrap_or("-").to_string();
    let themes = c
        .themes
        .iter()
        .map(|t| match t.subtheme {
            Some(sub) => format!("{}/{}", t.theme.as_str(), sub),
            None => t.theme.as_str().to_string(),
        })
        .collect::<Vec<_>>()
        .join(",");
    (country, themes)
}

#[test]
fn real_labels_match_the_reviewed_golden_file() {
    let text = std::fs::read_to_string(fixture_path()).expect("fixture");
    let bless = std::env::var_os("TAXONOMY_BLESS").is_some();
    let mut rewritten = String::new();
    let mut mismatches = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        let (kind, label) = (cols[0], cols[1]);
        let (country, themes) = describe(kind, label);
        rewritten.push_str(&format!("{kind}\t{label}\t{country}\t{themes}\n"));
        let expected = (cols.get(2).copied().unwrap_or(""), cols.get(3).copied().unwrap_or(""));
        if expected != (country.as_str(), themes.as_str()) {
            mismatches.push(format!("{label:?}: expected {expected:?}, got ({country:?}, {themes:?})"));
        }
    }
    if bless {
        std::fs::write(fixture_path(), rewritten).expect("write fixture");
        return;
    }
    assert!(mismatches.is_empty(), "{} label(s) changed:\n{}", mismatches.len(), mismatches.join("\n"));
}

#[test]
fn no_combined_label_stays_unclassified_when_one_part_is_known() {
    let text = std::fs::read_to_string(fixture_path()).expect("fixture");
    for line in text.lines() {
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() < 2 || !cols[1].contains(';') {
            continue;
        }
        let (_, themes) = describe(cols[0], cols[1]);
        assert_ne!(themes, "other", "{:?}", cols[1]);
    }
}

#[test]
fn every_theme_round_trips_through_its_stored_name() {
    for theme in Theme::ALL {
        assert_eq!(Theme::parse(theme.as_str()), Some(theme));
    }
    assert_eq!(Theme::parse("weather"), None);
}

#[test]
fn a_user_correction_replaces_the_values_but_never_drops_adult() {
    let input = ClassifyInput { kind: "live", group_title: Some("News"), tvg_id: Some("X.fr"), name: "x", country_attr: None };
    let correction = TaxonomyOverride { country: Some("BE".into()), themes: Some(vec![Theme::Sports]) };
    let c = classify_with_override(&input, Some(&correction));
    assert_eq!(c.country, Some(CountryMatch { code: "BE".into(), source: CountrySource::User }));
    assert_eq!(c.primary_theme(), Theme::Sports);

    let adult = ClassifyInput { kind: "live", group_title: Some("XXX"), tvg_id: None, name: "x", country_attr: None };
    let c = classify_with_override(&adult, Some(&TaxonomyOverride { country: None, themes: Some(vec![Theme::Kids]) }));
    assert!(c.has_theme(Theme::Adult));
    assert_eq!(c.primary_theme(), Theme::Adult);
}

/// SLA: < 5 µs per item (PRD-33). Timed only in optimised builds (`cargo test --release`).
#[test]
fn classifying_fifty_thousand_items_is_fast() {
    if cfg!(debug_assertions) {
        return;
    }
    let labels = ["Animation;Kids;Music", "FR| SPORTS FHD", "Italy", "General", "Undefined", "|UK| ENTERTAINMENT"];
    let ids = ["DHA.tr@SD", "BBCOne.uk", "", "Foo.tv"];
    let start = std::time::Instant::now();
    for i in 0..50_000 {
        let input = ClassifyInput {
            kind: "live",
            group_title: Some(labels[i % labels.len()]),
            tvg_id: Some(ids[i % ids.len()]),
            name: "|FR| Some Channel HD",
            country_attr: None,
        };
        std::hint::black_box(classify(&input));
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 250, "50 000 items took {elapsed:?}");
}

/// Coverage on a real catalogue (PRD-33 § 13): at least 95 % of live channels get a country. The
/// share left as `other` is printed only: labels such as `Undefined` or a bare country name carry
/// no theme, and the 15 % goal needs lots 33.2 (theme borrowed through the `tvg-id`) and 33.5 (EPG).
/// Read-only; run with
/// `CND_TAXONOMY_DB=/path/to/cnd_iptv.db cargo test -p cnd-core --lib --release taxonomy::tests::coverage -- --ignored --nocapture`.
#[test]
#[ignore = "needs a real catalogue: set CND_TAXONOMY_DB"]
fn coverage_on_a_real_catalogue() {
    let Some(path) = std::env::var_os("CND_TAXONOMY_DB") else { return };
    let conn = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open db");
    let mut stmt = conn
        .prepare("SELECT kind, group_title, tvg_id, name, country FROM iptv_items")
        .expect("query");
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .expect("rows");
    let (mut live, mut with_country, mut other) = (0usize, 0usize, 0usize);
    let mut by_source: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut unclassified: std::collections::BTreeMap<String, usize> = Default::default();
    for row in rows {
        let (kind, group, tvg_id, name, country) = row.expect("row");
        if kind != "live" {
            continue;
        }
        let c = classify(&ClassifyInput {
            kind: &kind,
            group_title: group.as_deref(),
            tvg_id: tvg_id.as_deref(),
            name: &name,
            country_attr: country.as_deref().filter(|c| !c.is_empty()),
        });
        live += 1;
        if let Some(m) = &c.country {
            with_country += 1;
            *by_source.entry(m.source.as_str()).or_default() += 1;
        }
        if c.primary_theme() == Theme::Other {
            other += 1;
            *unclassified.entry(group.unwrap_or_default()).or_default() += 1;
        }
    }
    let pct = |n: usize| 100.0 * n as f64 / live.max(1) as f64;
    println!("live channels: {live}");
    println!("with a country: {with_country} ({:.1} %), by source: {by_source:?}", pct(with_country));
    println!("theme `other`: {other} ({:.1} %), by label: {unclassified:?}", pct(other));
    assert!(pct(with_country) >= 95.0);
}
