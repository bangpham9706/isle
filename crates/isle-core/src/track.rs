// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 zickrian

//! Cleaning up track metadata so lyric lookups find a match, e.g. YouTube's
//! "Artist - Song (Official Video)" by "ArtistVEVO" becomes "Song" by "Artist".

/// Words that mark a bracketed suffix as noise rather than part of the title.
const NOISE: &[&str] = &[
    "official",
    "video",
    "audio",
    "lyric",
    "lyrics",
    "visualizer",
    "mv",
    "m/v",
    "hd",
    "4k",
    "remaster",
    "remastered",
    "live",
    "explicit",
    "clip",
];

/// Returns `(title, artist)` cleaned for a lyrics search.
pub fn search_terms(title: &str, artist: &str) -> (String, String) {
    let mut title = full_title(title);
    let mut artist = clean_artist(artist);

    // "Artist - Song" in the title wins over a channel name in the artist.
    if let Some((left, right)) = title.split_once(" - ") {
        let (left, right) = (left.trim().to_owned(), right.trim().to_owned());
        if artist.is_empty() || (!right.is_empty() && looks_like_channel(&artist, &left)) {
            artist = left;
            title = right;
        }
    }

    // "Song ft. Someone": the featured artist is rarely part of the stored title.
    for marker in [" ft. ", " feat. ", " ft ", " feat "] {
        let index = title.to_lowercase().find(marker);
        // Lowercasing can change byte lengths outside ASCII; only cut on a boundary.
        if let Some(index) = index.filter(|&i| title.is_char_boundary(i)) {
            title.truncate(index);
        }
    }

    (title.trim().to_owned(), artist.trim().to_owned())
}

/// The title without noise but with any "Artist - " prefix still in it: a
/// free-text search for when the split from [`search_terms`] finds nothing.
pub fn full_title(title: &str) -> String {
    // "Song | Channel" or "Song (Lyrics) | Series": what follows the bar is
    // the uploader's branding, never part of the song's name.
    let title = title.split(" | ").next().unwrap_or(title);
    strip_noise(title)
}

