//! Read-only iTunes Search API song lookup for the CLI's catalog protocol.
use super::{client_builder, ITunes};
use crate::catalog_lookup::{
    identity, read_response, request, valid_text, write, Candidate, Request,
};
use anyhow::{anyhow, ensure, Result};
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Write};
use std::time::Duration;

const LIMIT: usize = 25;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page {
    result_count: usize,
    results: Vec<Value>,
}

struct Song<'a> {
    ids: Option<(u64, u64)>,
    artist: &'a str,
    title: &'a str,
    genre: Option<&'a str>,
}

fn field<'a>(row: &'a Value, name: &str) -> Option<&'a str> {
    let value = row.get(name)?.as_str()?;
    valid_text(value).then(|| value.trim())
}

impl<'a> Song<'a> {
    fn from_row(row: &'a Value) -> Option<Self> {
        if row.get("wrapperType")?.as_str()? != "track" || row.get("kind")?.as_str()? != "song" {
            return None;
        }
        let id = |key: &str| row.get(key)?.as_u64().filter(|n| *n > 0);
        Some(Self {
            ids: id("trackId").zip(id("collectionId")),
            artist: field(row, "artistName")?,
            title: field(row, "trackName")?,
            genre: field(row, "primaryGenreName"),
        })
    }

    fn matches(&self, artist: &str, title: &str) -> bool {
        identity(self.artist) == artist && identity(self.title) == title
    }
}

fn lookup(input: Request, output: impl Write, bytes: &[u8]) -> Result<()> {
    let page: Page =
        serde_json::from_slice(bytes).map_err(|_| anyhow!("Invalid iTunes search result"))?;
    // resultCount is the returned count, not a reliable total of all matching songs.
    // A full page offers no evidence that an exact edition is unique.
    if page.results.len() >= LIMIT || page.results.len() != page.result_count {
        return write(output, "iTunes", "ambiguous", vec![]);
    }

    let artist = identity(&input.artist);
    let title = identity(&input.title);
    let songs: Vec<_> = page.results.iter().filter_map(Song::from_row).collect();
    let Some(found) = songs.iter().find(|song| song.matches(&artist, &title)) else {
        return write(output, "iTunes", "no_match", vec![]);
    };
    // An exact song without both IDs can be another edition. It belongs in the
    // uniqueness check even though it cannot supply a usable result.
    let Some((track_id, release_id)) = found.ids else {
        return write(output, "iTunes", "ambiguous", vec![]);
    };
    let genre = found.genre.map(identity);
    for song in &songs {
        let exact = song.matches(&artist, &title);
        if (exact && song.ids != found.ids)
            || (song.ids.is_some_and(|(id, _)| id == track_id)
                && (!exact || song.ids != found.ids || song.genre.map(identity) != genre))
        {
            return write(output, "iTunes", "ambiguous", vec![]);
        }
    }

    let candidate = Candidate {
        track_id: track_id.to_string(),
        release_id: release_id.to_string(),
        artist: found.artist.to_owned(),
        title: found.title.to_owned(),
        genres: found.genre.into_iter().map(str::to_owned).collect(),
        genre_scope: "track",
        label: String::new(),
    };
    write(output, "iTunes", "matched", vec![candidate])
}

fn search_query<'a>(term: &'a str, limit: &'a str) -> [(&'static str, &'a str); 5] {
    [
        ("term", term),
        ("media", "music"),
        ("entity", "song"),
        ("country", "US"),
        ("limit", limit),
    ]
}

