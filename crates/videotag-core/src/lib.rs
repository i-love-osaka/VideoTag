//! Pure parsing helpers for VideoTag.
//!
//! Everything in this crate works on path *strings* only: no filesystem
//! access, no network. It extracts titles, years, TMDB ids, season/episode
//! numbers and technical tags from directory and file names, and builds the
//! default target layout used by the renamer.

use std::ops::Range;

/// One of the online catalogues a work can be identified on. A
/// name can carry an id for exactly one of them (the folder-name
/// tags are `[TMDB-…]`, `[TVDB-…]`, `[BGM-…]`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IdPlatform {
    #[default]
    Tmdb,
    Tvdb,
    Bangumi,
}

impl IdPlatform {
    /// The tag written into library folder names before the id:
    /// `[TMDB-00048455]`, `[TVDB-00001234]`, `[BGM-00123456]`.
    pub fn tag(self) -> &'static str {
        match self {
            IdPlatform::Tmdb => "TMDB",
            IdPlatform::Tvdb => "TVDB",
            IdPlatform::Bangumi => "BGM",
        }
    }
}

/// Everything extracted from one directory or file name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedName {
    /// Cleaned title suitable for searching (logical form, half-width colon).
    pub title: String,
    pub year: Option<u16>,
    pub tmdb_id: Option<u64>,
    /// TheTVDB id, from a `[TVDB-…]` / `tvdb-…` tag.
    pub tvdb_id: Option<u64>,
    /// Bangumi subject id, from a `[BGM-…]` / `bgm-…` tag.
    pub bgm_id: Option<u64>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Last episode for multi-episode files (`S01E01E02`, `S01E01-E02`).
    pub episode_end: Option<u32>,
    /// Segment label for multi-part movies, e.g. `CD2`, `Disc 1`, `Part 1`.
    pub part: Option<String>,
    /// True for side files marked sample / trailer / 预告.
    pub is_extra: bool,
    /// Technical tags in the order they appeared in the original name,
    /// e.g. `["2160p", "AVC", "8bit", "AAC", "2.0"]`.
    pub tech_tags: Vec<String>,
}

impl ParsedName {
    /// The id this name carries for one specific platform, if
    /// any — the per-platform lookup the manual platform buttons
    /// run (another platform's id does not count here).
    pub fn id_for(&self, platform: IdPlatform) -> Option<u64> {
        match platform {
            IdPlatform::Tmdb => self.tmdb_id,
            IdPlatform::Tvdb => self.tvdb_id,
            IdPlatform::Bangumi => self.bgm_id,
        }
    }

    /// The platform-tagged id this name carried, if any. TMDB
    /// wins when several tags are present (it is the catalogue
    /// this app's library format grew up with); a name written
    /// by the app itself only ever carries one.
    pub fn platform_id(&self) -> Option<(IdPlatform, u64)> {
        if let Some(id) = self.tmdb_id {
            return Some((IdPlatform::Tmdb, id));
        }
        if let Some(id) = self.tvdb_id {
            return Some((IdPlatform::Tvdb, id));
        }
        if let Some(id) = self.bgm_id {
            return Some((IdPlatform::Bangumi, id));
        }
        None
    }
}

// ---------------------------------------------------------------------------
// small helpers

fn last_component(input: &str) -> &str {
    let trimmed = input.trim().trim_end_matches(|c| c == '/' || c == '\\');
    trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .unwrap_or("")
        .trim()
}

fn ascii_lower_bytes(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b.to_ascii_lowercase()).collect()
}

fn ascii_upper_bytes(s: &str) -> Vec<u8> {
    s.bytes().map(|b| b.to_ascii_uppercase()).collect()
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_digits(bytes: &[u8], start: usize, max: usize) -> Option<(u32, usize)> {
    let mut value: u32 = 0;
    let mut end = start;
    while end < bytes.len() && end - start < max && bytes[end].is_ascii_digit() {
        value = value * 10 + (bytes[end] - b'0') as u32;
        end += 1;
    }
    if end == start {
        None
    } else {
        Some((value, end))
    }
}

const MEDIA_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "wmv", "flv", "ts", "m2ts", "rmvb", "webm", "mpg", "mpeg", "iso",
    "srt", "ass", "ssa", "vtt", "sub", "sup",
];

fn strip_media_extension(name: &str) -> &str {
    if let Some(dot) = name.rfind('.') {
        if dot > 0 {
            let ext = &name[dot + 1..];
            let lower = ext.to_ascii_lowercase();
            if MEDIA_EXTENSIONS.contains(&lower.as_str()) {
                return &name[..dot];
            }
        }
    }
    name
}

fn remove_ranges(s: &str, ranges: &[Range<usize>]) -> String {
    if ranges.is_empty() {
        return s.to_string();
    }
    let mut sorted: Vec<Range<usize>> = ranges
        .iter()
        .filter(|r| r.start < r.end && r.end <= s.len())
        .cloned()
        .collect();
    sorted.sort_by_key(|r| r.start);
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    for r in sorted {
        if r.start < cursor {
            continue;
        }
        out.push_str(&s[cursor..r.start]);
        out.push(' ');
        cursor = r.end;
    }
    out.push_str(&s[cursor..]);
    out
}

// ---------------------------------------------------------------------------
// Platform ids (TMDB / TVDB / Bangumi) / year

/// A platform id tag: the platform's needle (`tmdb`, `tvdb`,
/// `bgm`) not glued to a preceding token character, optionally
/// followed by `-` or `=`, then the digits. Covers `TMDB48455`,
/// `[TMDB-00048455]`, `{tmdb=48455}`, `[TVDB-00001234]`,
/// `[BGM-00123456]` alike.
fn find_tagged_id(input: &str, needle: &[u8]) -> Option<(u64, Range<usize>)> {
    let bytes = ascii_lower_bytes(input);
    let mut from = 0usize;
    while let Some(rel) = find_subslice(&bytes[from..], needle) {
        let i = from + rel;
        if i > 0 && is_token_byte(bytes[i - 1]) {
            from = i + 1;
            continue;
        }
        let mut p = i + needle.len();
        if p < bytes.len() && (bytes[p] == b'-' || bytes[p] == b'=') {
            p += 1;
        }
        let digits_start = p;
        let mut value: u64 = 0;
        let mut digits = 0usize;
        while p < bytes.len() && bytes[p].is_ascii_digit() {
            value = value
                .saturating_mul(10)
                .saturating_add((bytes[p] - b'0') as u64);
            p += 1;
            digits += 1;
        }
        if digits > 0 {
            let _ = digits_start;
            return Some((value, i..p));
        }
        from = i + 1;
    }
    None
}

fn find_tmdb_id(input: &str) -> Option<(u64, Range<usize>)> {
    find_tagged_id(input, b"tmdb")
}

fn find_tvdb_id(input: &str) -> Option<(u64, Range<usize>)> {
    find_tagged_id(input, b"tvdb")
}

fn find_bgm_id(input: &str) -> Option<(u64, Range<usize>)> {
    find_tagged_id(input, b"bgm")
}

