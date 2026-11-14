//! Fixture tests for the TheTVDB and Bangumi parsers, against
//! hand-written responses in the shapes those APIs return
//! (TVDB API v4 envelope with `data`/`links`; Bangumi API v0
//! search / subject / episodes payloads).

use videotag_sources::{bangumi, pick_best, tvdb, IdPlatform, MediaType};

fn load_json(name: &str) -> serde_json::Value {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture present");
    serde_json::from_str(&text).expect("fixture is JSON")
}

// ---------------------------------------------------------------------------
// TheTVDB

#[test]
fn tvdb_login_token_parses() {
    let v = serde_json::json!({
        "status": "success",
        "data": { "token": "eyJhbGciOiJIUzI1NiJ9.fake.token" }
    });
    assert_eq!(
        tvdb::parse_login_token(&v).as_deref(),
        Some("eyJhbGciOiJIUzI1NiJ9.fake.token")
    );
    let no_token = serde_json::json!({ "status": "success", "data": {} });
    assert_eq!(tvdb::parse_login_token(&no_token), None);
}

#[test]
fn tvdb_search_fixture() {
    let cands = tvdb::parse_search_results(&load_json("tvdb_search.json"), MediaType::Tv);
    assert_eq!(cands.len(), 2);
    let c = &cands[0];
    assert_eq!(c.id, 78801); // spelled as a string in the payload
    assert_eq!(c.source, IdPlatform::Tvdb);
    assert_eq!(c.media_type, MediaType::Tv);
    assert_eq!(c.title, "101次求婚");
    assert_eq!(c.original_title, "101次求婚");
    assert_eq!(c.year, Some(1991));
    // The overview map resolves to a real translation.
    assert!(c.overview.contains("middle-aged"));
    // TVDB never carries a user rating.
    assert_eq!(c.vote_average, 0.0);
    assert_eq!(c.vote_count, 0);
    // Search results leave the English slot to the detail lookup.
    assert!(c.english_title.is_empty());
    // A numeric tvdb_id parses too.
    assert_eq!(cands[1].id, 255316);
    assert_eq!(cands[1].year, Some(2012));
}

#[test]
fn tvdb_series_extended_prefers_chinese_translation() {
    let v = load_json("tvdb_series_extended.json");
    let zh = tvdb::parse_extended(&v, MediaType::Tv, true).expect("extended parses");
    assert_eq!(zh.id, 78801);
    assert_eq!(zh.title, "101次求婚");
    assert_eq!(zh.original_title, "The 101st Proposal");
    assert_eq!(zh.english_title, "The 101st Proposal");
    assert_eq!(zh.year, Some(1991));
    assert!(zh.overview.contains("星野达郎"));
    assert_eq!(zh.vote_count, 0);

    let en = tvdb::parse_extended(&v, MediaType::Tv, false).expect("extended parses");
    assert_eq!(en.title, "The 101st Proposal");
    assert_eq!(en.year, Some(1991));
}

#[test]
fn tvdb_movie_extended_fixture() {
    let c = tvdb::parse_extended(&load_json("tvdb_movie_extended.json"), MediaType::Movie, true)
        .expect("movie extended parses");
    assert_eq!(c.id, 1181);
    assert_eq!(c.media_type, MediaType::Movie);
    assert_eq!(c.title, "乔布斯");
    assert_eq!(c.original_title, "Jobs");
    assert_eq!(c.english_title, "Jobs");
    assert_eq!(c.year, Some(2013));
}

#[test]
fn tvdb_episodes_page_fixture() {
    let (eps, next) = tvdb::parse_episodes_page(&load_json("tvdb_episodes.json"));
    assert_eq!(next, None);
    assert_eq!(eps.len(), 4);
    let s1: Vec<_> = eps.iter().filter(|(s, _)| *s == 1).collect();
    assert_eq!(s1.len(), 3);
    assert_eq!(s1[0].1.number, 1);
    assert_eq!(s1[0].1.title, "第一乐章：宿命的相亲");
    assert_eq!(s1[0].1.air_date.as_deref(), Some("1991-07-01"));
    // An empty `aired` string becomes None.
    assert_eq!(s1[2].1.air_date, None);
    // Season 0 (specials) is bucketed like any other season.
    let s0: Vec<_> = eps.iter().filter(|(s, _)| *s == 0).collect();
    assert_eq!(s0.len(), 1);
    assert_eq!(s0[0].1.title, "圣诞特别篇");
}

