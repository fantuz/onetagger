use std::collections::HashMap;
use anyhow::Error;
use chrono::Timelike;
use std::path::Path;
use id3::{Version, Tag, Timestamp, Content, TagLike, Encoder, Frame, Encoding};
use id3::frame::{Picture, PictureType, Comment, Lyrics, Popularimeter, ExtendedText, SynchronisedLyrics, TimestampFormat, SynchronisedLyricsType};
use serde::{Serialize, Deserialize};
use crate::{TagDate, CoverType, Field, TagImpl};

const COVER_TYPES: [(PictureType, CoverType); 21] = [
    (PictureType::Other, CoverType::Other),
    (PictureType::Icon, CoverType::Icon),
    (PictureType::OtherIcon, CoverType::OtherIcon),
    (PictureType::CoverFront, CoverType::CoverFront),
    (PictureType::CoverBack, CoverType::CoverBack),
    (PictureType::Leaflet, CoverType::Leaflet),
    (PictureType::Media, CoverType::Media),
    (PictureType::LeadArtist, CoverType::LeadArtist),
    (PictureType::Artist, CoverType::Artist),
    (PictureType::Conductor, CoverType::Conductor),
    (PictureType::Band, CoverType::Band),
    (PictureType::Composer, CoverType::Composer),
    (PictureType::Lyricist, CoverType::Lyricist),
    (PictureType::RecordingLocation, CoverType::RecordingLocation),
    (PictureType::DuringRecording, CoverType::DuringRecording),
    (PictureType::DuringPerformance, CoverType::DuringPerformance),
    (PictureType::ScreenCapture, CoverType::ScreenCapture),
    (PictureType::BrightFish, CoverType::BrightFish),
    (PictureType::Illustration, CoverType::Illustration),
    (PictureType::BandLogo, CoverType::BandLogo),
    (PictureType::PublisherLogo, CoverType::PublisherLogo),
];

#[derive(Debug, Clone, PartialEq)]
pub enum ID3AudioFormat {
    MP3, AIFF, WAV
}

pub struct ID3Tag {
    tag: Tag,
    version_on_disk: Option<Version>,
    pub format: ID3AudioFormat,
    pub id3_separator: String,
    pub id3v24: bool,
    pub comm_lang: String,
}

const ONETAGGER_RATING_OWNER: &str = "no@email";