fn find_year(input: &str) -> Option<(u16, Range<usize>)> {
    let bytes = input.as_bytes();
    let mut best: Option<(u8, usize, u16, Range<usize>)> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let end = i;
            if end - start == 4 {
                let value: u32 = input[start..end].parse().unwrap_or(0);
                if (1900..=2099).contains(&value) {
                    let prev = if start > 0 { bytes[start - 1] } else { 0 };
                    let next = if end < bytes.len() { bytes[end] } else { 0 };
                    let bracketed =
                        (prev == b'(' && next == b')') || (prev == b'[' && next == b']');
                    let score = if bracketed { 2 } else { 1 };
                    let replace = match &best {
                        None => true,
                        Some((bs, bstart, _, _)) => {
                            score > *bs || (score == *bs && start > *bstart)
                        }
                    };
                    if replace {
                        best = Some((score, start, value as u16, start..end));
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    best.map(|(_, _, v, r)| (v, r))
}

// ---------------------------------------------------------------------------
// Chinese numerals (1-99, plus 百 for completeness)

fn chinese_digit(ch: char) -> Option<u32> {
    match ch {
        '零' => Some(0),
        '一' => Some(1),
        '二' | '两' => Some(2),
        '三' => Some(3),
        '四' => Some(4),
        '五' => Some(5),
        '六' => Some(6),
        '七' => Some(7),
        '八' => Some(8),
        '九' => Some(9),
        _ => None,
    }
}

fn parse_chinese_or_arabic_number(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse().ok();
    }
    let mut total: u32 = 0;
    let mut pending: Option<u32> = None;
    let mut seen_any = false;
    for ch in text.chars() {
        if let Some(d) = chinese_digit(ch) {
            pending = Some(d);
            seen_any = true;
        } else if ch == '十' {
            total += pending.unwrap_or(1) * 10;
            pending = None;
            seen_any = true;
        } else if ch == '百' {
            total += pending.unwrap_or(1) * 100;
            pending = None;
            seen_any = true;
        } else {
            return None;
        }
    }
    if let Some(d) = pending {
        total += d;
    }
    if seen_any {
        Some(total)
    } else {
        None
    }
}

/// Finds `第X<marker>` and returns the number plus the byte range covered.
fn find_chinese_marker(input: &str, marker: char) -> Option<(u32, Range<usize>)> {
    let mut search_from = 0usize;
    while let Some(rel) = input[search_from..].find('第') {
        let start = search_from + rel;
        let after = start + '第'.len_utf8();
        if let Some(rel_end) = input[after..].find(marker) {
            let end_inner = after + rel_end;
            let inner = &input[after..end_inner];
            if inner.chars().count() <= 4 {
                if let Some(value) = parse_chinese_or_arabic_number(inner) {
                    return Some((value, start..end_inner + marker.len_utf8()));
                }
            }
        }
        search_from = after;
    }
    None
}

// ---------------------------------------------------------------------------
// season markers

fn find_season_marker(input: &str) -> Option<(u32, Range<usize>)> {
    if let Some(found) = find_chinese_marker(input, '季') {
        return Some(found);
    }
    let lower = ascii_lower_bytes(input);
    // "Specials" / 特别篇 -> season 0
    if let Some(pos) = find_subslice(&lower, b"specials") {
        let before_ok = pos == 0 || !is_token_byte(lower[pos - 1]);
        let end = pos + "specials".len();
        let after_ok = end >= lower.len() || !is_token_byte(lower[end]);
        if before_ok && after_ok {
            return Some((0, pos..end));
        }
    }
    if let Some(pos) = input.find("特别篇") {
        return Some((0, pos..pos + "特别篇".len()));
    }
    // "Season 01"
    if let Some(pos) = find_subslice(&lower, b"season") {
        let mut p = pos + "season".len();
        while p < lower.len() && matches!(lower[p], b' ' | b'-' | b'_' | b'.') {
            p += 1;
        }
        if let Some((value, end)) = parse_digits(&lower, p, 3) {
            return Some((value, pos..end));
        }
    }
    // Standalone S token: S01 / S1
    let upper = ascii_upper_bytes(input);
    for i in 0..upper.len() {
        if upper[i] == b'S' && (i == 0 || !is_token_byte(upper[i - 1])) {
            if let Some((value, end)) = parse_digits(&upper, i + 1, 2) {
                if end >= upper.len() || !is_token_byte(upper[end]) {
                    return Some((value, i..end));
                }
            }
        }
    }
    None
}

/// Season number from a season directory name (`S01`, `Season 1`, `第1季`,
/// `第一季`, `Specials`, `第0季` -> 0,...). Returns `None` when the name
/// carries no season marker.
pub fn parse_season_directory_name(input: &str) -> Option<u32> {
    let component = last_component(input);
    find_season_marker(component).map(|(v, _)| v)
}

// ---------------------------------------------------------------------------
// episode markers

#[derive(Debug, Clone, Copy)]
struct EpisodeInfo {
    season: Option<u32>,
    episode: Option<u32>,
    episode_end: Option<u32>,
    start: usize,
    end: usize,
}

fn find_sxx_exx(input: &str) -> Option<EpisodeInfo> {
    let upper = ascii_upper_bytes(input);
    for i in 0..upper.len() {
        if upper[i] != b'S' || (i > 0 && is_token_byte(upper[i - 1])) {
            continue;
        }
        let (season, p) = match parse_digits(&upper, i + 1, 3) {
            Some(v) => v,
            None => continue,
        };
        if p >= upper.len() || upper[p] != b'E' {
            continue;
        }
        let (episode, mut q) = match parse_digits(&upper, p + 1, 3) {
            Some(v) => v,
            None => continue,
        };
        let mut episode_end = None;
        loop {
            if q < upper.len() && upper[q] == b'E' {
                if let Some((v, end)) = parse_digits(&upper, q + 1, 3) {
                    episode_end = Some(v);
                    q = end;
                    continue;
                }
            }
            if q < upper.len() && upper[q] == b'-' {
                let mut r = q + 1;
                if r < upper.len() && upper[r] == b'E' {
                    r += 1;
                }
                if let Some((v, end)) = parse_digits(&upper, r, 3) {
                    episode_end = Some(v);
                    q = end;
                    continue;
                }
            }
            break;
        }
        if q < upper.len() && is_token_byte(upper[q]) {
            continue;
        }
        return Some(EpisodeInfo {
            season: Some(season),
            episode: Some(episode),
            episode_end,
            start: i,
            end: q,
        });
    }
    None
}

fn find_special_episode(input: &str) -> Option<EpisodeInfo> {
    let upper = ascii_upper_bytes(input);
    for token in [&b"SP"[..], &b"OVA"[..]] {
        let mut from = 0usize;
        while let Some(rel) = find_subslice(&upper[from..], token) {
            let i = from + rel;
            let end_token = i + token.len();
            let before_ok = i == 0 || !is_token_byte(upper[i - 1]);
            let after_ok = end_token >= upper.len() || !is_token_byte(upper[end_token]);
            if before_ok && (after_ok || upper[end_token].is_ascii_digit()) {
                let (episode, end) = match parse_digits(&upper, end_token, 3) {
                    Some((v, e)) => (Some(v), e),
                    None => (None, end_token),
                };
                return Some(EpisodeInfo {
                    season: Some(0),
                    episode,
                    episode_end: None,
                    start: i,
                    end,
                });
            }
            from = i + 1;
        }
    }
    if let Some(pos) = input.find("番外") {
        let start = pos;
        let after = pos + "番外".len();
        if let Some((ep, range)) = find_chinese_marker(&input[after..], '集') {
            return Some(EpisodeInfo {
                season: Some(0),
                episode: Some(ep),
                episode_end: None,
                start,
                end: after + range.end,
            });
        }
        let bytes = input.as_bytes();
        let mut p = after;
        while p < bytes.len() && matches!(bytes[p], b' ' | b'-' | b'_' | b'.') {
            p += 1;
        }
        if let Some((ep, end)) = parse_digits(bytes, p, 3) {
            return Some(EpisodeInfo {
                season: Some(0),
                episode: Some(ep),
                episode_end: None,
                start,
                end,
            });
        }
        return Some(EpisodeInfo {
            season: Some(0),
            episode: None,
            episode_end: None,
            start,
            end: after,
        });
    }
    None
}

fn find_standalone_episode(input: &str) -> Option<EpisodeInfo> {
    let upper = ascii_upper_bytes(input);
    // EP01 first (longer token wins over plain E01 at the same spot).
    let mut from = 0usize;
    while let Some(rel) = find_subslice(&upper[from..], b"EP") {
        let i = from + rel;
        if i == 0 || !is_token_byte(upper[i - 1]) {
            if let Some((ep, end)) = parse_digits(&upper, i + 2, 3) {
                if end >= upper.len() || !is_token_byte(upper[end]) {
                    return Some(EpisodeInfo {
                        season: None,
                        episode: Some(ep),
                        episode_end: None,
                        start: i,
                        end,
                    });
                }
            }
        }
        from = i + 1;
    }
    for i in 0..upper.len() {
        if upper[i] == b'E' && (i == 0 || !is_token_byte(upper[i - 1])) {
            if let Some((ep, end)) = parse_digits(&upper, i + 1, 3) {
                if end >= upper.len() || !is_token_byte(upper[end]) {
                    return Some(EpisodeInfo {
                        season: None,
                        episode: Some(ep),
                        episode_end: None,
                        start: i,
                        end,
                    });
                }
            }
        }
    }
    None
}

fn parse_episode_info(input: &str) -> Option<EpisodeInfo> {
    if let Some(info) = find_sxx_exx(input) {
        return Some(info);
    }
    if let Some(info) = find_special_episode(input) {
        return Some(info);
    }
    let chinese_episode = find_chinese_marker(input, '集')
        .or_else(|| find_chinese_marker(input, '话'))
        .or_else(|| find_chinese_marker(input, '話'));
    if let Some((ep, range)) = chinese_episode {
        let (season, start) = match find_chinese_marker(input, '季') {
            Some((s, srange)) => (Some(s), srange.start.min(range.start)),
            None => (None, range.start),
        };
        return Some(EpisodeInfo {
            season,
            episode: Some(ep),
            episode_end: None,
            start,
            end: range.end,
        });
    }
    let season_marker = find_season_marker(input);
    if let Some(mut info) = find_standalone_episode(input) {
        if info.season.is_none() {
            info.season = season_marker.map(|(v, _)| v);
        }
        return Some(info);
    }
    season_marker.map(|(v, range)| EpisodeInfo {
        season: Some(v),
        episode: None,
        episode_end: None,
        start: range.start,
        end: range.end,
    })
}

// ---------------------------------------------------------------------------
// multi-part markers (CD1 / Disc 1 / Part 1)

fn find_part(input: &str) -> Option<(String, Range<usize>)> {
    let lower = ascii_lower_bytes(input);
    let mut best: Option<(usize, String, Range<usize>)> = None;
    for (word, style) in [
        (&b"disc"[..], 1u8),
        (&b"disk"[..], 1u8),
        (&b"part"[..], 2u8),
        (&b"cd"[..], 0u8),
        (&b"pt"[..], 2u8),
    ] {
        let mut from = 0usize;
        while let Some(rel) = find_subslice(&lower[from..], word) {
            let i = from + rel;
            let before_ok = i == 0 || !is_token_byte(lower[i - 1]);
            let mut p = i + word.len();
            while p < lower.len() && matches!(lower[p], b' ' | b'-' | b'_' | b'.') {
                p += 1;
            }
            if before_ok {
                if let Some((num, end)) = parse_digits(&lower, p, 2) {
                    if end >= lower.len() || !is_token_byte(lower[end]) {
                        let label = match style {
                            0 => format!("CD{num}"),
                            1 => format!("Disc {num}"),
                            _ => format!("Part {num}"),
                        };
                        let replace = match &best {
                            None => true,
                            Some((bstart, _, _)) => i < *bstart,
                        };
                        if replace {
                            best = Some((i, label, i..end));
                        }
                        break;
                    }
                }
            }
            from = i + 1;
        }
    }
    best.map(|(_, label, range)| (label, range))
}

// ---------------------------------------------------------------------------
// extra / side-file markers (sample, trailer, 预告)

fn find_extra_marker(input: &str) -> Option<Range<usize>> {
    let lower = ascii_lower_bytes(input);
    let mut best: Option<Range<usize>> = None;
    for word in [&b"sample"[..], &b"trailer"[..]] {
        if let Some(pos) = find_subslice(&lower, word) {
            let end = pos + word.len();
            let before_ok = pos == 0 || !is_token_byte(lower[pos - 1]);
            let after_ok = end >= lower.len() || !is_token_byte(lower[end]);
            if before_ok && after_ok {
                let replace = match &best {
                    None => true,
                    Some(r) => pos < r.start,
                };
                if replace {
                    best = Some(pos..end);
                }
            }
        }
    }
    if let Some(pos) = input.find("预告") {
        let replace = match &best {
            None => true,
            Some(r) => pos < r.start,
        };
        if replace {
            best = Some(pos..pos + "预告".len());
        }
    }
    best
}

// ---------------------------------------------------------------------------
// technical tags

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TechCategory {
    Resolution,
    Source,
    VideoCodec,
    Dynamic,
    AudioCodec,
    Channel,
}

#[derive(Debug, Clone)]
struct TechCandidate {
    range: Range<usize>,
    label: String,
}

fn tech_candidates(input: &str) -> Vec<TechCandidate> {
    use TechCategory::*;
    let specs: &[(&[&str], &str, TechCategory)] = &[
        // resolution
        (&["4320p"], "4320p", Resolution),
        (&["2160p"], "2160p", Resolution),
        (&["1080p"], "1080p", Resolution),
        (&["1080i"], "1080i", Resolution),
        (&["720p"], "720p", Resolution),
        (&["576p"], "576p", Resolution),
        (&["480p"], "480p", Resolution),
        (&["4k"], "4K", Resolution),
        (&["8k"], "8K", Resolution),
        (&["uhd"], "UHD", Resolution),
        // source
        (&["bdremux", "bd-remux"], "BDRemux", Source),
        (&["bluray", "blu-ray", "blu ray"], "BluRay", Source),
        (&["bdrip"], "BDRip", Source),
        (&["remux"], "REMUX", Source),
        (&["web-dl", "webdl", "web dl"], "WEB-DL", Source),
        (&["webrip", "web-rip"], "WEBRip", Source),
        (&["hdtv"], "HDTV", Source),
        (&["dvdrip"], "DVDRip", Source),
        (&["hdrip"], "HDRip", Source),
        // video codec (original spelling kept, per spec default)
        (&["h.264"], "H.264", VideoCodec),
        (&["h.265"], "H.265", VideoCodec),
        (&["x264"], "x264", VideoCodec),
        (&["x265"], "x265", VideoCodec),
        (&["avc"], "AVC", VideoCodec),
        (&["hevc"], "HEVC", VideoCodec),
        (&["av1"], "AV1", VideoCodec),
        (&["vp9"], "VP9", VideoCodec),
        (&["mpeg2", "mpeg-2"], "MPEG2", VideoCodec),
        // bit depth & dynamic range
        (&["8bit", "8-bit"], "8bit", Dynamic),
        (&["10bit", "10-bit"], "10bit", Dynamic),
        (&["12bit", "12-bit"], "12bit", Dynamic),
        (&["hdr10+"], "HDR10+", Dynamic),
        (&["hdr10"], "HDR10", Dynamic),
        (&["hdr"], "HDR", Dynamic),
        (&["dolby vision"], "DV", Dynamic),
        (&["dovi"], "DoVi", Dynamic),
        (&["dv"], "DV", Dynamic),
        (&["sdr"], "SDR", Dynamic),
        // audio codec
        (&["dts-hd"], "DTS-HD", AudioCodec),
        (&["dts"], "DTS", AudioCodec),
        (&["eac3", "e-ac-3"], "EAC3", AudioCodec),
        (&["dd+"], "DD+", AudioCodec),
        (&["ddp"], "DD+", AudioCodec),
        (&["dd"], "DD", AudioCodec),
        (&["truehd"], "TrueHD", AudioCodec),
        (&["atmos"], "Atmos", AudioCodec),
        (&["aac"], "AAC", AudioCodec),
        (&["ac3"], "AC3", AudioCodec),
        (&["flac"], "FLAC", AudioCodec),
        (&["mp3"], "MP3", AudioCodec),
        (&["opus"], "Opus", AudioCodec),
        (&["lpcm"], "LPCM", AudioCodec),
        // channels (kept exactly as written in the source name:
        // `2ch` stays `2ch`, `2.0` stays `2.0`, and when a name
        // carries both forms both are kept, in order)
        (&["2.0"], "2.0", Channel),
        (&["2ch"], "2ch", Channel),
        (&["5.1"], "5.1", Channel),
        (&["6ch"], "6ch", Channel),
        (&["7.1"], "7.1", Channel),
        (&["8ch"], "8ch", Channel),
    ];

    let lower = ascii_lower_bytes(input);
    let mut found: Vec<TechCandidate> = Vec::new();
    for (needles, label, _category) in specs {
        for needle in *needles {
            let nb = needle.as_bytes();
            let mut from = 0usize;
            while let Some(rel) = find_subslice(&lower[from..], nb) {
                let start = from + rel;
                let end = start + nb.len();
                let first = nb[0];
                let last = nb[nb.len() - 1];
                let before_ok = start == 0
                    || !first.is_ascii_alphanumeric()
                    || !is_token_byte(lower[start - 1]);
                let after_ok = end >= lower.len()
                    || !last.is_ascii_alphanumeric()
                    || !is_token_byte(lower[end]);
                if before_ok && after_ok {
                    found.push(TechCandidate {
                        range: start..end,
                        label: label.to_string(),
                    });
                }
                from = start + 1;
            }
        }
    }

    // Earliest first; at the same start the longest match wins (DTS-HD > DTS,
    // HDR10+ > HDR10 > HDR, BDRemux > REMUX,...). Overlaps are dropped.
    found.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| (b.range.end - b.range.start).cmp(&(a.range.end - a.range.start)))
    });
    let mut selected: Vec<TechCandidate> = Vec::new();
    for cand in found {
        if selected.iter().any(|c: &TechCandidate| {
            cand.range.start < c.range.end && c.range.start < cand.range.end
        }) {
            continue;
        }
        if selected.iter().any(|c| c.label == cand.label) {
            continue;
        }
        selected.push(cand);
    }
    selected.sort_by_key(|c| c.range.start);
    selected
}

