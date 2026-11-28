//! Read-only Deezer catalog lookup. A search hit must agree with the track and its linked album.
use super::{DeezerGenres, DeezerResponse, SearchResults, COOLDOWN, RATE_LIMIT_CODE, REQUESTS};
use crate::catalog_lookup::{self, identity, valid_text, Candidate, Request};
use crate::cooldown::{retry_delay, ProviderCooldown};
use crate::request_policy::PacedRequest;
use anyhow::{ensure, Context, Result};
use reqwest::blocking::Client;
use serde::{de::DeserializeOwned, Deserialize};
use std::{
    io::{Read, Write},
    time::Duration,
};

const PAGE_SIZE: usize = 25;

#[derive(Deserialize)]
struct NamedArtist {
    name: String,
}

#[derive(Deserialize)]
struct AlbumRef {
    id: u64,
}

#[derive(Deserialize)]
struct Recording {
    id: u64,
    title: String,
    title_short: String,
    title_version: Option<String>,
    artist: NamedArtist,
    album: Option<AlbumRef>,
}

impl Recording {
    fn exact_title(&self, artist: &str, wanted: &str) -> Option<String> {
        if !valid_text(&self.artist.name)
            || identity(&self.artist.name) != artist
            || !valid_text(&self.title)
            || !valid_text(&self.title_short)
        {
            return None;
        }
        let title = identity(&self.title);
        let Some(version) = self
            .title_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            return (title == wanted).then(|| self.title.trim().to_owned());
        };
        if !valid_text(version) {
            return None;
        }
        let short = self.title_short.trim();
        let core = version
            .strip_prefix('(')
            .and_then(|v| v.strip_suffix(')'))
            .or_else(|| version.strip_prefix('[').and_then(|v| v.strip_suffix(']')))
            .unwrap_or(version)
            .trim();
        if core.is_empty() {
            return None;
        }
        let edition = identity(core);
        let suffixes = [
            format!(" ({edition})"),
            format!(" [{edition}]"),
            format!(" - {edition}"),
            format!(" {edition}"),
        ];
        let short_id = identity(short);
        if title != short_id {
            // An edition in the display title must agree with the short title and version.
            return (title == wanted
                && suffixes
                    .iter()
                    .any(|suffix| title.strip_prefix(&short_id) == Some(suffix.as_str())))
            .then(|| self.title.trim().to_owned());
        }
        if suffixes.iter().any(|suffix| title.ends_with(suffix)) {
            // The provider already included the edition in both title fields. Require an
            // explicit edition delimiter; a trailing word alone can be part of the song name.
            return (title == wanted && suffixes[..3].iter().any(|suffix| title.ends_with(suffix)))
                .then(|| self.title.trim().to_owned());
        }
        // Some track rows carry the version only in the separate field. Preserve both
        // fields and accept the user's exact edition spelling, including its delimiter.
        [
            format!("{short} {version}"),
            format!("{short} ({core})"),
            format!("{short} [{core}]"),
            format!("{short} - {core}"),
        ]
        .into_iter()
        .find(|title| valid_text(title) && identity(title) == wanted)
    }
}

#[derive(Deserialize)]
struct Release {
    id: u64,
    genres: Option<DeezerGenres>,
    label: Option<String>,
}

struct Response {
    body: Vec<u8>,
    status: reqwest::StatusCode,
    retry_after: Duration,
}

impl Response {
    fn decode<T: DeserializeOwned>(self, cooldown: &ProviderCooldown) -> Result<T> {
        let reply: Result<DeezerResponse<T>, _> = serde_json::from_slice(&self.body);
        if let Ok(DeezerResponse::Error { error }) = &reply {
            if error.code == RATE_LIMIT_CODE {
                return Err(cooldown.pause(self.retry_after).into());
            }
            anyhow::bail!("Deezer API error {}", error.code);
        }
        ensure!(self.status.is_success(), "Deezer returned {}", self.status);
        reply
            .context("Invalid Deezer catalog response")?
            .into_result()
    }
}

