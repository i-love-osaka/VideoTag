//! Fixture tests: every parser is checked against hand-written
//! responses in the exact shape TMDB API v3 returns (field set and
//! nesting mirror real `/search/*`, detail and season payloads).

use videotag_sources::tmdb;
use videotag_sources::{pick_best, MediaType};

fn load(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("fixture present")
}

fn load_json(name: &str) -> serde_json::Value {
    serde_json::from_str(&load(name)).expect("fixture is JSON")
}

#[test]
fn search_tv_fixture() {
    let cands = tmdb::parse_search_results(&load_json("search_tv.json"), MediaType::Tv);
    assert_eq!(cands.len(), 3);
    let c = &cands[0];
    assert_eq!(c.id, 8374756);
    assert_eq!(c.media_type, MediaType::Tv);
    assert_eq!(c.title, "红楼梦");
    assert_eq!(c.original_title, "红楼梦");
    assert_eq!(c.year, Some(2010));
    assert!(c.overview.contains("贾宝玉"));
    assert!(c.popularity > 0.0);
    assert_eq!(c.vote_average, 6.4);
    assert_eq!(c.vote_count, 18);
    // Search results never carry the en-US title (details do).
    assert!(c.english_title.is_empty());
    // Empty-string-free dates parse in every row.
    assert_eq!(cands[1].year, Some(1987));
}

#[test]
fn search_movie_fixture_uses_movie_fields() {
    let cands = tmdb::parse_search_results(&load_json("search_movie.json"), MediaType::Movie);
    assert_eq!(cands.len(), 2);
    let c = &cands[0];
    assert_eq!(c.media_type, MediaType::Movie);
    assert_eq!(c.title, "美国队长4");
    assert_eq!(c.original_title, "Captain America: Brave New World");
    assert_eq!(c.year, Some(2025));
    assert_eq!(c.vote_average, 6.1);
    assert_eq!(c.vote_count, 1950);
}

#[test]
fn english_title_comes_from_en_payload() {
    // The same detail object fetched with language=en-US carries
    // its title in the usual field; that is what gets stored.
    let en = serde_json::json!({"id": 115782, "title": "Jobs", "original_title": "Jobs"});
    assert_eq!(tmdb::english_title_of(&en, MediaType::Movie), "Jobs");
    let en_tv = serde_json::json!({"id": 1396, "name": "Breaking Bad"});
    assert_eq!(
        tmdb::english_title_of(&en_tv, MediaType::Tv),
        "Breaking Bad"
    );
    let no_title = serde_json::json!({"id": 1});
    assert_eq!(tmdb::english_title_of(&no_title, MediaType::Movie), "");
}

#[test]
fn tv_detail_fixture() {
    let c = tmdb::parse_candidate(&load_json("tv_detail.json"), MediaType::Tv)
        .expect("detail parses");
    assert_eq!(c.id, 8374756);
    assert_eq!(c.title, "红楼梦");
    assert_eq!(c.year, Some(2010));
}

#[test]
fn movie_detail_fixture() {
    let c = tmdb::parse_candidate(&load_json("movie_detail.json"), MediaType::Movie)
        .expect("detail parses");
    assert_eq!(c.id, 822119);
    assert_eq!(c.title, "美国队长4");
    assert_eq!(c.original_title, "Captain America: Brave New World");
    assert_eq!(c.year, Some(2025));
    assert_eq!(c.vote_average, 6.1);
    assert_eq!(c.vote_count, 1950);
}

#[test]
fn season_fixture_episodes() {
    let eps = tmdb::parse_episodes(&load_json("season.json"));
    assert_eq!(eps.len(), 3);
    assert_eq!(eps[0].number, 1);
    assert_eq!(eps[0].title, "太虚幻境");
    assert_eq!(eps[0].air_date.as_deref(), Some("2010-06-24"));
    assert_eq!(eps[1].number, 2);
    // An empty air_date string from TMDB becomes None.
    assert_eq!(eps[2].number, 3);
    assert_eq!(eps[2].air_date, None);
    assert_eq!(eps[2].title, "黛玉进府");
}

#[test]
fn pick_best_over_parsed_search_results() {
    let cands = tmdb::parse_search_results(&load_json("search_tv.json"), MediaType::Tv);
    // Exact title + exact year (2010) beats the more popular 1987
    // edition and the most popular knockoff.
    assert_eq!(pick_best(&cands, "红楼梦", Some(2010)), Some(0));
    // Without a year, exact title ties are broken by popularity:
    // the 1987 edition (21.875) over the 2010 one (9.412).
    assert_eq!(pick_best(&cands, "红楼梦", None), Some(1));
    // Year 1987 selects the 1987 edition outright.
    assert_eq!(pick_best(&cands, "红楼梦", Some(1987)), Some(1));
    // Off-by-one year tolerance: 2011 still points at the 2010 edition.
    assert_eq!(pick_best(&cands, "红楼梦", Some(2011)), Some(0));
}

#[test]
fn pick_best_matches_via_original_title() {
    let cands = tmdb::parse_search_results(&load_json("search_movie.json"), MediaType::Movie);
    // The zh-CN title is 美国队长4; the English query can only hit
    // original_title.
    assert_eq!(
        pick_best(&cands, "Captain America: Brave New World", None),
        Some(0)
    );
    assert_eq!(pick_best(&cands, "美国队长", Some(2011)), Some(1));
}

#[test]
fn malformed_payloads_yield_nothing() {
    let junk = serde_json::json!({"results": "not-an-array"});
    assert!(tmdb::parse_search_results(&junk, MediaType::Tv).is_empty());
    let junk = serde_json::json!({"episodes": [{"name": "no number"}]});
    assert!(tmdb::parse_episodes(&junk).is_empty());
    let no_id = serde_json::json!({"name": "no id"});
    assert!(tmdb::parse_candidate(&no_id, MediaType::Tv).is_none());
}