/// Technical tags found in a name, in order of appearance. Channel
/// tags are kept exactly as written (`2ch` does not become `2.0`),
/// and coexisting channel forms (`AAC 2ch 2.0`) all survive.
pub fn extract_tech_tags(input: &str) -> Vec<String> {
    let stem = strip_media_extension(last_component(input));
    tech_candidates(stem).into_iter().map(|c| c.label).collect()
}

// ---------------------------------------------------------------------------
// title cleaning

fn remove_bracketed_sources(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut square = 0usize;
    let mut cjk = 0usize;
    for ch in s.chars() {
        match ch {
            '[' => square += 1,
            ']' => square = square.saturating_sub(1),
            '\u{3010}' => cjk += 1,
            '\u{3011}' => cjk = cjk.saturating_sub(1),
            _ => {
                if square == 0 && cjk == 0 {
                    out.push(ch);
                }
            }
        }
    }
    out
}

/// A search-ready title: technical tags, TMDB id, year, season/episode and
/// part markers removed, release-group brackets dropped, separators and
/// colon normalized.
pub fn clean_search_title(input: &str) -> String {
    let component = last_component(input);
    let stem = strip_media_extension(component);
    let no_brackets = remove_bracketed_sources(stem);

    let mut ranges: Vec<Range<usize>> = Vec::new();
    for cand in tech_candidates(&no_brackets) {
        ranges.push(cand.range);
    }
    if let Some((_, r)) = find_tmdb_id(&no_brackets) {
        ranges.push(r);
    }
    if let Some((_, r)) = find_tvdb_id(&no_brackets) {
        ranges.push(r);
    }
    if let Some((_, r)) = find_bgm_id(&no_brackets) {
        ranges.push(r);
    }
    if let Some((_, r)) = find_year(&no_brackets) {
        ranges.push(r);
    }
    if let Some(info) = parse_episode_info(&no_brackets) {
        ranges.push(info.start..info.end);
    }
    if let Some((_, r)) = find_part(&no_brackets) {
        ranges.push(r);
    }
    if let Some(r) = find_extra_marker(&no_brackets) {
        ranges.push(r);
    }
    let stripped = remove_ranges(&no_brackets, &ranges);

    // Normalize punctuation: keep letters/digits (any script), apostrophe,
    // ampersand and colon; a hyphen survives only between two alphanumerics
    // (Spider-Man), everything else becomes a space.
    let chars: Vec<char> = stripped.chars().collect();
    let mut normalized = String::with_capacity(stripped.len());
    for (idx, ch) in chars.iter().enumerate() {
        match *ch {
            '：' | ':' => normalized.push(':'),
            '-' => {
                let prev = (0..idx).rev().map(|i| chars[i]).find(|c| *c != ' ');
                let next = chars[idx + 1..].iter().find(|c| **c != ' ');
                let keep = matches!(prev, Some(c) if c.is_alphanumeric())
                    && matches!(next, Some(c) if c.is_alphanumeric());
                normalized.push(if keep { '-' } else { ' ' });
            }
            c if c.is_alphanumeric() || c == '\'' || c == '&' => normalized.push(c),
            _ => normalized.push(' '),
        }
    }

    let mut tokens: Vec<&str> = Vec::new();
    for token in normalized.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "sample" | "trailer" | "preview" | "featurette" | "season"
        ) {
            continue;
        }
        if token.contains("预告") {
            continue;
        }
        tokens.push(token);
    }
    let joined = tokens.join(" ");

    // Tidy colon spacing: "Title: Sub" / "Title:Sub" -> "Title: Sub".
    let mut out = String::with_capacity(joined.len());
    let mut iter = joined.chars().peekable();
    while let Some(ch) = iter.next() {
        if ch == ':' {
            while out.ends_with(' ') {
                out.pop();
            }
            if out.is_empty() {
                continue;
            }
            out.push(':');
            if let Some(next) = iter.peek() {
                if *next != ' ' {
                    out.push(' ');
                }
            }
        } else {
            out.push(ch);
        }
    }
    out.trim().trim_end_matches(':').trim().to_string()
}

// ---------------------------------------------------------------------------
// directory-name parsing

fn is_short_ascii_prefix(segment: &str) -> bool {
    let t = segment.trim();
    !t.is_empty()
        && t.len() <= 20
        && t.is_ascii()
        && t.chars().any(|c| c.is_ascii_alphabetic())
        && t.split_whitespace().count() <= 3
}

fn extract_directory_title(name: &str) -> String {
    // NOTE: the recognition priority for *searching* is
    // 1) platform id, 2) the full directory name, 3) the 《》
    // name, 4) the dash-segment name. This function extracts the
    // single display/search title in the older order (《》 > dash
    // > cleaned full name); when matching grows multiple search
    // terms, try them in the priority order above rather than
    // baking this order in.
    // 《...》 wins wherever it appears.
    if let Some(start) = name.find('《') {
        let inner_start = start + '《'.len_utf8();
        if let Some(rel) = name[inner_start..].find('》') {
            let inner = &name[inner_start..inner_start + rel];
            let cleaned = clean_search_title(inner);
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }
    // — / － separated segments.
    for delim in ['—', '－'] {
        if name.contains(delim) {
            let segments: Vec<&str> = name.split(delim).collect();
            let candidate = pick_dash_segment(&segments);
            let cleaned = clean_search_title(candidate);
            if !cleaned.is_empty() {
                return cleaned;
            }
        }
    }
    if name.contains(" - ") {
        let segments: Vec<&str> = name.split(" - ").collect();
        let candidate = pick_dash_segment(&segments);
        let cleaned = clean_search_title(candidate);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    clean_search_title(name)
}

fn pick_dash_segment<'a>(segments: &[&'a str]) -> &'a str {
    match segments.len() {
        0 => "",
        1 => segments[0],
        2 => {
            if is_short_ascii_prefix(segments[0]) {
                segments[1]
            } else {
                segments[0]
            }
        }
        _ => segments[1],
    }
}

/// Parse a work directory name: TMDB id, title (《》 > — segments > cleaned
/// fallback) and year, plus any season marker / tech tags present.
pub fn parse_directory_name(input: &str) -> ParsedName {
    let component = last_component(input);
    let mut parsed = ParsedName {
        tmdb_id: find_tmdb_id(component).map(|(v, _)| v),
        tvdb_id: find_tvdb_id(component).map(|(v, _)| v),
        bgm_id: find_bgm_id(component).map(|(v, _)| v),
        year: find_year(component).map(|(v, _)| v),
        season: parse_season_directory_name(component),
        tech_tags: extract_tech_tags(component),
        ..ParsedName::default()
    };
    parsed.title = extract_directory_title(component);
    if parsed.title.eq_ignore_ascii_case("season") {
        parsed.title.clear();
    }
    parsed
}

// ---------------------------------------------------------------------------
// file-name parsing

/// Parse one file name: season/episode (incl. multi-episode and specials),
/// part markers, sample/trailer flags, tech tags, year, TMDB id and a
/// cleaned title (the part before the first structural marker).
pub fn parse_file_name(input: &str) -> ParsedName {
    let component = last_component(input);
    let stem = strip_media_extension(component);

    let info = parse_episode_info(stem);
    let part = find_part(stem);
    let extra = find_extra_marker(stem);

    let mut cut: Option<usize> = None;
    let mut note_cut = |pos: usize| {
        if pos > 0 {
            cut = Some(cut.map_or(pos, |c: usize| c.min(pos)));
        }
    };
    if let Some(info) = &info {
        note_cut(info.start);
    }
    if let Some((_, r)) = find_year(stem) {
        note_cut(r.start);
    }
    if let Some(first) = tech_candidates(stem).first() {
        note_cut(first.range.start);
    }
    if let Some((_, r)) = &part {
        note_cut(r.start);
    }
    if let Some(r) = &extra {
        note_cut(r.start);
    }

    let title = match cut {
        Some(pos) => clean_search_title(&stem[..pos]),
        None => clean_search_title(stem),
    };

    ParsedName {
        title,
        year: find_year(stem).map(|(v, _)| v),
        tmdb_id: find_tmdb_id(stem).map(|(v, _)| v),
        tvdb_id: find_tvdb_id(stem).map(|(v, _)| v),
        bgm_id: find_bgm_id(stem).map(|(v, _)| v),
        season: info.as_ref().and_then(|i| i.season),
        episode: info.as_ref().and_then(|i| i.episode),
        episode_end: info.as_ref().and_then(|i| i.episode_end),
        part: part.map(|(label, _)| label),
        is_extra: extra.is_some(),
        tech_tags: extract_tech_tags(stem),
    }
}