fn lookup(
    request: Request,
    output: impl Write,
    cooldown: &ProviderCooldown,
    mut fetch: impl FnMut(&str, &[(&str, &str)]) -> Result<Response>,
) -> Result<()> {
    let artist = identity(&request.artist);
    let title = identity(&request.title);
    let query = format!("{} {}", request.artist.trim(), request.title.trim());
    let limit = PAGE_SIZE.to_string();
    let page: SearchResults<Recording> =
        fetch("/search/track", &[("q", &query), ("limit", &limit)])?.decode(cooldown)?;
    ensure!(
        page.data.len() <= PAGE_SIZE && page.total >= page.data.len(),
        "Inconsistent Deezer search page"
    );
    if page.next.is_some() || page.total > page.data.len() {
        return catalog_lookup::write(output, "Deezer", "ambiguous", vec![]);
    }
    let mut selected = None;
    for track in page.data {
        if track.exact_title(&artist, &title).is_none() {
            continue;
        }
        ensure!(track.id > 0, "Invalid Deezer track identifier");
        let key = (track.id, track.album.map(|a| a.id).filter(|id| *id > 0));
        if selected.is_some_and(|old| old != key) {
            return catalog_lookup::write(output, "Deezer", "ambiguous", vec![]);
        }
        selected = Some(key);
    }
    let Some((track_id, Some(release_id))) = selected else {
        return catalog_lookup::write(output, "Deezer", "no_match", vec![]);
    };
    let track: Recording = fetch(&format!("/track/{track_id}"), &[])?.decode(cooldown)?;
    ensure!(track.id == track_id, "Deezer returned another track");
    let title = track.exact_title(&artist, &title);
    let Some(title) = title.filter(|_| {
        track
            .album
            .as_ref()
            .is_some_and(|album| album.id == release_id)
    }) else {
        return catalog_lookup::write(output, "Deezer", "no_match", vec![]);
    };
    let release: Release = fetch(&format!("/album/{release_id}"), &[])?.decode(cooldown)?;
    ensure!(release.id == release_id, "Deezer returned another album");
    let mut genres: Vec<String> = Vec::new();
    for genre in release.genres.into_iter().flat_map(|g| g.data) {
        let name = genre.name.trim();
        if genre.id > 0
            && valid_text(&genre.name)
            && name.len() <= 128
            && !genres.iter().any(|g| identity(g) == identity(name))
        {
            genres.push(name.to_owned());
            if genres.len() == 16 {
                break;
            }
        }
    }
    let candidate = Candidate {
        track_id: track_id.to_string(),
        release_id: release_id.to_string(),
        artist: track.artist.name.trim().into(),
        title,
        genres,
        genre_scope: "release",
        label: release
            .label
            .filter(|name| valid_text(name) && name.trim().len() <= 128)
            .map(|name| name.trim().to_owned())
            .unwrap_or_default(),
    };
    catalog_lookup::write(output, "Deezer", "matched", vec![candidate])
}

