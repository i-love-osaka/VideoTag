//! Live Bangumi smoke test. Ignored by default (it needs the
//! network); run explicitly:
//!
//! ```sh
//! cargo test -p videotag-sources --test live_bangumi -- --ignored
//! ```
//!
//! Bangumi is keyless, so no environment setup is needed.

use videotag_sources::{bangumi, pick_best, TmdbLanguage};

#[test]
#[ignore]
fn live_search_subject_and_episodes() {
    let cands = bangumi::search_subjects("进击的巨人", TmdbLanguage::ZhCn)
        .expect("bangumi search");
    assert!(!cands.is_empty(), "expected Bangumi results for 进击的巨人");
    let best = pick_best(&cands, "进击的巨人", Some(2013)).expect("a best pick");
    let show = &cands[best];
    assert_eq!(show.title, "进击的巨人");

    let detail = bangumi::get_subject(show.id, TmdbLanguage::ZhCn)
        .expect("subject request")
        .expect("subject present");
    assert_eq!(detail.id, show.id);
    assert_eq!(detail.year, Some(2013));

    let episodes = bangumi::get_episodes(show.id).expect("episodes request");
    assert!(!episodes.is_empty(), "expected episodes for the subject");
    assert_eq!(episodes[0].number, 1);

    // A far-out id must come back None, not Err.
    let missing = bangumi::get_subject(999_999_999, TmdbLanguage::ZhCn)
        .expect("missing id request");
    assert!(missing.is_none());
}