/// Parse a full path string: the file name provides the primary data, a
/// season directory fills in the season, and the work directory provides
/// title / year / TMDB id when the file name lacks them — or when the
/// file name's own title is meaningless (empty, purely numeric, one
/// character), in which case a purely numeric stem also becomes the
/// work's episode number.
pub fn parse_file_path(input: &str) -> ParsedName {
    let parts: Vec<&str> = input
        .split(|c| c == '/' || c == '\\')
        .filter(|p| !p.trim().is_empty())
        .collect();
    let Some((file_name, ancestors)) = parts.split_last() else {
        return ParsedName::default();
    };
    let mut parsed = parse_file_name(file_name);
    if let Some(parent) = ancestors.last() {
        let parent_season = parse_season_directory_name(parent);
        if parsed.season.is_none() {
            parsed.season = parent_season;
        }
        let work_dir = if parent_season.is_some() && ancestors.len() >= 2 {
            ancestors[ancestors.len() - 2]
        } else {
            parent
        };
        let dir_parsed = parse_directory_name(work_dir);
        if parsed.tmdb_id.is_none() {
            parsed.tmdb_id = dir_parsed.tmdb_id;
        }
        if parsed.tvdb_id.is_none() {
            parsed.tvdb_id = dir_parsed.tvdb_id;
        }
        if parsed.bgm_id.is_none() {
            parsed.bgm_id = dir_parsed.bgm_id;
        }
        if parsed.year.is_none() {
            parsed.year = dir_parsed.year;
        }
        // Title priority: a meaningless file-name title (empty,
        // purely numeric, one character) loses to the work
        // directory's real title — "01.mkv" inside
        // "101次求婚-TMDB48455" belongs to that show, it is not a
        // work called "01".
        let file_title_meaningless = is_unsearchable_title(&parsed.title);
        let dir_title_wins =
            file_title_meaningless && !is_unsearchable_title(&dir_parsed.title);
        if dir_title_wins {
            parsed.title = dir_parsed.title.clone();
        }
        // A purely numeric stem in a folder that names a real work
        // (by title or by a platform id) is that work's episode number.
        if parsed.episode.is_none()
            && file_title_meaningless
            && (dir_title_wins || dir_parsed.platform_id().is_some())
        {
            if let Some(ep) = numeric_episode(file_name) {
                parsed.episode = Some(ep);
            }
        }
    }
    parsed
}

// ---------------------------------------------------------------------------
// numbered episode sets & search-title guard

/// Episode number of a purely numeric file name: the stem (extension
/// stripped) is 1–3 ASCII digits — `01.mkv` -> 1, `007.mkv` -> 7.
/// Four-digit stems stay out on purpose (`2012.mkv` is a movie year,
/// not an episode number), and so does `00`.
pub fn numeric_episode(input: &str) -> Option<u32> {
    let stem = strip_media_extension(last_component(input)).trim();
    if stem.is_empty() || stem.len() > 3 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let value: u32 = stem.parse().ok()?;
    if value == 0 {
        None
    } else {
        Some(value)
    }
}

/// Detect a numbered episode set among the file names of ONE
/// directory. When two or more names are purely numeric
/// ([`numeric_episode`]), they are consecutive episodes of a single
/// show: the returned vector is parallel to `names`, holding the
/// episode number for each numeric file and `None` for the rest.
/// With fewer than two numeric files there is no set (a lone
/// `01.mkv` proves nothing) and every slot is `None`.
pub fn detect_numbered_episodes(names: &[&str]) -> Vec<Option<u32>> {
    let episodes: Vec<Option<u32>> = names.iter().map(|n| numeric_episode(n)).collect();
    if episodes.iter().filter(|e| e.is_some()).count() >= 2 {
        episodes
    } else {
        vec![None; names.len()]
    }
}

/// True when a parsed title is useless as a TMDB search term: empty,
/// purely numeric, or a single character. Matching such a title
/// anyway returns confident garbage (searching "01" finds some
/// unrelated movie), so callers must skip the work and report it as
/// unrecognized instead of matched or failed.
pub fn is_unsearchable_title(title: &str) -> bool {
    let title = title.trim();
    title.chars().count() <= 1 || title.bytes().all(|b| b.is_ascii_digit())
}

// ---------------------------------------------------------------------------
// name building

/// Make one path component safe: half-width colon becomes full-width ：,
/// other forbidden characters become their full-width forms, control
/// characters are dropped, edge spaces/dots are trimmed.
pub fn sanitize_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            ':' => out.push('：'),
            '/' => out.push('／'),
            '\\' => out.push('＼'),
            '*' => out.push('＊'),
            '?' => out.push('？'),
            '"' => out.push('＂'),
            '<' => out.push('＜'),
            '>' => out.push('＞'),
            '|' => out.push('｜'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c| c == ' ' || c == '.').to_string();
    if trimmed.is_empty() {
        "Untitled".to_string()
    } else {
        trimmed
    }
}

fn title_with_year(title: &str, year: Option<u16>) -> String {
    let clean = sanitize_component(title);
    match year {
        Some(y) => format!("{clean} ({y})"),
        None => clean,
    }
}

fn extension_suffix(extension: &str) -> String {
    let ext: String = extension
        .trim()
        .trim_start_matches('.')
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect();
    if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    }
}

/// Whether a tag label is an audio codec, mirroring the
/// AudioCodec entries of the extraction vocabulary.
fn is_audio_codec_label(label: &str) -> bool {
    matches!(
        label,
        "AAC"
            | "AC3"
            | "EAC3"
            | "DD"
            | "DD+"
            | "DTS"
            | "DTS-HD"
            | "TrueHD"
            | "Atmos"
            | "FLAC"
            | "MP3"
            | "Opus"
            | "LPCM"
    )
}

/// The tag segment of a library file name, glued to whatever
/// precedes it: the tags before the audio codec are hyphen-joined,
/// while the audio codec and everything after it (channels, ...)
/// form an "audio group" joined with spaces —
/// `-720p-AVC-8bit-AAC 2ch 2.0`. With no audio codec the whole
/// segment is hyphen-joined; with nothing before the codec the
/// segment is just `-` plus the audio group.
pub fn file_tag_suffix(tech_tags: &[String]) -> String {
    let tags: Vec<String> = tech_tags
        .iter()
        .map(|t| sanitize_component(t))
        .filter(|t| t != "Untitled")
        .collect();
    if tags.is_empty() {
        return String::new();
    }
    match tags.iter().position(|t| is_audio_codec_label(t)) {
        Some(idx) => {
            let mut out = String::new();
            if idx > 0 {
                out.push('-');
                out.push_str(&tags[..idx].join("-"));
            }
            out.push('-');
            out.push_str(&tags[idx..].join(" "));
            out
        }
        None => format!("-{}", tags.join("-")),
    }
}

// ---------------------------------------------------------------------------
// Media info (stream facts probed from a file, parsed out of
// `ffmpeg -i` output)
// ---------------------------------------------------------------------------

/// Stream facts probed from one media file — the values the
/// library file-name tag segment is built from when the original
/// file name carries no tags of its own (a file just called
/// `01.mkv` still gets a full tail from its streams).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MediaInfo {
    /// Resolution label by width tier: `2160p` / `1080p` /
    /// `720p` / `576p` / `480p`.
    pub resolution: Option<String>,
    /// Mapped video codec label (`AVC`, `HEVC`, ...).
    pub video_codec: Option<String>,
    /// `8bit` / `10bit` / `12bit`, from the pixel format.
    pub bit_depth: Option<String>,
    /// Mapped audio codec label (`AAC`, `DTS`, ...) of the
    /// default audio stream (the first one when none is marked
    /// default).
    pub audio_codec: Option<String>,
    /// Channel tags in the dual written form, e.g. `["6ch",
    /// "5.1"]` for a 5.1 layout.
    pub channel_tags: Vec<String>,
}

impl MediaInfo {
    /// The tag values in their fixed order — resolution, video
    /// codec, bit depth, audio codec, channels — ready to feed
    /// [`file_tag_suffix`].
    pub fn tags(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(v) = &self.resolution {
            out.push(v.clone());
        }
        if let Some(v) = &self.video_codec {
            out.push(v.clone());
        }
        if let Some(v) = &self.bit_depth {
            out.push(v.clone());
        }
        if let Some(v) = &self.audio_codec {
            out.push(v.clone());
        }
        out.extend(self.channel_tags.iter().cloned());
        out
    }

    /// Nothing usable was found (no video or audio stream).
    pub fn is_empty(&self) -> bool {
        self.resolution.is_none()
            && self.video_codec.is_none()
            && self.audio_codec.is_none()
    }
}

fn media_video_codec_label(codec: &str) -> String {
    match codec {
        "h264" => "AVC".to_string(),
        "hevc" => "HEVC".to_string(),
        "av1" => "AV1".to_string(),
        "vp9" => "VP9".to_string(),
        "mpeg2video" => "MPEG2".to_string(),
        "mpeg4" => "MPEG4".to_string(),
        other => other.to_uppercase(),
    }
}

fn media_audio_codec_label(codec: &str) -> String {
    match codec {
        "aac" => "AAC".to_string(),
        "ac3" => "AC3".to_string(),
        "eac3" => "EAC3".to_string(),
        "dts" => "DTS".to_string(),
        "truehd" => "TrueHD".to_string(),
        "flac" => "FLAC".to_string(),
        "mp3" => "MP3".to_string(),
        "opus" => "Opus".to_string(),
        other => other.to_uppercase(),
    }
}

/// The leading run of codec-name characters at the start of an
/// ffmpeg stream description (`h264 (High), ...` → `h264`).
fn leading_token(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    &s[..end]
}

/// The first `WxH` dimension pair in a video stream description.
/// The digit-run shapes are constrained (3–5 digits wide, 2–5
/// high) so hex codec tags like `0x31637661` are not mistaken
/// for dimensions.
fn stream_dimensions(rest: &str) -> Option<(u32, u32)> {
    let b = rest.as_bytes();
    for i in 0..b.len() {
        if b[i] != b'x' {
            continue;
        }
        let mut l = i;
        while l > 0 && b[l - 1].is_ascii_digit() {
            l -= 1;
        }
        if !(3..=5).contains(&(i - l)) {
            continue;
        }
        let mut r = i + 1;
        while r < b.len() && b[r].is_ascii_digit() {
            r += 1;
        }
        if !(2..=5).contains(&(r - (i + 1))) {
            continue;
        }
        let w: u32 = rest[l..i].parse().ok()?;
        let h: u32 = rest[i + 1..r].parse().ok()?;
        return Some((w, h));
    }
    None
}