pub fn run(input: impl Read, output: impl Write) -> Result<()> {
    let input = request(input)?;
    let mut api = ITunes {
        client: client_builder()
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none())
            .build()?,
        slowdown: 1,
        rate_limit: 20,
    };
    let term = format!("{} {}", input.artist.trim(), input.title.trim());
    // A single CLI request starts a fresh process-local pacer. Space sequential
    // CLI invocations at the provider's 20 requests/minute budget as well.
    std::thread::sleep(Duration::from_secs(3));
    let response = api.get("/search", &search_query(&term, &LIMIT.to_string()))?;
    ensure!(
        response.status().is_success(),
        "iTunes search returned HTTP {}",
        response.status()
    );
    let bytes = read_response(response).map_err(|_| {
        anyhow!("iTunes search response exceeded the size limit or could not be read")
    })?;
    lookup(input, output, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_parameters_pin_us_songs_and_keep_the_term_literal() {
        let params = search_query("Artist & Song (Remix)", "25");
        let request = client_builder()
            .build()
            .unwrap()
            .get("https://itunes.apple.com/search")
            .query(&params)
            .build()
            .unwrap();
        assert_eq!(request.url().as_str(),
            "https://itunes.apple.com/search?term=Artist+%26+Song+%28Remix%29&media=music&entity=song&country=US&limit=25");
    }

    fn song() -> Value {
        json!({
            "wrapperType": "track", "kind": "song", "artistName": "AÉ  Artist",
            "trackName": "Song (Extended Mix)", "trackId": 31, "collectionId": 9,
            "primaryGenreName": "Dance", "collectionName": "Various Artists",
            "artworkUrl100": "https://invalid.test/artwork", "previewUrl": "https://invalid.test/audio"
        })
    }

    fn reply(rows: Vec<Value>, count: usize) -> Value {
        let input =
            request(br#"{"artist":"a\u00e9 artist","title":"song (extended mix)"}"#.as_slice())
                .unwrap();
        let page = serde_json::to_vec(&json!({"resultCount":count,"results":rows})).unwrap();
        let mut output = Vec::new();
        lookup(input, &mut output, &page).unwrap();
        serde_json::from_slice(&output).unwrap()
    }

    #[test]
    fn exact_song_carries_track_category_and_canonical_ids_only() {
        let s = song();
        let mut duplicated = s.clone();
        duplicated["primaryGenreName"] = json!("dance");
        let result = reply(vec![s, duplicated], 2);
        assert_eq!(result["status"], "matched");
        assert_eq!(result["provider"], "iTunes");
        let candidates = result["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c["track_id"], "31");
        assert_eq!(c["release_id"], "9");
        assert_eq!(c["genre_scope"], "track");
        assert_eq!(c["genres"], json!(["Dance"]));
        assert_eq!(c["label"], "");
        assert!(!result.to_string().contains("invalid.test"));
    }

    #[test]
    fn wrong_artist_edition_and_wrapper_are_ignored() {
        let mut rows = Vec::new();
        for (key, wrong) in [
            ("trackName", json!("Song")),
            ("artistName", json!("Another Artist")),
            ("wrapperType", json!("collection")),
            ("kind", json!("podcast")),
            ("artistName", json!("AÉ\nArtist")),
        ] {
            let mut s = song();
            s[key] = wrong;
            rows.push(s);
        }
        let count = rows.len();
        let misses = reply(rows, count);
        assert_eq!(misses["status"], "no_match");
        assert_eq!(misses["candidates"], json!([]));

        let mut song = song();
        song["primaryGenreName"] = json!("Hidden\ntext");
        assert_eq!(reply(vec![song], 1)["candidates"][0]["genres"], json!([]));
    }

    #[test]
    fn unidentifiable_exact_songs_prevent_enrichment_in_either_result_order() {
        for (key, invalid) in [
            ("trackId", None),
            ("trackId", Some(json!(0))),
            ("trackId", Some(json!(-1))),
            ("trackId", Some(json!("31"))),
            ("collectionId", None),
            ("collectionId", Some(Value::Null)),
            ("collectionId", Some(json!(9.5))),
        ] {
            let mut partial = song();
            if let Some(value) = invalid {
                partial[key] = value;
            } else {
                partial.as_object_mut().unwrap().remove(key);
            }
            for rows in [
                vec![partial.clone()],
                vec![partial.clone(), song()],
                vec![song(), partial.clone()],
            ] {
                let count = rows.len();
                let result = reply(rows, count);
                assert_eq!(result["status"], "ambiguous");
                assert_eq!(result["candidates"], json!([]));
            }
            partial["trackName"] = json!("Unrelated title");
            assert_eq!(reply(vec![partial, song()], 2)["status"], "matched");
        }
    }

    #[test]
    fn distinct_tracks_and_conflicts_on_the_same_track_id_hide_all_candidates() {
        for (key, conflict) in [
            ("trackId", json!(32)),
            ("trackName", json!("Song (Radio Edit)")),
            ("artistName", json!("Another Artist")),
            ("collectionId", json!(10)),
            ("primaryGenreName", json!("Rock")),
        ] {
            let mut other = song();
            other[key] = conflict;
            for rows in [vec![song(), other.clone()], vec![other, song()]] {
                let result = reply(rows, 2);
                assert_eq!(result["status"], "ambiguous", "{key}");
                assert_eq!(result["candidates"], json!([]));
            }
        }
    }

    #[test]
    fn saturated_or_inconsistent_search_pages_do_not_claim_uniqueness() {
        for result in [
            reply(vec![song(); LIMIT], LIMIT),
            reply(vec![song()], 2),
            reply(vec![song()], 0),
        ] {
            assert_eq!(result["status"], "ambiguous");
            assert_eq!(result["candidates"], json!([]));
        }
    }
}