/// Removes bracketed suffixes like "(Official Video)" or "\[4K\]".
fn strip_noise(title: &str) -> String {
    let mut result = String::with_capacity(title.len());
    let mut rest = title;
    while let Some(start) = rest.find(['(', '[']) {
        let close = if rest.as_bytes()[start] == b'(' {
            ')'
        } else {
            ']'
        };
        let Some(len) = rest[start..].find(close) else {
            break;
        };
        let inner = rest[start + 1..start + len].to_lowercase();
        result.push_str(&rest[..start]);
        if !NOISE
            .iter()
            .any(|word| inner.split_whitespace().any(|w| w == *word))
        {
            result.push_str(&rest[start..=start + len]);
        }
        rest = &rest[start + len + 1..];
    }
    result.push_str(rest);
    result.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Words a browser puts between the artists of a collaboration when its page is
/// in that language. YouTube sends "Ariana Grande và The Weeknd" from a
/// Vietnamese tab, and only LRCLIB's own spellings find a lyrics entry.
const ARTIST_JOINS: &[&str] = &["and", "và", "und", "et", "y", "e", "ja", "och", "и", "та"];

/// The artist names to look up, most likely first: the collaboration as LRCLIB
/// writes it, the name as the player sent it, and the first artist alone. A name
/// without a join word has the one form, so a lookup is never asked twice.
pub fn artist_forms(artist: &str) -> Vec<String> {
    let artist = artist.trim();
    let separators = || ARTIST_JOINS.iter().map(|word| format!(" {word} "));
    let parts: Vec<String> = match separators().find(|sep| artist.contains(sep.as_str())) {
        Some(sep) => artist
            .split(sep.as_str())
            .map(str::trim)
            .filter(|part| is_name(part))
            .map(str::to_owned)
            .collect(),
        None => Vec::new(),
    };
    if parts.len() < 2 {
        return vec![artist.to_owned()];
    }
    // Chasing every artist of a mash-up would ask too much of a busy LRCLIB.
    vec![parts.join(", "), artist.to_owned(), parts[0].clone()]
}

/// Whether a part of an artist name reads as an artist: it has a word of more
/// than one letter in it. Names spelled out letter by letter — the way the
/// slowed-and-reverb channels write "v e i l" — are no collaboration, and the
/// spaces in them are no join words.
fn is_name(part: &str) -> bool {
    part.split_whitespace()
        .any(|word| word.chars().count() >= 2)
}

/// Drops YouTube's " - Topic" and "VEVO" channel suffixes from an artist name.
fn clean_artist(artist: &str) -> String {
    let artist = artist.trim();
    let artist = artist.strip_suffix(" - Topic").unwrap_or(artist);
    let artist = artist.strip_suffix("VEVO").unwrap_or(artist);
    artist.trim().to_owned()
}

/// Whether `artist` is probably the uploader's channel rather than the
/// performer named on the left side of the title.
fn looks_like_channel(artist: &str, title_artist: &str) -> bool {
    let a = artist.to_lowercase().replace(' ', "");
    let b = title_artist.to_lowercase().replace(' ', "");
    a == b || a.contains(&b) || b.contains(&a) || a.ends_with("records") || a.ends_with("music")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_metadata_is_untouched() {
        assert_eq!(
            search_terms("Paper Sky", "Lumen Coast"),
            ("Paper Sky".into(), "Lumen Coast".into())
        );
    }

    #[test]
    fn youtube_style_titles_are_split() {
        assert_eq!(
            search_terms("Lumen Coast - Paper Sky (Official Video)", "LumenCoastVEVO"),
            ("Paper Sky".into(), "Lumen Coast".into())
        );
        assert_eq!(
            search_terms("Lumen Coast - Paper Sky [4K]", ""),
            ("Paper Sky".into(), "Lumen Coast".into())
        );
    }

    #[test]
    fn topic_channels_and_features_are_cleaned() {
        assert_eq!(
            search_terms("Paper Sky ft. Someone", "Lumen Coast - Topic"),
            ("Paper Sky".into(), "Lumen Coast".into())
        );
    }

    #[test]
    fn branding_after_a_bar_is_dropped() {
        assert_eq!(
            search_terms(
                "Lumen Coast - Paper Sky (Lyrics) | Night Drive",
                "Lyric Hub"
            ),
            ("Lumen Coast - Paper Sky".into(), "Lyric Hub".into())
        );
        assert_eq!(
            full_title("Lumen Coast - Paper Sky (Lyrics) | Night Drive"),
            "Lumen Coast - Paper Sky"
        );
    }

    #[test]
    fn meaningful_brackets_are_kept() {
        assert_eq!(search_terms("Song (Acoustic)", "A").0, "Song (Acoustic)");
    }

    #[test]
    fn dash_in_real_title_is_kept_when_artist_differs() {
        assert_eq!(search_terms("Part 1 - Intro", "Band").0, "Part 1 - Intro");
    }

    #[test]
    fn a_single_artist_is_asked_once() {
        assert_eq!(artist_forms("Lumen Coast"), ["Lumen Coast"]);
        assert_eq!(artist_forms(""), [""]);
        // A name spelled out letter by letter has no join word in it.
        assert_eq!(artist_forms("v e i l"), ["v e i l"]);
        assert_eq!(artist_forms("r a i n y d a y s"), ["r a i n y d a y s"]);
    }

    #[test]
    fn a_collaboration_in_another_language_is_rewritten() {
        assert_eq!(
            artist_forms("Ariana Grande và The Weeknd"),
            [
                "Ariana Grande, The Weeknd",
                "Ariana Grande và The Weeknd",
                "Ariana Grande"
            ]
        );
        assert_eq!(
            artist_forms("Justice y Simian"),
            ["Justice, Simian", "Justice y Simian", "Justice"]
        );
        // However the browser wrote it, the artists come out the same way.
        for join in [" and ", " und ", " et ", " и ", " och "] {
            let artist = format!("Queen{join}David Bowie");
            assert_eq!(artist_forms(&artist)[0], "Queen, David Bowie", "{artist}");
        }
    }

    #[test]
    fn a_mash_up_stops_at_the_first_artists() {
        assert_eq!(
            artist_forms("Usher and Beyoncé and Lil Wayne"),
            [
                "Usher, Beyoncé, Lil Wayne",
                "Usher and Beyoncé and Lil Wayne",
                "Usher"
            ]
        );
    }
}