/// The pixel-format token of a video stream description: the
/// comma-separated field right after the codec (and its optional
/// parenthesized profile), up to its own detail parenthesis.
fn stream_pix_fmt(rest: &str) -> Option<String> {
    let after = rest.split_once(',')?.1.trim_start();
    let end = after
        .find(|c: char| c == '(' || c == ',' || c.is_whitespace())
        .unwrap_or(after.len());
    let token = &after[..end];
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

fn bit_depth_label(pix_fmt: &str) -> String {
    let p = pix_fmt.to_ascii_lowercase();
    // `nv12` is an 8-bit format whose name happens to contain
    // "12"; it must be checked before the digit rules.
    if p.starts_with("nv12") {
        "8bit"
    } else if p.contains("12") {
        "12bit"
    } else if p.contains("10") {
        "10bit"
    } else {
        "8bit"
    }
    .to_string()
}

fn resolution_label(width: u32, height: u32) -> String {
    if width >= 3800 {
        "2160p"
    } else if width >= 1900 {
        "1080p"
    } else if width >= 1270 {
        "720p"
    } else if height >= 570 {
        "576p"
    } else {
        "480p"
    }
    .to_string()
}

fn is_xy_layout(s: &str) -> bool {
    let Some((a, b)) = s.split_once('.') else {
        return false;
    };
    !a.is_empty()
        && a.bytes().all(|c| c.is_ascii_digit())
        && !b.is_empty()
        && b.bytes().all(|c| c.is_ascii_digit())
}

/// The channel-layout name in an audio stream description:
/// the comma-separated field that is a known layout word or an
/// `X.Y` form, with any parenthesized qualifier (`(side)`,
/// `(wide)`) stripped.
fn channel_layout(rest: &str) -> Option<String> {
    for seg in rest.split(',').map(str::trim) {
        let base = seg.split('(').next().unwrap_or(seg).trim();
        if matches!(base, "mono" | "stereo" | "quad") || is_xy_layout(base) {
            return Some(base.to_string());
        }
    }
    None
}

/// Channel tags in the dual written form: the channel count and
/// the `X.Y` layout together (`5.1` → `["6ch", "5.1"]`).
fn channel_tags(layout: &str) -> Vec<String> {
    match layout {
        "mono" => vec!["1ch".to_string(), "1.0".to_string()],
        "stereo" => vec!["2ch".to_string(), "2.0".to_string()],
        "quad" => vec!["4ch".to_string(), "4.0".to_string()],
        xy => {
            if let Some((a, b)) = xy.split_once('.') {
                if let (Ok(x), Ok(y)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    return vec![format!("{}ch", x + y), xy.to_string()];
                }
            }
            Vec::new()
        }
    }
}

/// Parse the stream facts out of `ffmpeg -i <file>` output (the
/// text ffmpeg prints to stderr). The first video stream that is
/// not an attached picture provides the video facts; the audio
/// facts come from the stream marked `(default)`, else the first
/// audio stream.
pub fn parse_ffmpeg_info(text: &str) -> MediaInfo {
    let mut video: Option<&str> = None;
    let mut audios: Vec<(&str, bool)> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        if !line.starts_with("Stream ") {
            continue;
        }
        if let Some(pos) = line.find(": Video: ") {
            if !line.contains("(attached pic)") && video.is_none() {
                video = Some(&line[pos + ": Video: ".len()..]);
            }
        } else if let Some(pos) = line.find(": Audio: ") {
            let rest = &line[pos + ": Audio: ".len()..];
            audios.push((rest, line.contains("(default)")));
        }
    }
    let mut info = MediaInfo::default();
    if let Some(rest) = video {
        let codec = leading_token(rest);
        if !codec.is_empty() {
            info.video_codec = Some(media_video_codec_label(codec));
        }
        if let Some(pix) = stream_pix_fmt(rest) {
            info.bit_depth = Some(bit_depth_label(&pix));
        }
        if let Some((w, h)) = stream_dimensions(rest) {
            info.resolution = Some(resolution_label(w, h));
        }
    }
    let audio = audios
        .iter()
        .find(|(_, is_default)| *is_default)
        .or_else(|| audios.first());
    if let Some((rest, _)) = audio {
        let codec = leading_token(rest);
        if !codec.is_empty() {
            info.audio_codec = Some(media_audio_codec_label(codec));
        }
        if let Some(layout) = channel_layout(rest) {
            info.channel_tags = channel_tags(&layout);
        }
    }
    info
}

fn tag_suffix(tech_tags: &[String]) -> String {
    let tags: Vec<String> = tech_tags
        .iter()
        .map(|t| sanitize_component(t))
        .filter(|t| t != "Untitled")
        .collect();
    if tags.is_empty() {
        String::new()
    } else {
        format!(" - {}", tags.join(" "))
    }
}

/// Movie layout: `Title (Year)/Title (Year)[ - Part][ - tags].ext`
/// (the year segment is omitted when `year` is `None`).
pub fn build_movie_relative_path(
    title: &str,
    year: Option<u16>,
    part: Option<&str>,
    tech_tags: &[String],
    extension: &str,
) -> String {
    let dir_name = title_with_year(title, year);
    let mut file_name = dir_name.clone();
    if let Some(part) = part {
        let part = part.trim();
        if !part.is_empty() {
            file_name.push_str(" - ");
            file_name.push_str(&sanitize_component(part));
        }
    }
    file_name.push_str(&tag_suffix(tech_tags));
    format!("{dir_name}/{file_name}{}", extension_suffix(extension))
}

/// Series layout: `Show/Season 01/Show - S01E01[ - Episode Title][ - tags].ext`
/// Multi-episode files use the range form `S01E01-E02`.
pub fn build_tv_relative_path(
    title: &str,
    season: u32,
    episode: u32,
    episode_end: Option<u32>,
    episode_title: Option<&str>,
    tech_tags: &[String],
    extension: &str,
) -> String {
    let show = sanitize_component(title);
    let mut code = format!("S{season:02}E{episode:02}");
    if let Some(end) = episode_end {
        code.push_str(&format!("-E{end:02}"));
    }
    let mut file_name = format!("{show} - {code}");
    if let Some(ep_title) = episode_title {
        let ep_title = ep_title.trim();
        if !ep_title.is_empty() {
            file_name.push_str(" - ");
            file_name.push_str(&sanitize_component(ep_title));
        }
    }
    file_name.push_str(&tag_suffix(tech_tags));
    format!(
        "{show}/Season {season:02}/{file_name}{}",
        extension_suffix(extension)
    )
}

/// Build the target relative path for a parsed file, using `title` as the
/// canonical (e.g. TMDB) title. Returns `None` for episode files whose
/// season is unknown.
pub fn build_relative_path(
    parsed: &ParsedName,
    title: &str,
    episode_title: Option<&str>,
    extension: &str,
) -> Option<String> {
    let title = if title.trim().is_empty() {
        parsed.title.as_str()
    } else {
        title
    };
    match (parsed.season, parsed.episode) {
        (Some(season), Some(episode)) => Some(build_tv_relative_path(
            title,
            season,
            episode,
            parsed.episode_end,
            episode_title,
            &parsed.tech_tags,
            extension,
        )),
        (Some(_), None) => None,
        _ => Some(build_movie_relative_path(
            title,
            parsed.year,
            parsed.part.as_deref(),
            &parsed.tech_tags,
            extension,
        )),
    }
}

// ---------------------------------------------------------------------------
// library layout (the target media-library format)
//
// Work folder: `[6.1] [2013] [TMDB-00115782]《乔布斯》(Jobs)`
//   - the score segment is always present; unrated works (no votes)
//     get the `[0.0]` placeholder;
//   - the year segment is omitted when the year is unknown;
//   - the TMDB segment is omitted when there is no id;
//   - the `(English)` segment is omitted when the en-US title is
//     empty or identical to the localized title.
// Movie file: `《乔布斯》(Jobs)[ - Part]-2160p-AVC-8bit-AAC 2.0.mkv`
// TV file:    `乡村爱情 - S01E01 - 第 1 集 (2006-09-30)-720p-AVC-8bit-AAC 2ch 2.0.mkv`
//             inside `<work folder>/Season 01/` — the show title
//             in file names carries no 《》 brackets, the episode
//             title segment is always present (TMDB name or the
//             per-language placeholder, air date in parentheses
//             when known), and the tech tags follow the audio-
//             group layout of `file_tag_suffix`.

/// Everything about a matched work that the library layout needs.
/// Mirrors the fields TMDB provides; kept as a plain local struct so
/// this crate stays dependency-free.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkTitle {
    /// Localized (e.g. zh-CN) title.
    pub title: String,
    /// en-US title; empty when unknown.
    pub english_title: String,
    /// Release / first-air year.
    pub year: Option<u16>,
    /// The matched work's id on `id_platform`'s catalogue (the
    /// field keeps its original name; with a TVDB or Bangumi
    /// match it holds that platform's id and the folder segment
    /// is tagged accordingly).
    pub tmdb_id: Option<u64>,
    /// Which catalogue `tmdb_id` belongs to — decides the folder
    /// tag: `[TMDB-…]`, `[TVDB-…]` or `[BGM-…]`.
    pub id_platform: IdPlatform,
    /// The matching platform's average vote (one decimal is printed).
    pub vote_average: f64,
    /// Vote count; 0 means "not rated yet" (TVDB matches never
    /// carry votes, so they always print the `[0.0]` placeholder).
    pub vote_count: u64,
}

/// The `《中文名》` segment, plus `(English)` when it adds information.
fn bracket_title_segment(work: &WorkTitle) -> String {
    let title = sanitize_component(&work.title);
    let mut out = format!("\u{300a}{title}\u{300b}");
    let english = work.english_title.trim();
    if !english.is_empty() && english != work.title.trim() {
        out.push_str(&format!("({})", sanitize_component(english)));
    }
    out
}

/// Work folder name: `[score] [year] [TMDB-id]《Title》(English)`,
/// segments in that order; see the layout notes above for which
/// segments are omitted (the score never is).
pub fn build_work_folder_name(work: &WorkTitle) -> String {
    let score = if work.vote_count == 0 {
        0.0
    } else {
        work.vote_average
    };
    let mut brackets = vec![format!("[{score:.1}]")];
    if let Some(year) = work.year {
        brackets.push(format!("[{year}]"));
    }
    if let Some(id) = work.tmdb_id {
        // Fixed 8-digit field, zero-padded on the left; longer
        // ids simply outgrow it (never truncated). The tag names
        // the catalogue the id belongs to.
        brackets.push(format!("[{}-{id:08}]", work.id_platform.tag()));
    }
    format!("{}{}", brackets.join(" "), bracket_title_segment(work))
}

/// Movie file name: `《Title》(English)[ - Part][tag segment].ext`
/// (the tag segment is `file_tag_suffix`'s audio-group layout,
/// glued on with no space).
pub fn build_movie_file_name(
    work: &WorkTitle,
    part: Option<&str>,
    tech_tags: &[String],
    extension: &str,
) -> String {
    let mut name = bracket_title_segment(work);
    if let Some(part) = part {
        let part = part.trim();
        if !part.is_empty() {
            name.push_str(" - ");
            name.push_str(&sanitize_component(part));
        }
    }
    name.push_str(&file_tag_suffix(tech_tags));
    format!("{name}{}", extension_suffix(extension))
}