/// Accepts artist and title on stdin and retrieves public, read-only catalog metadata.
pub fn run(input: impl Read, output: impl Write) -> Result<()> {
    let request = catalog_lookup::request(input)?;
    let client = Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(12))
        .build()?;
    // Each CLI process starts with an empty pacer. Keep sequential invocations within
    // the same conservative request spacing used by the ordinary Deezer source.
    COOLDOWN.check()?;
    std::thread::sleep(Duration::from_millis(500));
    lookup(request, output, &COOLDOWN, |path, params| {
        let response = client
            .get(format!("https://api.deezer.com{path}"))
            .query(params)
            .send_paced(&REQUESTS, 2)
            .map_err(|error| match error.downcast::<reqwest::Error>() {
                Ok(http) => anyhow::Error::new(http.without_url()),
                Err(other) => other,
            })?;
        let status = response.status();
        let retry_after = retry_delay(
            response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|header| header.to_str().ok()),
            Duration::from_secs(3),
        );
        let body = catalog_lookup::read_response(response)?;
        Ok(Response {
            body,
            status,
            retry_after,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::collections::VecDeque;

    fn row(id: u64, album: u64) -> Value {
        json!({"id":id, "title":"Light (Club Mix)", "title_short":"Light", "title_version":"(Club Mix)",
            "artist":{"name":"Björk & Friends"}, "album":{"id":album}})
    }

    fn page(rows: Vec<Value>) -> Value {
        json!({"total":rows.len(), "data":rows})
    }

    fn album() -> Value {
        json!({"id":45, "label":" Local Label ", "genres":{"data":[{"id":7,"name":"House"},{"id":8,"name":" house "},{"id":0,"name":"All"}]}})
    }

    fn lookup_fixture(title: &str, pages: Vec<(&str, Value)>) -> Result<Value> {
        let req = Request {
            artist: "BJÖRK  & Friends".into(),
            title: title.into(),
        };
        let mut responses: VecDeque<_> = pages.into();
        let mut output = Vec::new();
        let result = lookup(
            req,
            &mut output,
            &ProviderCooldown::new(),
            |path, params| {
                let (expected, body) = responses.pop_front().expect("unexpected catalog request");
                assert_eq!(path, expected);
                if path == "/search/track" {
                    assert_eq!(
                        params,
                        &[
                            ("q", format!("BJÖRK  & Friends {title}").as_str()),
                            ("limit", "25")
                        ]
                    );
                } else {
                    assert!(params.is_empty());
                }
                Ok(Response {
                    body: serde_json::to_vec(&body)?,
                    status: reqwest::StatusCode::OK,
                    retry_after: Duration::from_secs(27),
                })
            },
        );
        assert!(responses.is_empty());
        if let Err(error) = result {
            assert!(output.is_empty());
            return Err(error);
        }
        Ok(serde_json::from_slice(&output)?)
    }

    #[test]
    fn confirms_one_track_and_its_linked_release_before_returning_album_tags() {
        let mut search = row(123, 45);
        search["title"] = json!("Light"); // Edition present only in title_version on this row.
        let detail = row(123, 45);
        let result = lookup_fixture(
            "light (club mix)",
            vec![
                ("/search/track", page(vec![search.clone(), search])),
                ("/track/123", detail),
                ("/album/45", album()),
            ],
        )
        .unwrap();
        assert_eq!(
            result,
            json!({"schema":2,"provider":"Deezer","status":"matched","candidates":[{
                "track_id":"123","release_id":"45","artist":"Björk & Friends","title":"Light (Club Mix)",
                "genres":["House"],"genre_scope":"release","label":"Local Label"
            }]})
        );
        let result = lookup_fixture(
            "Light (Club Mix)",
            vec![
                ("/search/track", page(vec![row(123, 45)])),
                ("/track/123", row(123, 45)),
                ("/album/45", json!({"id":45,"genres":null,"label":null})),
            ],
        )
        .unwrap();
        assert_eq!(result["candidates"][0]["genres"], json!([]));
        assert_eq!(result["candidates"][0]["label"], "");
    }

    #[test]
    fn rejects_incomplete_pages_multiple_tracks_and_conflicting_album_links() {
        let mut truncated = page(vec![row(1, 45)]);
        truncated["total"] = json!(26);
        let mut continued = page(vec![row(1, 45)]);
        continued["next"] = json!("https://example.invalid/never-follow");
        for p in [
            truncated,
            continued,
            page(vec![row(1, 45), row(2, 45)]),
            page(vec![row(1, 45), row(1, 46)]),
        ] {
            let result = lookup_fixture("Light (Club Mix)", vec![("/search/track", p)]).unwrap();
            assert_eq!(result["status"], "ambiguous");
            assert_eq!(result["candidates"], json!([]));
        }
    }

    #[test]
    fn skips_wrong_edition_artist_or_missing_release_and_revalidates_details() {
        let mut partial = row(9, 45);
        partial["title"] = json!("Light");
        let mut conflict = row(9, 45);
        conflict["title_version"] = json!("Radio Mix");
        let mut artist = row(9, 45);
        artist["artist"]["name"] = json!("Björk");
        let mut unlinked = row(9, 45);
        unlinked.as_object_mut().unwrap().remove("album");
        for (title, record) in [
            ("Light", partial),
            ("Light (Club Mix)", conflict),
            ("Light (Club Mix)", artist),
            ("Light (Club Mix)", unlinked),
            ("Light (Club Mix)", row(9, 0)),
        ] {
            let result =
                lookup_fixture(title, vec![("/search/track", page(vec![record]))]).unwrap();
            assert_eq!(result["status"], "no_match");
            assert_eq!(result["candidates"], json!([]));
        }
        let result = lookup_fixture(
            "Light (Club Mix)",
            vec![
                ("/search/track", page(vec![row(9, 45)])),
                ("/track/9", row(9, 46)),
            ],
        )
        .unwrap();
        assert_eq!(result["status"], "no_match");
        assert_eq!(result["candidates"], json!([]));
        assert!(lookup_fixture(
            "Light (Club Mix)",
            vec![
                ("/search/track", page(vec![row(9, 45)])),
                ("/track/9", row(10, 45))
            ]
        )
        .is_err());
        assert!(lookup_fixture(
            "Light (Club Mix)",
            vec![
                ("/search/track", page(vec![row(9, 45)])),
                ("/track/9", row(9, 45)),
                ("/album/45", json!({"id":50,"label":"Wrong"}))
            ]
        )
        .is_err());
    }

    #[test]
    fn api_quota_error_is_typed_and_produces_no_json() {
        let result = lookup_fixture(
            "Light (Club Mix)",
            vec![(
                "/search/track",
                json!({
                    "error":{"code":4}, "data":[], "total":0
                }),
            )],
        )
        .unwrap_err();
        let limited = result.downcast_ref::<crate::ProviderRateLimited>().unwrap();
        assert_eq!(limited.retry_after_seconds(), 27);
    }
}