fn is_text_list_frame(id: &str) -> bool {
    id.len() == 4 && id.starts_with('T') && !matches!(id, "TXXX" | "TRCK" | "TPOS")
        && id.bytes().all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

impl ID3Tag {
    // Read from file
    pub fn load_file(path: impl AsRef<Path>) -> Result<ID3Tag, Error> {
        let ext = path.as_ref().extension().unwrap_or_default().to_ascii_lowercase();
        let format = match ext.to_str() {
            Some("mp3") => ID3AudioFormat::MP3,
            Some("aif" | "aiff") => ID3AudioFormat::AIFF,
            Some("wav") => ID3AudioFormat::WAV,
            _ => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Unsupported format!").into()),
        };
        let (tag, version_on_disk) = if format == ID3AudioFormat::WAV {
            crate::wav::read_wav(path)?
        } else {
            let tag = Tag::read_from_path(path)?;
            let version = tag.version();
            (tag, Some(version))
        };
        Ok(ID3Tag {
            tag,
            version_on_disk,
            format,
            id3_separator: String::from(", "),
            id3v24: version_on_disk.unwrap_or_default() == Version::Id3v24,
            comm_lang: "eng".to_string(),
        })
    }

    // Load tag from file or create new
    pub fn load_or_new(path: impl AsRef<Path>) -> Result<ID3Tag, Error> {
        let ext = path.as_ref().extension().unwrap_or_default().to_ascii_lowercase();
        let format = if ext == "mp3" {
            ID3AudioFormat::MP3
        } else if ext == "wav" {
            ID3AudioFormat::WAV
        } else {
            ID3AudioFormat::AIFF
        };

        match ID3Tag::load_file(&path) {
            Ok(tag) => Ok(tag),
            Err(e) if e.downcast_ref::<id3::Error>().is_some_and(|e| matches!(e.kind, id3::ErrorKind::NoTag)) => {
                Ok(ID3Tag {
                    tag: Tag::new(),
                    version_on_disk: None,
                    format,
                    id3_separator: String::from(", "),
                    id3v24: true,
                    comm_lang: "eng".to_string()
                })
            }
            Err(e) => Err(e),
        }
    }

    // ID3 Settings
    pub fn set_id3v24(&mut self, id3v24: bool) {
        self.id3v24 = id3v24;
    }

    pub fn has_id3v24_on_disk(&self) -> bool {
        self.version_on_disk == Some(Version::Id3v24)
    }

    pub fn native_text_values(&self) -> HashMap<String, Vec<String>> {
        let mut values = HashMap::new();
        if self.has_id3v24_on_disk() {
            for frame in self.tag.frames() {
                let id = frame.id();
                if !is_text_list_frame(id) {
                    continue;
                }
                if let Some(text) = frame.content().text().filter(|text| text.contains('\0')) {
                    values.entry(id.to_owned()).or_insert_with(|| text.split('\0').map(String::from).collect());
                }
            }
        }
        values
    }

    pub(crate) fn set_native_text_values(&mut self, id: &str, values: Vec<String>) -> Result<(), Error> {
        anyhow::ensure!(is_text_list_frame(id),
            "Expected an ID3 text frame that accepts multiple values");
        anyhow::ensure!(values.iter().all(|value| !value.contains('\0')), "An ID3 text value contains a NUL character");
        let values: Vec<_> = values.into_iter().filter(|value| !value.is_empty()).collect();
        self.tag.remove(id);
        if !values.is_empty() {
            self.tag.set_text_values(id, values);
        }
        Ok(())
    }

    fn upgrade_legacy_recording_date(&mut self) {
        let year = self.tag.get("TYER").and_then(|frame| frame.content().text());
        if self.tag.get("TDRC").is_some()
            || !year.is_some_and(|text| text.len() == 4 && text.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return;
        }
        let Some(date) = self.get_date() else { return };
        let clock = self.tag.get("TIME").and_then(|frame| {
            let text = frame.content().text()?;
            if date.day.is_none() || text.len() != 4 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            chrono::NaiveTime::parse_from_str(text, "%H%M").ok()
        });
        self.tag.set_date_recorded(Timestamp {
            year: date.year,
            month: date.month,
            day: date.day,
            hour: clock.map(|time| time.hour() as u8),
            minute: clock.map(|time| time.minute() as u8),
            second: None,
        });
        self.tag.remove("TYER");
        if date.day.is_some() {
            self.tag.remove("TDAT");
        }
        if clock.is_some() {
            self.tag.remove("TIME");
        }
    }

    fn mirror_legacy_recording_date(&mut self) {
        let Some(date) = self.tag.date_recorded() else { return };
        self.tag.set_year(date.year);
        self.tag.remove("TDAT");
        self.tag.remove("TIME");
        let day_month = date.month.zip(date.day).filter(|(month, day)| {
            chrono::NaiveDate::from_ymd_opt(date.year, (*month).into(), (*day).into()).is_some()
        });
        let time = date.hour.zip(date.minute).filter(|(hour, minute)| *hour < 24 && *minute < 60);
        if let Some((month, day)) = day_month {
            self.tag.set_text("TDAT", format!("{day:02}{month:02}"));
            if let Some((hour, minute)) = time {
                self.tag.set_text("TIME", format!("{hour:02}{minute:02}"));
            }
        }
        // Keep the richer timestamp as an extension when v2.3 cannot express it.
        if (date.month.is_none() || day_month.is_some())
            && (date.hour.is_none() || day_month.is_some() && time.is_some()) && date.second.is_none()
        {
            self.tag.remove_date_recorded();
        }
    }

    /// Set COMM frame language
    pub fn set_comm_lang(&mut self, lang: String) {
        self.comm_lang = lang;
    }

    // Read and write all comments
    pub fn get_comments(&self) -> Vec<ID3Comment> {
        self.tag.comments().map(|c| c.clone().into()).collect()
    }

    pub fn set_comments(&mut self, comments: &Vec<ID3Comment>) {
        self.tag.remove("COMM");
        for c in comments {
            let comment: Comment = c.clone().into();
            self.tag.add_frame(comment);
        }
    }

    // Read write all unsynchronized lyrics
    pub fn get_unsync_lyrics(&self) -> Vec<ID3Comment> {
        self.tag.lyrics().map(|l| l.clone().into()).collect()
    }

    pub fn set_unsync_lyrics(&mut self, lyrics: &Vec<ID3Comment>) {
        self.tag.remove("USLT");
        for l in lyrics {
            let lyric: Lyrics = l.clone().into();
            self.tag.add_frame(lyric);
        }
    }

    // POPM
    pub fn get_popularimeter(&self) -> Option<ID3Popularimeter> {
        self.tag.frames().filter_map(|frame| frame.content().popularimeter())
            .min_by_key(|frame| frame.user != ONETAGGER_RATING_OWNER)
            .cloned().map(Into::into)
    }

    pub fn set_popularimeter(&mut self, popm: &ID3Popularimeter) {
        // ID3 identifies a POPM frame by owner. add_frame replaces only that owner.
        let popularimeter: Popularimeter = popm.clone().into();
        self.tag.add_frame(popularimeter);
    }

    // Convert between different cover/picture types
    fn picture_type(&self, cover_type: &CoverType) -> PictureType {
        COVER_TYPES.iter().find(|(_, c)| c == cover_type).unwrap().0
    }
    fn cover_type(&self, picture_type: &PictureType) -> CoverType {
        COVER_TYPES.iter().find(|(p, _)| p == picture_type).unwrap_or(
            &(PictureType::Undefined(20u8), CoverType::Undefined)
        ).1.clone()
    }

}

impl TagImpl for ID3Tag {
    // Write tag to file
    fn save_file(&mut self, path: &Path) -> Result<(), Error> {
        let version = match self.id3v24 {
            true => Version::Id3v24,
            false => Version::Id3v23
        };
        if self.id3v24 && matches!(self.version_on_disk, Some(Version::Id3v22 | Version::Id3v23)) {
            self.upgrade_legacy_recording_date();
        } else if !self.id3v24 && self.has_id3v24_on_disk() {
            self.mirror_legacy_recording_date();
        }

        // Fix art for serato
        if !self.id3v24 {
            let pictures = self.tag.pictures().map(|p| p.clone()).collect::<Vec<_>>();
            self.tag.remove_all_pictures();
            for picture in pictures {
                self.tag.add_frame(Frame::with_content("APIC", Content::Picture(picture)).set_encoding(Some(Encoding::Latin1)));
            }
        }

        // Fix TRCK when converting ID3v2.3 to v2.4
        // Caused by rust-id3 library replacing \0 with / on v2.3
        // get_raw has abstraction over it so should be fine
        if self.id3v24 {
            self.set_raw("TRCK", self.get_raw("TRCK").unwrap_or(vec![]), true);
            self.set_raw("TPOS", self.get_raw("TPOS").unwrap_or(vec![]), true);
        }

        match self.format {
            ID3AudioFormat::MP3 => crate::file_ops::atomic_update(path, |staged| {
                Encoder::new()
                    .version(version)
                    .padding(2048)
                    .write_to_path(&self.tag, staged)?;
                Ok(())
            })?,
            ID3AudioFormat::AIFF => crate::file_ops::atomic_update(path, |staged| {
                self.tag.write_to_path(staged, version)?;
                Ok(())
            })?,
            ID3AudioFormat::WAV => crate::file_ops::atomic_rewrite(path, |source, staged| {
                crate::wav::write_wav(source, staged, &self.tag, version)
            })?,
        }
        self.version_on_disk = Some(version);
        Ok(())
    }

    fn set_separator(&mut self, separator: &str) {
        self.id3_separator = separator.replace("\\0", "\0");
    }

    // Get all tags
    fn all_tags(&self) -> HashMap<String, Vec<String>> {
        let mut tags = HashMap::new();
        for frame in self.tag.frames() {
            if let Some(v) = self.get_raw(frame.id()) {
                tags.insert(frame.id().to_owned(), v);
            }
        }
        // Add TXXX
        for extended in self.tag.extended_texts() {
            let separator = if self.has_id3v24_on_disk() && extended.value.contains('\0') { "\0" } else { &self.id3_separator };
            tags.insert(extended.description.to_string(), extended.value.split(separator).map(String::from).collect());
        }
        tags
    }

    // Set date to tag
    fn set_date(&mut self, date: &TagDate, overwrite: bool) {
        // ID3 v2.3
        if !self.id3v24 {
            if !overwrite && self.tag.date_recorded().is_some() {
                return;
            }
            // Year
            if overwrite || self.tag.get("TYER").is_none() {
                // Remove v2.4
                self.tag.remove_date_recorded();
                self.tag.set_text("TYER", date.year.to_string());
            }
            // Date
            if let (Some(month), Some(day)) = (date.month, date.day) {
                let fill_day_month = self.tag.get("TDAT").is_none() && self.tag.year() == Some(date.year);
                if overwrite || fill_day_month {
                    self.tag.remove_date_recorded();
                    self.tag.set_text("TDAT", format!("{day:02}{month:02}"));
                }
            } else if overwrite {
                self.tag.remove("TDAT");
            }
            return;
        }
        // ID3 v2.4
        if overwrite || self.get_date().is_none() {
            let ts = Timestamp {
                year: date.year,
                month: date.month,
                day: date.day,
                hour: None,
                minute: None,
                second: None
            };
            self.tag.set_date_recorded(ts);
        }
    }

    // Set publish date
    fn set_publish_date(&mut self, date: &TagDate, overwrite: bool) {
        if overwrite || self.tag.date_released().is_none() {
            let ts = Timestamp {
                year: date.year,
                month: date.month,
                day: date.day,
                hour: None,
                minute: None,
                second: None
            };
            self.tag.set_date_released(ts);
        }
    }

    // Rating
    fn get_rating(&self) -> Option<u8> {
        let rating = self.get_popularimeter()?.rating;
        (rating != 0).then(|| rating.div_ceil(51))
    }

    fn set_rating(&mut self, rating: u8, overwrite: bool) {
        if !overwrite && self.get_rating().is_some() { return; }
        let rating = rating.min(5);
        let mut counter = 0;
        let mut has_other_owner = false;
        for frame in self.tag.frames().filter_map(|frame| frame.content().popularimeter()) {
            if frame.user == ONETAGGER_RATING_OWNER { counter = frame.counter; }
            else { has_other_owner = true; }
        }
        // A zero rating means unrated. Keep an explicit owner frame when it carries
        // a play count or must take precedence over another application's rating.
        if rating > 0 || counter > 0 || has_other_owner {
            self.tag.add_frame(Popularimeter { user: ONETAGGER_RATING_OWNER.into(), rating: rating * 51, counter });
        } else {
            // Only remove this owner's frame. Preserve unrecognized POPM encodings.
            for frame in self.tag.remove("POPM") {
                if frame.content().popularimeter().is_none_or(|p| p.user != ONETAGGER_RATING_OWNER) {
                    self.tag.add_frame(frame);
                }
            }
        }
        match rating {
            0 => self.remove_raw("RATING WMP"),
            n => {
                let value = match n { 1 => 1, 5 => 255, _ => (n - 1) * 64 };
                self.set_raw("RATING WMP", vec![value.to_string()], overwrite);
            }
        }
    }

    // Set album art
    fn set_art(&mut self, kind: CoverType, mime: &str, description: Option<&str>, data: Vec<u8>) {
        let picture_type = self.picture_type(&kind);
        self.tag.remove_picture_by_type(picture_type);
        self.tag.add_frame(Picture {
            mime_type: mime.to_string(),
            picture_type,
            description: description.unwrap_or("Cover").to_owned(),
            data
        });
    }
    // Get album art by type
    fn get_art(&self) -> Vec<crate::Picture> {
        self.tag.pictures().map(
            |p| crate::Picture {
                kind: self.cover_type(&p.picture_type),
                description: p.description.to_string(),
                data: p.data.clone(),
                mime: p.mime_type.to_string()
            }
        ).collect()
    }
    // Check if has album art
    fn has_art(&self) -> bool {
        self.tag.pictures().next().is_some()
    }
    fn remove_art(&mut self, kind: CoverType) { 
        // Remove all undefined kinds
        if kind == CoverType::Undefined {
            self.tag.pictures()
                .filter(|p| matches!(p.picture_type, PictureType::Undefined(_)))
                .map(|p| p.picture_type)
                .collect::<Vec<_>>()
                .into_iter()
                .for_each(|t| self.tag.remove_picture_by_type(t));
            return;
        }
        self.tag.remove_picture_by_type(self.picture_type(&kind));
    }

    // Set/Get named field
    fn set_field(&mut self, field: Field, mut value: Vec<String>, overwrite: bool) {
        // Modify duration from seconds to milliseconds
        if field == Field::Duration {
            value[0] = format!("{}000", value[0]);
        }
        self.set_raw(field.id3(), value, overwrite);
    }
    fn get_field(&self, field: Field) -> Option<Vec<String>> {
        // Overrides
        if field == Field::TrackNumber {
            return self.tag.track().map(|v| vec![v.to_string()]);
        }
        if field == Field::TrackTotal {
            return self.tag.total_tracks().map(|v| vec![v.to_string()]);
        }
        if field == Field::DiscNumber {
            return self.tag.disc().map(|v| vec![v.to_string()]);
        }

        self.get_raw(field.id3())
    }

    // Set/Get by tag
    fn set_raw(&mut self, tag: &str, value: Vec<String>, overwrite: bool) {
        // TXXX
        if tag.len() != 4 {
            if overwrite || self.get_raw(tag).is_none() {
                // ID3 readers can retain the same description in multiple text encodings.
                self.tag.remove_extended_text(Some(tag), None);
                if !value.is_empty() {
                    self.tag.add_frame(ExtendedText {
                        description: tag.to_string(),
                        value: value.join(&self.id3_separator),
                    });
                }
            }
            return;
        }

        // COMM tag override for compatibility with DJ apps
        if tag.to_uppercase() == "COMM" {
            if overwrite || self.tag.comments().next().is_none() {
                let mut comment = match self.tag.comments().cloned().next() {
                    Some(comment) => comment.to_owned(),
                    None => Comment {
                        lang: self.comm_lang.to_string(),
                        description: String::new(),
                        text: String::new()
                    }
                };
                comment.lang = self.comm_lang.to_owned();
                // Add value
                self.tag.remove("COMM");
                if !value.is_empty() {
                    comment.text = value.join(&self.id3_separator);
                    self.tag.add_frame(comment);
                }
            }
            return;
        }

        // USLT override so can be used as normal tag
        if tag.to_uppercase() == "USLT" {
            if overwrite || self.tag.lyrics().next().is_none() {
                self.tag.remove_all_lyrics();
                self.tag.add_frame(Lyrics {
                    lang: "eng".to_string(),
                    description: String::new(),
                    text: value.join(&self.id3_separator),
                });
            }
            return;
        }

        
        // Normal
        if overwrite || self.tag.get(tag).is_none() {
            // Remove if empty
            if value.is_empty() {
                self.tag.remove(tag);
                return;
            }
            self.tag.set_text(tag, value.join(&self.id3_separator));
        }
    }

    // Get raw TEXT field
    fn get_raw(&self, tag: &str) -> Option<Vec<String>> {
        // Custom tag (TXXX)
        if tag.len() != 4 {
            if let Some(t) = self.tag.extended_texts().find(|t| t.description == tag) {
                if self.has_id3v24_on_disk() && t.value.contains('\0') {
                    return Some(t.value.split('\0').map(String::from).collect());
                }
                return Some(vec![t.value.to_string()]);
            }
            return None;
        }

        // COMM tag override for compatibility with DJ apps
        if tag.to_uppercase() == "COMM" {
            return match self.tag.comments().next() {
                Some(comment) => Some(comment.text.split(&self.id3_separator).map(String::from).collect()),
                None => None
            };
        }

        // USLT override
        if tag.to_uppercase() == "USLT" {
            return match self.tag.lyrics().next() {
                Some(lyrics) => Some(lyrics.text.split(&self.id3_separator).map(String::from).collect()),
                None => None
            };
        }

        // Get tag
        if let Some(t) = self.tag.get(tag) {
            if let Some(content) = t.content().text() {
                if self.has_id3v24_on_disk() && content.contains('\0') {
                    return Some(if matches!(tag, "TRCK" | "TPOS") {
                        vec![content.replace('\0', "/")]
                    } else {
                        content.split('\0').map(String::from).collect()
                    });
                }
                // In ID3v2.2/3 strings are NULL terminated, so they can't contain null bytes.
                // Because of that the ID3 library replaces \0 with / and then back
                if matches!(self.version_on_disk.unwrap_or(self.tag.version()), Version::Id3v22 | Version::Id3v23)
                    && content.contains("\0")
                {
                    return Some(content.replace("\0", "/").split(&self.id3_separator).map(String::from).collect());
                }
                Some(content.split(&self.id3_separator).map(String::from).collect())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn remove_raw(&mut self, tag: &str) {
        // TXXX
        if tag.len() != 4 {
            self.tag.remove_extended_text(Some(tag), None);
            return;
        }

        self.tag.remove(tag);
    }

    fn get_date(&self) -> Option<TagDate> {
        if let Some(date) = self.tag.get("TDRC").and_then(|frame| frame.content().text()).and_then(TagDate::parse) {
            return Some(date);
        }
        // ID3v2.3 stores the year and the optional DDMM in separate frames.
        let year = self.tag.get("TYER")?.content().text()?.parse().ok()?;
        let calendar = self.tag.get("TDAT").and_then(|frame| {
            let text = frame.content().text()?;
            if text.len() != 4 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let day: u8 = text[..2].parse().ok()?;
            let month: u8 = text[2..].parse().ok()?;
            chrono::NaiveDate::from_ymd_opt(year, month.into(), day.into())?;
            Some((month, day))
        });
        let (month, day) = calendar.unzip();
        Some(TagDate { year, month, day })
    }

    fn set_track_number(&mut self, track_number: &str, track_total: Option<u16>, overwrite: bool) {
        let mut value = format!("{track_number}");
        if let Some(total) = track_total {
            value = format!("{value}/{total}");
        }
        self.set_raw("TRCK", vec![value], overwrite);
    }

    fn set_lyrics(&mut self, lyrics: &crate::Lyrics, synced: bool, overwrite: bool) -> bool {
        let code = lyrics.language.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
        let lang = if code.len() == 3 && code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
            &code
        } else {
            isolang::Language::from_639_1(&code).map_or("XXX", |language| language.to_639_3())
        };
        if synced {
            if !overwrite && self.tag.synchronised_lyrics().next().is_some() {
                return true;
            }
            let content: Vec<_> = lyrics.iter_lines().filter_map(|line| {
                let start = u32::try_from(line.start?.as_millis()).ok()?;
                Some((start, line.text.to_string()))
            }).collect();
            if content.is_empty() {
                return false;
            }
            self.tag.remove_all_synchronised_lyrics();
            self.tag.add_frame(SynchronisedLyrics {
                lang: lang.into(),
                timestamp_format: TimestampFormat::Ms,
                content_type: SynchronisedLyricsType::Lyrics,
                description: "Lyrics".to_string(),
                content,
            });
            return true;
        }
        if overwrite || self.tag.lyrics().next().is_none() {
            self.tag.remove_all_lyrics();
            self.tag.add_frame(Lyrics {
                lang: lang.into(),
                description: "Lyrics".to_string(),
                text: lyrics.text()
            });
        }
        true
    }

    fn set_explicit(&mut self, explicit: bool) {
        self.set_raw("ITUNESADVISORY", vec![
            match explicit { true => "1", false => "2" }.to_string()
        ], true)
    }

    fn get_separator(&self) -> Option<String> {
        Some(self.id3_separator.clone())
    }
    
    
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ID3Popularimeter {
    pub email: String,
    pub rating: u8,
    pub counter: u64
}

impl ID3Popularimeter {
    pub fn new(email: &str, rating: u8, counter: u64) -> ID3Popularimeter {
        ID3Popularimeter {
            email: email.to_string(),
            rating, counter
        }
    }
}

impl Into<Popularimeter> for ID3Popularimeter {
    fn into(self) -> Popularimeter {
        Popularimeter {
            user: self.email,
            rating: self.rating,
            counter: self.counter,
        }
    }
}

impl From<Popularimeter> for ID3Popularimeter {
    fn from(p: Popularimeter) -> Self {
        ID3Popularimeter {
            email: p.user,
            rating: p.rating,
            counter: p.counter
        }
    }
}

// Reimplementations of rust-id3 for serialization
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ID3Comment {
    pub lang: String,
    pub description: String,
    pub text: String
}

impl From<Comment> for ID3Comment {
    fn from(c: Comment) -> Self {
        Self {
            lang: c.lang,
            description: c.description,
            text: c.text.replace("\0", "")
        }
    }
}

impl From<ID3Comment> for Comment {
    fn from(c: ID3Comment) -> Self {
        Self {
            lang: c.lang,
            description: c.description,
            text: c.text
        }
    }
}

// Unsynchronized lyrics have exactly same schema as comment
impl From<Lyrics> for ID3Comment {
    fn from(l: Lyrics) -> Self {
        Self {
            lang: l.lang,
            description: l.description,
            text: l.text.replace("\0", "")
        }
    }
}

impl From<ID3Comment> for Lyrics {
    fn from(c: ID3Comment) -> Self {
        Self {
            lang: c.lang,
            description: c.description,
            text: c.text
        }
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use std::fs;

    #[test]
    fn overwriting_a_custom_field_replaces_all_encodings_of_its_description() {
        for (version, encoding) in [(Version::Id3v23, Encoding::UTF16), (Version::Id3v24, Encoding::UTF8)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("track.mp3");
            let audio = [0u8; 64];
            fs::write(&path, audio).unwrap();
            let mut seed = Tag::new();
            for (description, value, enc) in [
                ("WWWAUDIOFILE", "old-1", encoding),
                ("WWWAUDIOFILE", "old-2", Encoding::Latin1),
                ("UNIQUEFILEID", "keep", Encoding::Latin1),
            ] {
                let frame = Frame::from(ExtendedText { description: description.into(), value: value.into() });
                seed.add_frame(frame.set_encoding(Some(enc)));
            }
            seed.write_to_path(&path, version).unwrap();
            let values = |tag: &ID3Tag| tag.tag.extended_texts()
                .filter(|t| t.description == "WWWAUDIOFILE").map(|t| t.value.clone()).collect::<Vec<_>>();
            let mut tag = ID3Tag::load_file(&path).unwrap();
            assert_eq!(values(&tag), ["old-1", "old-2"]);
            tag.set_raw("WWWAUDIOFILE", vec!["ignored".into()], false);
            assert_eq!(values(&tag), ["old-1", "old-2"]);
            for value in ["new-1", "new-2"] {
                tag.set_raw("WWWAUDIOFILE", vec![value.into()], true);
                tag.save_file(&path).unwrap();
                tag = ID3Tag::load_file(&path).unwrap();
                assert_eq!(values(&tag), [value]);
                assert_eq!(tag.get_raw("UNIQUEFILEID").unwrap(), ["keep"]);
                assert!(fs::read(&path).unwrap().ends_with(&audio));
            }
        }
    }

    #[test]
    fn missing_id3_tag_is_allowed_but_io_and_corruption_errors_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ID3Tag::load_or_new(dir.path().join("missing.mp3")).is_err());
        let path = dir.path().join("track.mp3");
        fs::write(&path, [0u8; 64]).unwrap();
        assert!(ID3Tag::load_or_new(&path).is_ok());
        let broken = b"ID3\x05\x00\x00\x00\x00\x00\x06broken";
        fs::write(&path, broken).unwrap();
        assert!(ID3Tag::load_or_new(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), broken);
    }

    fn wav_bytes() -> Vec<u8> {
        let mut bytes = Vec::from(&b"RIFF"[..]);
        bytes.extend(52u32.to_le_bytes());
        bytes.extend(b"WAVEfmt ");
        bytes.extend(16u32.to_le_bytes());
        bytes.extend(1u16.to_le_bytes()); // PCM
        bytes.extend(1u16.to_le_bytes()); // mono
        bytes.extend(44100u32.to_le_bytes());
        bytes.extend(88200u32.to_le_bytes());
        bytes.extend(2u16.to_le_bytes());
        bytes.extend(16u16.to_le_bytes());
        bytes.extend(b"data");
        bytes.extend(4u32.to_le_bytes());
        bytes.extend([1, 2, 3, 4]);
        bytes.extend(b"JUNK");
        bytes.extend(3u32.to_le_bytes());
        bytes.extend([7, 8, 9, 0]); // Unknown chunk with odd-sized payload and padding.
        bytes
    }

    #[test]
    fn invalid_empty_wav_containers_are_rejected_without_modification() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.wav");
        let mut invalid_riff = wav_bytes();
        invalid_riff[4..8].copy_from_slice(&0u32.to_le_bytes());
        let mut invalid_list = wav_bytes();
        invalid_list.extend(b"LIST\0\0\0\0");
        for bytes in [invalid_riff, invalid_list] {
            fs::write(&path, &bytes).unwrap();
            assert!(ID3Tag::load_or_new(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn wav_tag_round_trip_preserves_audio_and_rejects_truncated_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.wav");
        let original = wav_bytes();
        fs::write(&path, &original).unwrap();
        let mut tag = ID3Tag::load_or_new(&path).unwrap();
        tag.set_field(Field::Title, vec!["Updated title".into()], true);
        tag.save_file(&path).unwrap();
        assert_eq!(ID3Tag::load_file(&path).unwrap().get_field(Field::Title).unwrap(), vec!["Updated title"]);
        tag.set_field(Field::Title, vec!["Second title".into()], true);
        tag.save_file(&path).unwrap();
        assert_eq!(ID3Tag::load_file(&path).unwrap().get_field(Field::Title).unwrap(), vec!["Second title"]);
        let written = fs::read(&path).unwrap();
        let junk_offset = written.windows(4).position(|w| w == b"JUNK").unwrap();
        assert_eq!(&written[junk_offset + 8..junk_offset + 12], &[7, 8, 9, 0]);
        let data_offset = written.windows(4).position(|w| w == b"data").unwrap();
        assert_eq!(&written[data_offset + 8..data_offset + 12], &[1, 2, 3, 4]);
        fs::write(&path, &original[..20]).unwrap();
        assert!(ID3Tag::load_or_new(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), &original[..20]);
    }

    #[test]
    fn wav_riff_comment_saves_in_both_id3_versions_and_can_be_removed() {
        use riff::{ChunkContents, ChunkId, LIST_ID};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("riff-comment.wav");
        let comment = "Échantillon";
        let mut original = wav_bytes();
        let mut wav = std::io::Cursor::new(&mut original);
        wav.set_position(wav.get_ref().len() as u64);
        ChunkContents::Children(LIST_ID.clone(), ChunkId::new("INFO").unwrap(), vec![
            ChunkContents::Data(ChunkId::new("ICMT").unwrap(), format!("{comment}\0").into_bytes()),
        ]).write(&mut wav).unwrap();

        for use_v24 in [false, true] {
            fs::write(&path, &original).unwrap();
            let mut tag = ID3Tag::load_file(&path).unwrap();
            assert!(!tag.has_id3v24_on_disk());
            assert_eq!(tag.get_comments()[0].text, comment);
            tag.set_id3v24(use_v24);
            tag.save_file(&path).unwrap();

            let mut saved = ID3Tag::load_file(&path).unwrap();
            assert_eq!(saved.has_id3v24_on_disk(), use_v24);
            assert_eq!(Tag::read_from_path(&path).unwrap().comments().next().unwrap().text, comment);
            let written = fs::read(&path).unwrap();
            assert!(written.windows(12).any(|bytes| bytes == b"data\x04\0\0\0\x01\x02\x03\x04"));

            saved.set_comments(&vec![]);
            saved.save_file(&path).unwrap();
            assert!(ID3Tag::load_file(&path).unwrap().get_comments().is_empty());
        }
    }

    #[test]
    fn id3v23_release_date_round_trips_calendar_precision() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.mp3");
        fs::write(&path, [0u8; 64]).unwrap();
        let mut tag = ID3Tag::load_or_new(&path).unwrap();
        tag.set_id3v24(false);
        let leap_day = TagDate { year: 2024, month: Some(2), day: Some(29) };
        tag.set_date(&leap_day, true);
        tag.save_file(&path).unwrap();
        let mut tag = ID3Tag::load_file(&path).unwrap();
        let date = tag.get_date().unwrap();
        assert_eq!((date.year, date.month, date.day), (2024, Some(2), Some(29)));

        let year_only = TagDate { year: 2025, month: None, day: None };
        tag.set_date(&year_only, false);
        assert_eq!(tag.get_raw("TDAT").unwrap(), ["2902"]);
        tag.set_date(&year_only, true);
        tag.save_file(&path).unwrap();
        let mut tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_raw("TYER").unwrap(), ["2025"]);
        assert!(tag.get_raw("TDAT").is_none());
        tag.set_date(&leap_day, false);
        assert!(tag.get_raw("TDAT").is_none());
        tag.set_date(&TagDate { year: 2025, month: Some(3), day: Some(1) }, false);
        assert_eq!(tag.get_raw("TDAT").unwrap(), ["0103"]);
    }

    #[test]
    fn invalid_id3v23_day_month_still_returns_the_year() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.mp3");
        fs::write(&path, [0u8; 64]).unwrap();
        let mut tag = ID3Tag::load_or_new(&path).unwrap();
        tag.set_raw("TYER", vec!["2023".into()], true);
        for invalid in ["2902", "3213", "0", "1é2"] {
            tag.set_raw("TDAT", vec![invalid.into()], true);
            let date = tag.get_date().unwrap();
            assert_eq!((date.year, date.month, date.day), (2023, None, None));
        }
    }

    #[test]
    fn timed_and_plain_lyrics_are_independent_and_language_is_normalized() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lyrics.mp3");
        fs::write(&path, [0u8; 64]).unwrap();
        let mut tag = ID3Tag::load_or_new(&path).unwrap();
        let lyrics = crate::Lyrics { language: "fr-CH".into(), paragraphs: vec![vec![
            crate::LyricsLine { text: "Introduction".into(), start: None, end: None, parts: vec![] },
            crate::LyricsLine { text: "Première ligne".into(), start: Some(std::time::Duration::from_millis(1234)),
                end: None, parts: vec![] },
        ]] };
        assert!(lyrics.synced());
        assert!(tag.set_lyrics(&lyrics, true, false));
        tag.save_file(&path).unwrap();
        let mut tag = ID3Tag::load_file(&path).unwrap();
        assert!(tag.get_unsync_lyrics().is_empty());
        let timed = tag.tag.synchronised_lyrics().next().unwrap();
        assert_eq!(timed.lang, "fra");
        assert_eq!(timed.content, [(1234, "Première ligne".to_owned())]);
        assert!(tag.set_lyrics(&lyrics, false, false));
        tag.save_file(&path).unwrap();
        let tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_unsync_lyrics()[0].text, "Introduction\nPremière ligne");
        assert_eq!(tag.tag.synchronised_lyrics().count(), 1);
    }

    #[test]
    fn changing_id3_versions_preserves_calendar_fields_and_existing_release_dates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dates.mp3");
        fs::write(&path, [0u8; 64]).unwrap();
        let mut tag = ID3Tag::load_or_new(&path).unwrap();
        tag.set_raw("TDRC", vec!["2024-02-29T17:35".into()], true);
        tag.save_file(&path).unwrap();
        let mut tag = ID3Tag::load_file(&path).unwrap();
        tag.set_id3v24(false);
        tag.save_file(&path).unwrap();
        let mut tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_raw("TYER").unwrap(), ["2024"]);
        assert_eq!(tag.get_raw("TDAT").unwrap(), ["2902"]);
        assert_eq!(tag.get_raw("TIME").unwrap(), ["1735"]);
        tag.set_id3v24(true);
        tag.set_date(&TagDate::parse("2010").unwrap(), false);
        tag.save_file(&path).unwrap();
        let tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_raw("TDRC").unwrap(), ["2024-02-29T17:35"]);
    }
}
