//! Shared boundaries for read-only provider lookups. A catalog's track and release IDs
//! belong to that provider. MusicBrainz retains its existing recording-MBID response.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub artist: String,
    pub title: String,
}

pub(crate) fn valid_text(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
}

pub(crate) fn identity(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub(crate) fn read_limited(reader: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "JSON exceeds {limit} bytes");
    Ok(bytes)
}

pub(crate) fn request(input: impl Read) -> Result<Request> {
    let req: Request = serde_json::from_slice(&read_limited(input, 8192)?)
        .context("Expected a JSON object with artist and title")?;
    ensure!(
        valid_text(&req.artist) && valid_text(&req.title),
        "artist and title must each contain 1 to 512 UTF-8 bytes without control characters"
    );
    Ok(req)
}

pub(crate) fn read_response(reader: impl Read) -> Result<Vec<u8>> {
    read_limited(reader, 1024 * 1024)
}

#[derive(Serialize)]
pub(crate) struct Candidate {
    pub track_id: String,
    pub release_id: String,
    pub artist: String,
    pub title: String,
    pub genres: Vec<String>,
    pub genre_scope: &'static str,
    pub label: String,
}

#[derive(Serialize)]
struct Report {
    schema: u8,
    provider: &'static str,
    status: &'static str,
    candidates: Vec<Candidate>,
}

pub(crate) fn write(
    mut output: impl Write,
    provider: &'static str,
    status: &'static str,
    candidates: Vec<Candidate>,
) -> Result<()> {
    ensure!(
        matches!(
            (status, candidates.len()),
            ("matched", 1) | ("no_match" | "ambiguous", 0)
        ),
        "A catalog lookup must identify exactly one item before returning tags"
    );
    serde_json::to_writer(
        &mut output,
        &Report {
            schema: 2,
            provider,
            status,
            candidates,
        },
    )?;
    writeln!(output)?;
    Ok(())
}