#[test]
fn tvdb_fill_episode_gaps_from_english() {
    use videotag_sources::Episode;
    // Chinese listing: episode present, title untranslated (""),
    // date known. English listing supplies the title.
    let mut primary = vec![(1u32, vec![
        Episode { number: 1, title: String::new(), air_date: Some("2004-10-03".into()) },
        Episode { number: 2, title: "主妇的抉择".into(), air_date: None },
    ])];
    let fallback = vec![(1u32, vec![
        Episode { number: 1, title: "Pilot".into(), air_date: Some("2004-10-03".into()) },
        Episode { number: 2, title: "Ah, But Underneath".into(), air_date: Some("2004-10-10".into()) },
    ])];
    tvdb::fill_episode_gaps(&mut primary, &fallback);
    assert_eq!(primary[0].1[0].title, "Pilot");
    // Existing Chinese data is never overwritten.
    assert_eq!(primary[0].1[1].title, "主妇的抉择");
    assert_eq!(primary[0].1[1].air_date.as_deref(), Some("2004-10-10"));
}

#[test]
fn tvdb_pick_best_over_search_fixture() {
    let cands = tvdb::parse_search_results(&load_json("tvdb_search.json"), MediaType::Tv);
    assert_eq!(pick_best(&cands, "101次求婚", Some(1991)), Some(0));
}

// ---------------------------------------------------------------------------
// Bangumi

#[test]
fn bgm_search_fixture_chinese() {
    let cands = bangumi::parse_search_results(&load_json("bgm_search.json"), true);
    assert_eq!(cands.len(), 2);
    let c = &cands[0];
    assert_eq!(c.id, 64131);
    assert_eq!(c.source, IdPlatform::Bangumi);
    assert_eq!(c.media_type, MediaType::Tv);
    assert_eq!(c.title, "进击的巨人");
    assert_eq!(c.original_title, "進撃の巨人");
    // The folder's "(English)" slot takes the original name.
    assert_eq!(c.english_title, "進撃の巨人");
    assert_eq!(c.year, Some(2013));
    // Real user ratings, unlike TVDB.
    assert_eq!(c.vote_average, 8.4);
    assert_eq!(c.vote_count, 5210);
    // A subject with no Chinese name falls back to the original.
    assert_eq!(cands[1].title, "進撃の巨人 総集編");
}

#[test]
fn bgm_search_fixture_english_mode_leads_with_original() {
    let cands = bangumi::parse_search_results(&load_json("bgm_search.json"), false);
    assert_eq!(cands[0].title, "進撃の巨人");
}

#[test]
fn bgm_subject_detail_fixture() {
    let c = bangumi::parse_subject(&load_json("bgm_subject.json"), true)
        .expect("subject parses");
    assert_eq!(c.id, 64131);
    assert_eq!(c.title, "进击的巨人");
    assert_eq!(c.year, Some(2013));
    assert_eq!(c.vote_average, 8.4);
    assert_eq!(c.vote_count, 5210);
    assert!(c.overview.contains("巨人"));
}

#[test]
fn bgm_episodes_page_fixture() {
    let (eps, total) = bangumi::parse_episodes_page(&load_json("bgm_episodes.json"));
    assert_eq!(total, 25);
    // The type-1 (SP) row and the ep-less 2.5 row are skipped.
    assert_eq!(eps.len(), 2);
    assert_eq!(eps[0].number, 1);
    assert_eq!(eps[0].title, "致二千年后的你");
    assert_eq!(eps[0].air_date.as_deref(), Some("2013-04-06"));
    // An empty name_cn falls back to the original name.
    assert_eq!(eps[1].number, 2);
    assert_eq!(eps[1].title, "その日 ―シガンシナ陥落②―");
}

#[test]
fn bgm_pick_best_over_search_fixture() {
    let cands = bangumi::parse_search_results(&load_json("bgm_search.json"), true);
    assert_eq!(pick_best(&cands, "进击的巨人", Some(2013)), Some(0));
}

#[test]
fn malformed_platform_payloads_yield_nothing() {
    let junk = serde_json::json!({ "data": "not-an-array" });
    assert!(tvdb::parse_search_results(&junk, MediaType::Tv).is_empty());
    assert!(bangumi::parse_search_results(&junk, true).is_empty());
    let no_id = serde_json::json!({ "data": { "name": "no id" } });
    assert!(tvdb::parse_extended(&no_id, MediaType::Tv, true).is_none());
    let no_id = serde_json::json!({ "name": "no id" });
    assert!(bangumi::parse_subject(&no_id, true).is_none());
}