/// Full relative path of a movie file: `<work folder>/<movie file>`.
pub fn build_movie_library_path(
    work: &WorkTitle,
    part: Option<&str>,
    tech_tags: &[String],
    extension: &str,
) -> String {
    format!(
        "{}/{}",
        build_work_folder_name(work),
        build_movie_file_name(work, part, tech_tags, extension)
    )
}

/// Language of the naming layer (folder/file names), which the
/// app ties to the system language — the same decision that picks
/// the TMDB payload language and the UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameLang {
    Zh,
    En,
}

/// The placeholder episode-title segment used when TMDB has no
/// name for an episode: `第 {n} 集` in Chinese naming,
/// `Episode {n}` in English naming.
fn episode_placeholder(episode: u32, lang: NameLang) -> String {
    match lang {
        NameLang::Zh => format!("第 {episode} 集"),
        NameLang::En => format!("Episode {episode}"),
    }
}

/// TV episode file name:
/// `Title - S01E01 - Episode Title (YYYY-MM-DD)-720p-AVC-....ext`
/// (multi-episode files use the range form `S01E01-E02`). The show
/// title carries no 《》 brackets here.
///
/// The episode-title segment is a fixed part of the format: the
/// TMDB episode name when one is known, otherwise the placeholder
/// for `lang` (see `episode_placeholder`). The air date follows in
/// parentheses when known and is omitted otherwise. The tech tag
/// segment follows `file_tag_suffix`'s audio-group layout.
pub fn build_tv_file_name(
    work: &WorkTitle,
    season: u32,
    episode: u32,
    episode_end: Option<u32>,
    episode_title: Option<&str>,
    air_date: Option<&str>,
    tech_tags: &[String],
    extension: &str,
    lang: NameLang,
) -> String {
    let show = sanitize_component(&work.title);
    let mut code = format!("S{season:02}E{episode:02}");
    if let Some(end) = episode_end {
        code.push_str(&format!("-E{end:02}"));
    }
    let mut name = format!("{show} - {code}");
    let title = episode_title.map(str::trim).filter(|s| !s.is_empty());
    let mut segment = match title {
        Some(t) => sanitize_component(t),
        None => episode_placeholder(episode, lang),
    };
    if let Some(date) = air_date.map(str::trim).filter(|s| !s.is_empty()) {
        segment.push_str(&format!(" ({})", sanitize_component(date)));
    }
    name.push_str(" - ");
    name.push_str(&segment);
    name.push_str(&file_tag_suffix(tech_tags));
    format!("{name}{}", extension_suffix(extension))
}

/// Full relative path of an episode file:
/// `<work folder>/Season 01/<episode file>`.
pub fn build_tv_library_path(
    work: &WorkTitle,
    season: u32,
    episode: u32,
    episode_end: Option<u32>,
    episode_title: Option<&str>,
    air_date: Option<&str>,
    tech_tags: &[String],
    extension: &str,
    lang: NameLang,
) -> String {
    format!(
        "{}/Season {season:02}/{}",
        build_work_folder_name(work),
        build_tv_file_name(
            work,
            season,
            episode,
            episode_end,
            episode_title,
            air_date,
            tech_tags,
            extension,
            lang
        )
    )
}

// ---------------------------------------------------------------------------
// tests

#[cfg(test)]
mod tests {
    use super::*;

    fn jobs() -> WorkTitle {
        WorkTitle {
            title: "乔布斯".to_string(),
            english_title: "Jobs".to_string(),
            year: Some(2013),
            tmdb_id: Some(115782),
            id_platform: IdPlatform::Tmdb,
            vote_average: 6.14,
            vote_count: 1950,
        }
    }

    #[test]
    fn work_folder_name_full() {
        assert_eq!(
            build_work_folder_name(&jobs()),
            "[6.1] [2013] [TMDB-00115782]\u{300a}乔布斯\u{300b}(Jobs)"
        );
    }

    #[test]
    fn work_folder_name_pads_short_tmdb_id() {
        let mut work = jobs();
        work.tmdb_id = Some(48455);
        assert_eq!(
            build_work_folder_name(&work),
            "[6.1] [2013] [TMDB-00048455]\u{300a}乔布斯\u{300b}(Jobs)"
        );
        // Longer ids outgrow the field; they are never truncated.
        work.tmdb_id = Some(123_456_789);
        assert_eq!(
            build_work_folder_name(&work),
            "[6.1] [2013] [TMDB-123456789]\u{300a}乔布斯\u{300b}(Jobs)"
        );
    }

    #[test]
    fn work_folder_name_unrated_keeps_score_placeholder() {
        let mut work = jobs();
        work.vote_average = 0.0;
        work.vote_count = 0;
        assert_eq!(
            build_work_folder_name(&work),
            "[0.0] [2013] [TMDB-00115782]\u{300a}乔布斯\u{300b}(Jobs)"
        );
    }

    #[test]
    fn work_folder_name_omits_year_and_english() {
        let mut work = jobs();
        work.year = None;
        work.english_title = String::new();
        assert_eq!(
            build_work_folder_name(&work),
            "[6.1] [TMDB-00115782]\u{300a}乔布斯\u{300b}"
        );
        // An en-US title identical to the localized one adds nothing.
        work.english_title = "乔布斯".to_string();
        assert_eq!(
            build_work_folder_name(&work),
            "[6.1] [TMDB-00115782]\u{300a}乔布斯\u{300b}"
        );
    }

    #[test]
    fn movie_library_path_uses_folder_and_bracket_stem() {
        let tags = vec![
            "2160p".to_string(),
            "AVC".to_string(),
            "8bit".to_string(),
            "AAC".to_string(),
            "2.0".to_string(),
        ];
        assert_eq!(
            build_movie_library_path(&jobs(), None, &tags, "mkv"),
            "[6.1] [2013] [TMDB-00115782]\u{300a}乔布斯\u{300b}(Jobs)/\u{300a}乔布斯\u{300b}(Jobs)-2160p-AVC-8bit-AAC 2.0.mkv"
        );
        // Multi-part files keep their segment label.
        assert_eq!(
            build_movie_file_name(&jobs(), Some("CD2"), &[], "mp4"),
            "\u{300a}乔布斯\u{300b}(Jobs) - CD2.mp4"
        );
    }

    #[test]
    fn tv_library_path_uses_plain_title_in_file_name() {
        let work = WorkTitle {
            title: "红楼梦".to_string(),
            english_title: String::new(),
            year: Some(2010),
            tmdb_id: Some(8374756),
            id_platform: IdPlatform::Tmdb,
            vote_average: 6.4,
            vote_count: 18,
        };
        let tags = vec!["1080p".to_string()];
        // No TMDB episode title yet: the placeholder segment is
        // always present, and the tag segment is hyphen-joined and
        // glued to it.
        assert_eq!(
            build_tv_library_path(&work, 1, 1, None, None, None, &tags, "mkv", NameLang::Zh),
            "[6.4] [2010] [TMDB-08374756]\u{300a}红楼梦\u{300b}/Season 01/红楼梦 - S01E01 - 第 1 集-1080p.mkv"
        );
        assert_eq!(
            build_tv_file_name(&work, 1, 1, Some(2), Some("太虚幻境"), None, &[], "mkv", NameLang::Zh),
            "红楼梦 - S01E01-E02 - 太虚幻境.mkv"
        );
    }

