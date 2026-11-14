//! Live TMDB smoke test. Ignored by default; run explicitly with a
//! real key:
//!
//! ```sh
//! TMDB_API_KEY=xxxx cargo test -p videotag-sources --test live -- --ignored
//! ```

use videotag_sources::{
    get_episodes, get_tv, pick_best, search_movie, search_tv, TmdbLanguage,
};

fn key() -> String {
    std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set for the live test")
}

#[test]
#[ignore]
fn live_search_and_fetch() {
    let key = key();

    let tv = search_tv(&key, "红楼梦", None, TmdbLanguage::ZhCn).expect("search tv");
    assert!(!tv.is_empty(), "expected TV results for 红楼梦");
    let best = pick_best(&tv, "红楼梦", None).expect("a best pick");
    let show = &tv[best];

    let detail = get_tv(&key, show.id, TmdbLanguage::ZhCn)
        .expect("tv detail request")
        .expect("tv detail present");
    assert_eq!(detail.id, show.id);

    let episodes = get_episodes(&key, show.id, 1, TmdbLanguage::ZhCn).expect("season 1 episodes");
    assert!(!episodes.is_empty(), "expected episodes in season 1");
    assert_eq!(episodes[0].number, 1);

    let movies = search_movie(
        &key,
        "Captain America: Brave New World",
        Some(2025),
        TmdbLanguage::ZhCn,
    )
        .expect("search movie");
    assert!(!movies.is_empty(), "expected movie results");

    // A made-up id far beyond any real one must come back None, not Err.
    let missing = get_tv(&key, 999_999_999, TmdbLanguage::ZhCn).expect("missing id request");
    assert!(missing.is_none());
}
