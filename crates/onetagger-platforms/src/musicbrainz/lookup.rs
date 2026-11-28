//! Read-only MusicBrainz recording lookup used by the CLI's JSON protocol.
use super::{client_builder, MusicBrainz};
use crate::catalog_lookup::{self, identity as normalize, valid_text as valid_field, Request};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::time::Duration;

const PAGE_SIZE: usize = 25;

fn literal(value: &str) -> String {
    // Each value is one Lucene phrase. Escape its delimiter and escape character.
    format!(
        "\"{}\"",
        value.trim().replace('\\', "\\\\").replace('"', "\\\"")
    )
}

#[derive(Deserialize)]
struct SearchPage {
    count: usize,
    offset: usize,
    recordings: Vec<Recording>,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Recording {
    id: String,
    title: String,
    artist_credit: Vec<Credit>,
    #[serde(default)]
    genres: Vec<Genre>,
}

#[derive(Deserialize)]
struct Credit {
    name: String,
    #[serde(default)]
    joinphrase: Option<String>,
}

#[derive(Deserialize)]
struct Genre {
    name: String,
    count: i32,
}

impl Recording {
    fn artist(&self) -> Option<String> {
        let mut artist = String::new();
        for (index, credit) in self.artist_credit.iter().enumerate() {
            artist.push_str(&credit.name);
            if index + 1 < self.artist_credit.len() {
                artist.push_str(credit.joinphrase.as_deref()?);
            }
        }
        Some(artist)
    }
}

fn recording_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

#[derive(Serialize)]
struct Candidate {
    recording_id: String,
    artist: String,
    title: String,
    genres: Vec<String>,
    // Reserved for sources that provide recording-specific style and release provenance.
    styles: Vec<String>,
    label: &'static str,
}

impl Candidate {
    fn exact(recording: Recording, artist: &str, title: &str) -> Option<Self> {
        let credit = recording.artist()?;
        if normalize(&credit) != artist || normalize(&recording.title) != title {
            return None;
        }
        Some(Self {
            recording_id: recording.id.to_ascii_lowercase(),
            artist: credit,
            title: recording.title,
            genres: vec![],
            styles: vec![],
            label: "",
        })
    }
}

#[derive(Serialize)]
struct Report {
    schema: u8,
    provider: &'static str,
    status: &'static str,
    candidates: Vec<Candidate>,
}

fn lookup(
    request: Request,
    mut fetch: impl FnMut(&str, &[(&str, &str)]) -> Result<Vec<u8>>,
) -> Result<Report> {
    ensure!(
        valid_field(&request.artist) && valid_field(&request.title),
        "artist and title must each contain 1 to 512 UTF-8 bytes without control characters"
    );
    let artist = normalize(&request.artist);
    let title = normalize(&request.title);
    let query = format!(
        "artist:{} AND recording:{}",
        literal(&request.artist),
        literal(&request.title)
    );
    let bytes = fetch(
        "/recording",
        &[("query", &query), ("limit", &PAGE_SIZE.to_string())],
    )?;
    let page: SearchPage =
        serde_json::from_slice(&bytes).context("Invalid MusicBrainz recording search")?;
    ensure!(
        page.recordings.len() <= PAGE_SIZE
            && page.offset == 0
            && page.count >= page.recordings.len(),
        "Inconsistent MusicBrainz search page"
    );
    let incomplete = page.recordings.len() < page.count;
    let mut distinct = BTreeMap::new();
    for recording in page.recordings {
        if let Some(candidate) = Candidate::exact(recording, &artist, &title) {
            ensure!(
                recording_id(&candidate.recording_id),
                "Invalid MusicBrainz recording identifier"
            );
            distinct
                .entry(candidate.recording_id.clone())
                .or_insert(candidate);
        }
    }

    let mut report = Report {
        schema: 1,
        provider: "MusicBrainz",
        status: "no_match",
        candidates: distinct.into_values().collect(),
    };
    if incomplete || report.candidates.len() > 1 {
        report.status = "ambiguous";
    } else if let Some(candidate) = report.candidates.first_mut() {
        let bytes = fetch(
            &format!("/recording/{}", candidate.recording_id),
            &[("inc", "artist-credits genres")],
        )?;
        let mut detail: Recording =
            serde_json::from_slice(&bytes).context("Invalid MusicBrainz recording details")?;
        ensure!(
            detail.id.eq_ignore_ascii_case(&candidate.recording_id),
            "MusicBrainz returned another recording"
        );
        let genres = std::mem::take(&mut detail.genres);
        if Candidate::exact(detail, &artist, &title).is_none() {
            // Search results can lag a merged or edited recording.
            report.candidates.clear();
            return Ok(report);
        }
        let mut voted = genres
            .into_iter()
            .filter(|g| g.count > 0 && valid_field(&g.name) && g.name.len() <= 128)
            .collect::<Vec<_>>();
        voted.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        for genre in voted {
            let name = genre.name.trim();
            if !candidate
                .genres
                .iter()
                .any(|g| normalize(g) == normalize(name))
            {
                candidate.genres.push(name.to_string());
            }
            if candidate.genres.len() == 16 {
                break;
            }
        }
        report.status = "matched";
    }
    Ok(report)
}

/// Accepts one bounded request and writes one response. Provider failures leave stdout empty.
pub fn run(input: impl Read, mut output: impl Write) -> Result<()> {
    let request = catalog_lookup::request(input)?;
    let client = client_builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut first_request = true;
    let report = lookup(request, |path, query| {
        // Each single-request CLI invocation has a fresh process-local pacer. Space the
        // first GET too so sequential invocations still observe MusicBrainz's 1 Hz quota.
        if first_request {
            std::thread::sleep(Duration::from_secs(1));
            first_request = false;
        }
        let response = MusicBrainz::response(&client, 1, path, query).map_err(|error| {
            // reqwest status/transport errors can include the artist/title in their URL.
            match error.downcast::<reqwest::Error>() {
                Ok(http) => anyhow::Error::new(http.without_url()),
                Err(other) => other,
            }
        })?;
        ensure!(
            response.status().is_success(),
            "MusicBrainz returned {}",
            response.status()
        );
        catalog_lookup::read_response(response)
    })?;
    serde_json::to_writer(&mut output, &report)?;
    writeln!(output)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    const A: &str = "8bca1d60-94f7-4d60-8525-000000000001";
    const B: &str = "8bca1d60-94f7-4d60-8525-000000000002";
    fn request() -> Request {
        Request {
            artist: "Björk & D'Angelo".into(),
            title: "Song \"A\" (Club Mix)".into(),
        }
    }
    fn recording(id: &str) -> Value {
        json!({"id":id, "title":"Song \"A\" (Club Mix)",
            "artist-credit":[{"name":"Björk","joinphrase":" & "},{"name":"D'Angelo"}]})
    }
    fn page(count: usize, records: Vec<Value>) -> Value {
        json!({"count":count, "offset":0, "recordings":records})
    }
    fn reply(search: Value, detail: Option<Value>) -> Result<Report> {
        let mut detail = detail;
        let report = lookup(request(), |path, params| {
            if path == "/recording" {
                assert_eq!(
                    params[0].1,
                    r#"artist:"Björk & D'Angelo" AND recording:"Song \"A\" (Club Mix)""#
                );
                return Ok(serde_json::to_vec(&search)?);
            }
            assert_eq!(path, format!("/recording/{A}"));
            assert_eq!(params, &[("inc", "artist-credits genres")]);
            Ok(serde_json::to_vec(
                &detail.take().expect("unexpected detail lookup"),
            )?)
        })?;
        assert!(detail.is_none(), "expected a detail lookup");
        Ok(report)
    }

    #[test]
    fn exact_recording_uses_joined_credits_and_recording_genres_only() {
        let mut detail = recording(A);
        detail["genres"] = json!([{"count":1,"name":"house"},{"count":2,"name":"Dance"},
            {"count":1,"name":"dance"},{"count":0,"name":"ambient"}]);
        detail["releases"] = json!([{"label":"unverified label"}]);
        let result = reply(page(2, vec![recording(A), recording(A)]), Some(detail)).unwrap();
        assert_eq!(result.status, "matched");
        let candidate = &result.candidates[0];
        assert_eq!(candidate.recording_id, A);
        assert_eq!(candidate.genres, ["Dance", "house"]);
        assert!(candidate.label.is_empty() && candidate.styles.is_empty());
        assert_eq!(
            normalize("  BJÖRK    & D'ANGELO "),
            normalize(&request().artist)
        );
    }

    #[test]
    fn partial_results_and_distinct_recordings_never_select_a_candidate() {
        let result = reply(page(2, vec![recording(B), recording(A)]), None).unwrap();
        assert_eq!(result.status, "ambiguous");
        assert_eq!(
            result
                .candidates
                .iter()
                .map(|c| c.recording_id.as_str())
                .collect::<Vec<_>>(),
            [A, B]
        );
        let partial = reply(page(38, vec![recording(A)]), None).unwrap();
        assert_eq!(partial.status, "ambiguous");
        assert!(partial.candidates[0].genres.is_empty());
    }

    #[test]
    fn editions_changes_after_indexing_and_mismatched_identifiers_are_rejected() {
        let mut edit = recording(A);
        edit["title"] = json!("Song \"A\" (Radio Mix)");
        assert_eq!(
            reply(page(1, vec![edit.clone()]), None).unwrap().status,
            "no_match"
        );
        assert_eq!(
            reply(page(1, vec![recording(A)]), Some(edit))
                .unwrap()
                .status,
            "no_match"
        );
        let no_genres = reply(page(1, vec![recording(A)]), Some(recording(A))).unwrap();
        assert_eq!(no_genres.status, "matched");
        assert!(no_genres.candidates[0].genres.is_empty());
        assert!(reply(page(1, vec![recording(A)]), Some(recording(B))).is_err());
        assert!(reply(page(1, vec![recording("../invalid")]), None).is_err());
    }

    #[test]
    fn input_limits_and_query_escaping_precede_provider_requests() {
        assert!(catalog_lookup::read_limited(&b"12345"[..], 4).is_err());
        assert!(catalog_lookup::read_limited(&b"1234"[..], 4).is_ok());
        for invalid in ["", "  ", "\nprivate", &"a".repeat(513)] {
            assert!(lookup(
                Request {
                    artist: invalid.into(),
                    title: "ok".into()
                },
                |_, _| panic!("unexpected network call")
            )
            .is_err());
        }
        assert!(serde_json::from_value::<Request>(
            json!({"artist":"a","title":"b","path":"song.mp3"})
        )
        .is_err());
        assert_eq!(
            literal(r#"x" OR artist:* \name"#),
            r#""x\" OR artist:* \\name""#
        );
    }
}
