//! Live TheTVDB smoke test. Ignored by default (it needs the
//! network and an API key); run explicitly:
//!
//! ```sh
//! VIDEO_TVDB_KEY=<key> cargo test -p videotag-sources --test live_tvdb -- --ignored
//! ```
//!
//! Written after a fixture-drift bug: the episodes
//! fixture used the documented field name `episodeNumber`,
//! while the live API actually sends `number` — every episode
//! was silently dropped and all TVDB renames fell back to
//! placeholder titles. This test pins the client to the real
//! payload shape.

use videotag_sources::{pick_best, tvdb, TmdbLanguage};

#[test]
#[ignore]
fn live_search_and_episodes_desperate_housewives() {
    let key = std::env::var("VIDEO_TVDB_KEY").expect("VIDEO_TVDB_KEY env var");
    let creds = tvdb::TvdbCredentials { api_key: key, pin: String::new() };

    let cands = tvdb::search_tv(&creds, "Desperate Housewives", None, TmdbLanguage::ZhCn)
        .expect("tvdb search");
    assert!(!cands.is_empty(), "expected TVDB results");
    let best = pick_best(&cands, "Desperate Housewives", Some(2004)).expect("a best pick");
    assert_eq!(cands[best].id, 73800);

    let seasons = tvdb::get_episodes(&creds, 73800, TmdbLanguage::ZhCn)
        .expect("episodes request");
    let s1 = seasons
        .iter()
        .find(|(s, _)| *s == 1)
        .expect("season 1 present");
    assert!(s1.1.len() >= 23, "season 1 episode count, got {}", s1.1.len());
    let e1 = &s1.1[0];
    assert_eq!(e1.number, 1);
    assert!(!e1.title.is_empty(), "episode 1 must carry a real title");
    assert_eq!(e1.air_date.as_deref(), Some("2004-10-03"));
}