    #[test]
    fn tv_file_name_user_library_format() {
        // The target library layout, from a real example:
        // a placeholder episode title (TMDB has no name for it),
        // its air date, and hyphen-joined tech tags.
        let work = WorkTitle {
            title: "乡村爱情".to_string(),
            english_title: String::new(),
            year: Some(2006),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        let tags: Vec<String> = ["720p", "AVC", "8bit", "AAC", "2.0"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            build_tv_file_name(&work, 1, 1, None, None, Some("2006-09-30"), &tags, "mkv", NameLang::Zh),
            "乡村爱情 - S01E01 - 第 1 集 (2006-09-30)-720p-AVC-8bit-AAC 2.0.mkv"
        );
        assert_eq!(
            build_tv_library_path(&work, 1, 1, None, None, Some("2006-09-30"), &tags, "mkv", NameLang::Zh),
            "[0.0] [2006]\u{300a}乡村爱情\u{300b}/Season 01/乡村爱情 - S01E01 - 第 1 集 (2006-09-30)-720p-AVC-8bit-AAC 2.0.mkv"
        );
    }

    #[test]
    fn tv_file_name_title_and_date_variants() {
        let work = WorkTitle {
            title: "红楼梦".to_string(),
            english_title: String::new(),
            year: Some(2010),
            tmdb_id: Some(8374756),
            id_platform: IdPlatform::Tmdb,
            vote_average: 6.4,
            vote_count: 18,
        };
        // TMDB title and air date both present.
        assert_eq!(
            build_tv_file_name(&work, 1, 2, None, Some("太虚幻境"), Some("2010-04-29"), &[], "mkv", NameLang::Zh),
            "红楼梦 - S01E02 - 太虚幻境 (2010-04-29).mkv"
        );
        // Title without a date: no parenthesis segment.
        assert_eq!(
            build_tv_file_name(&work, 1, 2, None, Some("太虚幻境"), None, &[], "mkv", NameLang::Zh),
            "红楼梦 - S01E02 - 太虚幻境.mkv"
        );
        // An empty TMDB title falls back to the placeholder, and a
        // blank date is dropped.
        assert_eq!(
            build_tv_file_name(&work, 2, 3, None, Some("  "), Some(""), &[], "mkv", NameLang::Zh),
            "红楼梦 - S02E03 - 第 3 集.mkv"
        );
        // Multi-episode files keep the range code and take the
        // first episode's title segment.
        assert_eq!(
            build_tv_file_name(&work, 1, 1, Some(2), None, Some("2010-04-29"), &[], "mkv", NameLang::Zh),
            "红楼梦 - S01E01-E02 - 第 1 集 (2010-04-29).mkv"
        );
    }

    #[test]
    fn tv_file_name_user_example_from_extracted_tags() {
        // The reference example, end to end: tags extracted
        // from a download-style name (channels kept as written —
        // `2ch` and `2.0` coexist) must rebuild his exact string.
        let work = WorkTitle {
            title: "乡村爱情".to_string(),
            english_title: String::new(),
            year: Some(2006),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        let tags = extract_tech_tags("乡村爱情.S01E01.720p.AVC.8bit.AAC 2ch 2.0.mkv");
        assert_eq!(tags, vec!["720p", "AVC", "8bit", "AAC", "2ch", "2.0"]);
        assert_eq!(
            build_tv_file_name(&work, 1, 1, None, None, Some("2006-09-30"), &tags, "mkv", NameLang::Zh),
            "乡村爱情 - S01E01 - 第 1 集 (2006-09-30)-720p-AVC-8bit-AAC 2ch 2.0.mkv"
        );
    }

    #[test]
    fn tv_file_name_drops_non_vocabulary_residue() {
        // `x264()` loses its parentheses and `core` is not in the
        // vocabulary at all: extraction only keeps known tags, so
        // neither residue reaches the new name.
        let work = WorkTitle {
            title: "新还珠格格".to_string(),
            english_title: String::new(),
            year: Some(2011),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        let tags = extract_tech_tags(
            "新还珠格格 - S01E02 - 小燕子紫薇义结金兰 (2011-07-16)-1080p-x264()-core-8bit-AAC 2ch 2.0.mkv",
        );
        assert_eq!(tags, vec!["1080p", "x264", "8bit", "AAC", "2ch", "2.0"]);
        assert_eq!(
            build_tv_file_name(
                &work,
                1,
                2,
                None,
                Some("小燕子紫薇义结金兰"),
                Some("2011-07-16"),
                &tags,
                "mkv",
                NameLang::Zh
            ),
            "新还珠格格 - S01E02 - 小燕子紫薇义结金兰 (2011-07-16)-1080p-x264-8bit-AAC 2ch 2.0.mkv"
        );
    }

    #[test]
    fn tv_file_name_breaking_bad_channels_and_audio_group() {
        // Both channel spellings survive (`6ch` and `5.1`), and
        // the audio group — codec plus everything after it — is
        // space-joined while the head stays hyphen-joined.
        let work = WorkTitle {
            title: "绝命毒师".to_string(),
            english_title: "Breaking Bad".to_string(),
            year: Some(2008),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        let tags = extract_tech_tags(
            "Breaking Bad - S01E01 - 向导 (2008-01-20)-1080p-HEVC-8bit-DTS 6ch 5.1.mkv",
        );
        assert_eq!(tags, vec!["1080p", "HEVC", "8bit", "DTS", "6ch", "5.1"]);
        assert_eq!(
            build_tv_file_name(
                &work,
                1,
                1,
                None,
                Some("向导"),
                Some("2008-01-20"),
                &tags,
                "mkv",
                NameLang::Zh
            ),
            "绝命毒师 - S01E01 - 向导 (2008-01-20)-1080p-HEVC-8bit-DTS 6ch 5.1.mkv"
        );
    }

    #[test]
    fn movie_file_name_audio_group_spacing() {
        let work = WorkTitle {
            title: "片名".to_string(),
            english_title: "Title".to_string(),
            year: Some(2024),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        let tags = extract_tech_tags("Some Movie-2160p-HEVC-10bit-TrueHD 7.1.mkv");
        assert_eq!(tags, vec!["2160p", "HEVC", "10bit", "TrueHD", "7.1"]);
        assert_eq!(
            build_movie_file_name(&work, None, &tags, "mkv"),
            "\u{300a}片名\u{300b}(Title)-2160p-HEVC-10bit-TrueHD 7.1.mkv"
        );
    }

    #[test]
    fn tv_file_name_english_placeholder() {
        // English naming: the placeholder is `Episode {n}` and a
        // known air date still follows it in parentheses.
        let work = WorkTitle {
            title: "House".to_string(),
            english_title: String::new(),
            year: Some(2004),
            tmdb_id: None,
            id_platform: IdPlatform::Tmdb,
            vote_average: 0.0,
            vote_count: 0,
        };
        assert_eq!(
            build_tv_file_name(&work, 1, 1, None, None, Some("2004-11-16"), &[], "mkv", NameLang::En),
            "House - S01E01 - Episode 1 (2004-11-16).mkv"
        );
        assert_eq!(
            build_tv_file_name(&work, 1, 2, None, None, None, &[], "mkv", NameLang::En),
            "House - S01E02 - Episode 2.mkv"
        );
    }

    #[test]
    fn directory_tmdb_id_and_book_title() {
        let p = parse_directory_name("《红楼梦》TMDB0008374756");
        assert_eq!(p.tmdb_id, Some(8374756));
        assert_eq!(p.title, "红楼梦");
    }

    #[test]
    fn directory_tmdb_id_variants() {
        assert_eq!(
            parse_directory_name("红楼梦 tmdb-8374756").tmdb_id,
            Some(8374756)
        );
        assert_eq!(
            parse_directory_name("红楼梦 {tmdb-8374756}").tmdb_id,
            Some(8374756)
        );
        assert_eq!(
            parse_directory_name("红楼梦 {tmdb=8374756}").tmdb_id,
            Some(8374756)
        );
    }

    #[test]
    fn directory_tvdb_and_bgm_ids() {
        // The folder names the app itself writes for TVDB /
        // Bangumi matches parse back to the right platform.
        let p = parse_directory_name("[7.9] [1991] [TVDB-00001234]《101次求婚》");
        assert_eq!(p.tvdb_id, Some(1234));
        assert_eq!(p.tmdb_id, None);
        assert_eq!(p.platform_id(), Some((IdPlatform::Tvdb, 1234)));
        assert_eq!(p.title, "101次求婚");

        let p = parse_directory_name("[8.7] [2013] [BGM-00123456]《进击的巨人》");
        assert_eq!(p.bgm_id, Some(123456));
        assert_eq!(p.platform_id(), Some((IdPlatform::Bangumi, 123456)));

        assert_eq!(
            parse_directory_name("Some Show {tvdb=777}").tvdb_id,
            Some(777)
        );
        assert_eq!(parse_directory_name("Some Show bgm-42").bgm_id, Some(42));
        // A bare "BGM" mention with no digits right after it is
        // not an id (release names talk about background music).
        let p = parse_directory_name("Some Show BGM Collection");
        assert_eq!(p.bgm_id, None);
        // TMDB keeps priority when a name somehow carries both.
        let p = parse_directory_name("Show [TVDB-00000001] [TMDB-00000002]");
        assert_eq!(p.platform_id(), Some((IdPlatform::Tmdb, 2)));
    }

    #[test]
    fn file_path_inherits_tvdb_and_bgm_ids() {
        let p = parse_file_path("[TVDB-00001234]《101次求婚》/01.mkv");
        assert_eq!(p.platform_id(), Some((IdPlatform::Tvdb, 1234)));
        assert_eq!(p.episode, Some(1));
        let p = parse_file_path("进击的巨人 BGM-123456/05.mkv");
        assert_eq!(p.platform_id(), Some((IdPlatform::Bangumi, 123456)));
        assert_eq!(p.episode, Some(5));
    }

    #[test]
    fn work_folder_name_platform_tags() {
        let mut work = jobs();
        work.id_platform = IdPlatform::Tvdb;
        work.tmdb_id = Some(1234);
        assert_eq!(
            build_work_folder_name(&work),
            "[6.1] [2013] [TVDB-00001234]\u{300a}乔布斯\u{300b}(Jobs)"
        );
        // TVDB matches carry no votes: the score segment falls
        // back to the placeholder.
        work.vote_count = 0;
        work.id_platform = IdPlatform::Bangumi;
        work.tmdb_id = Some(123456);
        assert_eq!(
            build_work_folder_name(&work),
            "[0.0] [2013] [BGM-00123456]\u{300a}乔布斯\u{300b}(Jobs)"
        );
    }

    #[test]
    fn captain_america_file() {
        let p = parse_file_name(
            "Captain America： Brave New World-2025-2160p-AVC-8bit-AAC 2ch 2.0.mp4",
        );
        assert_eq!(p.title, "Captain America: Brave New World");
        assert_eq!(p.year, Some(2025));
        assert_eq!(
            p.tech_tags,
            vec!["2160p", "AVC", "8bit", "AAC", "2ch", "2.0"]
        );
        assert_eq!(p.season, None);
        assert_eq!(p.episode, None);
        let path = build_relative_path(&p, &p.title.clone(), None, "mp4").unwrap();
        assert_eq!(
path,
"Captain America： Brave New World (2025)/Captain America： Brave New World (2025) - 2160p AVC 8bit AAC 2ch 2.0.mp4"
);
    }

    #[test]
    fn directory_dash_segments() {
        // Three segments: the middle one is the title.
        let p = parse_directory_name("CMCT — 红楼梦 — 2023");
        assert_eq!(p.title, "红楼梦");
        assert_eq!(p.year, Some(2023));
        // Two segments with a short ASCII release-group prefix.
        let p2 = parse_directory_name("CMCT — 红楼梦");
        assert_eq!(p2.title, "红楼梦");
    }

    #[test]
    fn directory_fallback_cleanup() {
        let p = parse_directory_name("[字幕组] 某剧 (2020) [1080p]");
        assert_eq!(p.title, "某剧");
        assert_eq!(p.year, Some(2020));
    }

    #[test]
    fn season_directory_names() {
        assert_eq!(parse_season_directory_name("S01"), Some(1));
        assert_eq!(parse_season_directory_name("S1"), Some(1));
        assert_eq!(parse_season_directory_name("Season 1"), Some(1));
        assert_eq!(parse_season_directory_name("Season 01"), Some(1));
        assert_eq!(parse_season_directory_name("第1季"), Some(1));
        assert_eq!(parse_season_directory_name("第一季"), Some(1));
        assert_eq!(parse_season_directory_name("第十二季"), Some(12));
        assert_eq!(parse_season_directory_name("Specials"), Some(0));
        assert_eq!(parse_season_directory_name("第0季"), Some(0));
        assert_eq!(parse_season_directory_name("S00"), Some(0));
        assert_eq!(parse_season_directory_name("红楼梦"), None);
    }

    #[test]
    fn episode_patterns() {
        let p = parse_file_name("Show.S01E01.mkv");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("Show S1E1.mkv");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("Show E01.mkv");
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("Show EP01.mkv");
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("某剧 第12集.mkv");
        assert_eq!(p.episode, Some(12));

        let p = parse_file_name("某剧 第一集.mkv");
        assert_eq!(p.episode, Some(1));
    }

    #[test]
    fn multi_episode_patterns() {
        let p = parse_file_name("Show.S01E01E02.mkv");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, Some(2));

        let p = parse_file_name("Show.S01E01-E02.mkv");
        assert_eq!(p.episode, Some(1));
        assert_eq!(p.episode_end, Some(2));

        let path = build_tv_relative_path("Show", 1, 1, Some(2), None, &[], "mkv");
        assert_eq!(path, "Show/Season 01/Show - S01E01-E02.mkv");
    }

    #[test]
    fn specials_go_to_season_zero() {
        let p = parse_file_name("Show.SP01.mkv");
        assert_eq!(p.season, Some(0));
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("某剧 番外01.mkv");
        assert_eq!(p.season, Some(0));
        assert_eq!(p.episode, Some(1));

        let p = parse_file_name("Show OVA2.mkv");
        assert_eq!(p.season, Some(0));
        assert_eq!(p.episode, Some(2));
    }

    #[test]
    fn part_markers() {
        let p = parse_file_name("Movie (2019) CD2.mkv");
        assert_eq!(p.part.as_deref(), Some("CD2"));
        let path = build_relative_path(&p, "Movie", None, "mkv").unwrap();
        assert_eq!(path, "Movie (2019)/Movie (2019) - CD2.mkv");

        let p = parse_file_name("Movie Disc 1.mkv");
        assert_eq!(p.part.as_deref(), Some("Disc 1"));
        let p = parse_file_name("Movie part1.mkv");
        assert_eq!(p.part.as_deref(), Some("Part 1"));
    }

    #[test]
    fn extra_markers() {
        assert!(parse_file_name("Movie.sample.mkv").is_extra);
        assert!(parse_file_name("Movie trailer.mkv").is_extra);
        assert!(parse_file_name("某电影 预告.mkv").is_extra);
        assert!(!parse_file_name("Movie.mkv").is_extra);
    }

    #[test]
    fn tech_tags_order_and_channels_kept_as_written() {
        let tags = extract_tech_tags("Movie.2024.WEB-DL.1080p.HEVC.10bit.DTS-HD.6ch.mkv");
        assert_eq!(
            tags,
            vec!["WEB-DL", "1080p", "HEVC", "10bit", "DTS-HD", "6ch"]
        );
        // Channel forms are not normalized and do not collapse
        // into each other: `8ch` and `7.1` both survive.
        let tags = extract_tech_tags("Movie 8ch 7.1.mkv");
        assert_eq!(tags, vec!["8ch", "7.1"]);
        // Clean title drops the tags.
        assert_eq!(
            clean_search_title("Movie.2024.WEB-DL.1080p.HEVC.mkv"),
            "Movie"
        );
    }

    #[test]
    fn tv_path_with_episode_title_and_tags() {
        let path = build_tv_relative_path(
            "红楼梦",
            1,
            1,
            None,
            Some("第一集标题"),
            &["2160p".to_string(), "HEVC".to_string(), "10bit".to_string()],
            "mkv",
        );
        assert_eq!(
            path,
            "红楼梦/Season 01/红楼梦 - S01E01 - 第一集标题 - 2160p HEVC 10bit.mkv"
        );
    }

    #[test]
    fn sanitize_components() {
        assert_eq!(sanitize_component("A:B"), "A：B");
        assert_eq!(sanitize_component("a/b*c?d"), "a／b＊c？d");
        assert_eq!(sanitize_component(" name. "), "name");
        assert_eq!(sanitize_component(""), "Untitled");
    }

    #[test]
    fn file_path_merges_season_and_work_dirs() {
        let p = parse_file_path("《红楼梦》TMDB0008374756/Season 01/S01E01.mkv");
        assert_eq!(p.tmdb_id, Some(8374756));
        assert_eq!(p.title, "红楼梦");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(1));
    }

    #[test]
    fn numeric_episode_values() {
        assert_eq!(numeric_episode("01.mkv"), Some(1));
        assert_eq!(numeric_episode("1.mkv"), Some(1));
        assert_eq!(numeric_episode("007.mp4"), Some(7));
        assert_eq!(numeric_episode("999.mkv"), Some(999));
        // A four-digit stem is a year, not an episode.
        assert_eq!(numeric_episode("2012.mkv"), None);
        assert_eq!(numeric_episode("00.mkv"), None);
        assert_eq!(numeric_episode("01a.mkv"), None);
        assert_eq!(numeric_episode("E01.mkv"), None);
        assert_eq!(numeric_episode("花絮.mkv"), None);
        assert_eq!(numeric_episode("Some Movie (2020).mkv"), None);
    }

    #[test]
    fn numbered_group_maps_a_full_season() {
        // Layout: a show folder full of 01.mkv .. 12.mkv.
        let names: Vec<String> = (1..=12).map(|ep| format!("{ep:02}.mkv")).collect();
        let refs: Vec<&str> = names.iter().map(|n| n.as_str()).collect();
        let found = detect_numbered_episodes(&refs);
        let expected: Vec<Option<u32>> = (1..=12).map(Some).collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn numbered_group_needs_two_numeric_files() {
        // A lone numeric file proves nothing: no group.
        assert_eq!(
            detect_numbered_episodes(&["01.mkv"]),
            vec![None]
        );
        assert_eq!(
            detect_numbered_episodes(&["2012.mkv"]),
            vec![None]
        );
        assert_eq!(
            detect_numbered_episodes(&["01.mkv", "Making Of.mkv"]),
            vec![None, None]
        );
        // Two numeric files are enough.
        assert_eq!(
            detect_numbered_episodes(&["01.mkv", "02.mkv"]),
            vec![Some(1), Some(2)]
        );
    }

    #[test]
    fn numbered_group_leaves_other_files_out() {
        // Non-numeric neighbours in the same directory keep their
        // own handling; only the numeric files join the group.
        let found = detect_numbered_episodes(&[
            "01.mkv",
            "影片花絮.mkv",
            "02.mkv",
            "Some Movie (2020).mkv",
            "10.mp4",
        ]);
        assert_eq!(found, vec![Some(1), None, Some(2), None, Some(10)]);
    }

    #[test]
    fn unsearchable_titles() {
        assert!(is_unsearchable_title(""));
        assert!(is_unsearchable_title("  "));
        assert!(is_unsearchable_title("01"));
        assert!(is_unsearchable_title("2012"));
        assert!(is_unsearchable_title("7"));
        assert!(is_unsearchable_title("红"));
        assert!(!is_unsearchable_title("红楼梦"));
        assert!(!is_unsearchable_title("01 号公路"));
        assert!(!is_unsearchable_title("Jobs"));
    }

    #[test]
    fn file_path_numeric_file_takes_work_dir_identity() {
        // A show folder added as a root: the file is episode 1
        // of the show the directory names, not a work called "01".
        let p = parse_file_path("101次求婚-TMDB48455/01.mkv");
        assert_eq!(p.title, "101次求婚");
        assert_eq!(p.tmdb_id, Some(48455));
        assert_eq!(p.episode, Some(1));

        let p = parse_file_path("红楼梦/05.mkv");
        assert_eq!(p.title, "红楼梦");
        assert_eq!(p.episode, Some(5));
    }

    #[test]
    fn file_path_meaningless_file_title_loses_to_dir() {
        // "2012" as a file title is meaningless; the folder's title
        // wins. Four digits are a year, though — no episode.
        let p = parse_file_path("Movies/2012.mkv");
        assert_eq!(p.title, "Movies");
        assert_eq!(p.year, Some(2012));
        assert_eq!(p.episode, None);
    }

    #[test]
    fn file_path_keeps_real_file_title() {
        let p = parse_file_path("Some Show/Show.S01E02.mkv");
        assert_eq!(p.title, "Show");
        assert_eq!(p.season, Some(1));
        assert_eq!(p.episode, Some(2));
    }

    #[test]
    fn chinese_number_parsing() {
        assert_eq!(parse_chinese_or_arabic_number("一"), Some(1));
        assert_eq!(parse_chinese_or_arabic_number("十"), Some(10));
        assert_eq!(parse_chinese_or_arabic_number("十二"), Some(12));
        assert_eq!(parse_chinese_or_arabic_number("二十"), Some(20));
        assert_eq!(parse_chinese_or_arabic_number("二十一"), Some(21));
        assert_eq!(parse_chinese_or_arabic_number("九十九"), Some(99));
        assert_eq!(parse_chinese_or_arabic_number("42"), Some(42));
    }

    #[test]
    fn media_info_h264_aac() {
        let text = "\
Input #0, matroska,webm, from 'show.mkv':
  Metadata:
    encoder         : libebml v1.4.2 + libmatroska v1.6.4
  Duration: 00:24:32.12, start: 0.000000, bitrate: 4500 kb/s
  Stream #0:0: Video: h264 (High), yuv420p(progressive), 1920x1080 [SAR 1:1 DAR 16:9], 23.98 fps, 23.98 tbr, 1k tbn (default)
  Stream #0:1(jpn): Audio: aac (LC), 48000 Hz, stereo, fltp, 160 kb/s (default)
  Stream #0:2: Subtitle: subrip (default)
";
        let info = parse_ffmpeg_info(text);
        assert_eq!(info.resolution.as_deref(), Some("1080p"));
        assert_eq!(info.video_codec.as_deref(), Some("AVC"));
        assert_eq!(info.bit_depth.as_deref(), Some("8bit"));
        assert_eq!(info.audio_codec.as_deref(), Some("AAC"));
        assert_eq!(info.channel_tags, vec!["2ch", "2.0"]);
        assert_eq!(info.tags(), vec!["1080p", "AVC", "8bit", "AAC", "2ch", "2.0"]);
    }

    #[test]
    fn media_info_hevc_10bit_dts() {
        // Cropped 4K (3840x1600): the tier comes from the width.
        let text = "\
Input #0, matroska,webm, from 'movie.mkv':
  Duration: 02:15:10.55, start: 0.000000, bitrate: 18500 kb/s
  Stream #0:0: Video: hevc (Main 10), yuv420p10le(tv, bt2020nc/bt2020/smpte2084), 3840x1600 [SAR 1:1 DAR 12:5], 23.98 fps, 23.98 tbr, 1k tbn (default)
  Stream #0:1(eng): Audio: dts (DTS-HD MA), 48000 Hz, 5.1(side), s32 (24 bit), 3800 kb/s (default)
";
        let info = parse_ffmpeg_info(text);
        assert_eq!(info.tags(), vec!["2160p", "HEVC", "10bit", "DTS", "6ch", "5.1"]);
    }

    #[test]
    fn media_info_prefers_default_audio_track() {
        let text = "\
Input #0, matroska,webm, from 'ep.mkv':
  Duration: 00:43:00.00, start: 0.000000, bitrate: 3000 kb/s
  Stream #0:0: Video: h264 (High), yuvj420p(pc, progressive), 1280x720 [SAR 1:1 DAR 16:9], 25 fps, 25 tbr, 1k tbn (default)
  Stream #0:1(jpn): Audio: aac (LC), 48000 Hz, stereo, fltp, 128 kb/s
  Stream #0:2(eng): Audio: ac3, 48000 Hz, 5.1(side), fltp, 640 kb/s (default)
";
        let info = parse_ffmpeg_info(text);
        // Not the first track — the one marked (default).
        assert_eq!(info.tags(), vec!["720p", "AVC", "8bit", "AC3", "6ch", "5.1"]);
    }

    #[test]
    fn media_info_video_only_and_attached_pic() {
        let text = "\
Input #0, matroska,webm, from 'clip.mkv':
  Duration: 00:01:00.00, start: 0.000000, bitrate: 800 kb/s
  Stream #0:0: Video: vp9, yuv420p(tv, bt709, progressive), 854x480 [SAR 1:1 DAR 427:240], 30 fps, 30 tbr, 1k tbn (default)
  Stream #0:1: Video: png, rgba, 500x500 [SAR 1:1 DAR 1:1], 90k tbr, 90k tbn (attached pic)
";
        let info = parse_ffmpeg_info(text);
        assert_eq!(info.tags(), vec!["480p", "VP9", "8bit"]);
        assert_eq!(info.audio_codec, None);
        assert!(info.channel_tags.is_empty());
    }

    #[test]
    fn media_info_channel_layouts() {
        let one = |layout: &str| {
            let text = format!(
                "  Stream #0:0(eng): Audio: flac, 44100 Hz, {layout}, s16, 700 kb/s (default)\n"
            );
            parse_ffmpeg_info(&text).channel_tags
        };
        assert_eq!(one("mono"), vec!["1ch", "1.0"]);
        assert_eq!(one("stereo"), vec!["2ch", "2.0"]);
        assert_eq!(one("quad"), vec!["4ch", "4.0"]);
        assert_eq!(one("2.1"), vec!["3ch", "2.1"]);
        assert_eq!(one("5.1"), vec!["6ch", "5.1"]);
        assert_eq!(one("7.1(wide)"), vec!["8ch", "7.1"]);
    }

    #[test]
    fn media_info_no_streams() {
        let info = parse_ffmpeg_info("Input #0, mov,mp4, from 'x.mp4':\n  Duration: N/A\n");
        assert!(info.is_empty());
        assert!(info.tags().is_empty());
    }
}
