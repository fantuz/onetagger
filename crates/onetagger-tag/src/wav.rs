use anyhow::{ensure, Error};
use id3::{frame::Comment, Frame, Tag, TagLike, Version};
use riff::{Chunk, ChunkId, LIST_ID, RIFF_ID};
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

const WAVE_ID: ChunkId = ChunkId { value: *b"WAVE" };
const INFO_ID: ChunkId = ChunkId { value: *b"INFO" };

const INFO_FRAMES: [([u8; 4], &str); 6] = [
    (*b"INAM", "TIT2"),
    (*b"IPRD", "TALB"),
    (*b"IART", "TPE1"),
    (*b"IGNR", "TCON"),
    (*b"ISRC", "TSRC"),
    (*b"ICMT", "COMM"),
];

struct WaveLayout {
    passthrough: Vec<Chunk>,
    info: Vec<Chunk>,
    trailing_start: u64,
    file_len: u64,
}

fn is_id3(chunk: &Chunk) -> bool {
    matches!(&chunk.id().value, b"id3 " | b"ID3 ")
}

fn info_frame(chunk: &Chunk) -> Option<&'static str> {
    INFO_FRAMES.iter().find(|(id, _)| *id == chunk.id().value).map(|(_, frame)| *frame)
}

fn padded_end(chunk: &Chunk) -> u64 {
    let length = u64::from(chunk.len());
    chunk.offset() + 8 + length + length % 2
}

fn read_chunk(file: &mut BufReader<File>, start: u64, limit: u64) -> Result<Chunk, Error> {
    ensure!(limit - start >= 8, "Truncated WAV chunk header");
    let chunk = Chunk::read(file, start)?;
    ensure!(padded_end(&chunk) <= limit, "Truncated WAV chunk {:?}", chunk.id().value);
    Ok(chunk)
}

fn is_info(file: &mut BufReader<File>, chunk: &Chunk) -> Result<bool, Error> {
    if chunk.id() != LIST_ID {
        return Ok(false);
    }
    ensure!(chunk.len() >= 4, "Invalid WAV LIST type");
    Ok(chunk.read_type(file)? == INFO_ID)
}

fn read_info(file: &mut BufReader<File>, list: &Chunk, info: &mut Vec<Chunk>) -> Result<(), Error> {
    let mut offset = list.offset() + 12;
    let limit = list.offset() + 8 + u64::from(list.len());
    while offset < limit {
        let chunk = read_chunk(file, offset, limit)?;
        offset = padded_end(&chunk);
        info.push(chunk);
    }
    Ok(())
}

fn read_layout(file: &mut BufReader<File>) -> Result<WaveLayout, Error> {
    let file_len = file.get_ref().metadata()?.len();
    let root = read_chunk(file, 0, file_len)?;
    ensure!(root.id() == RIFF_ID && root.len() >= 4 && root.read_type(file)? == WAVE_ID,
        "Expected a RIFF/WAVE container");
    let mut layout = WaveLayout {
        passthrough: Vec::new(),
        info: Vec::new(),
        trailing_start: padded_end(&root),
        file_len,
    };
    let limit = 8 + u64::from(root.len());
    let mut offset = 12;
    while offset < limit {
        let chunk = read_chunk(file, offset, limit)?;
        offset = padded_end(&chunk);
        if is_info(file, &chunk)? {
            read_info(file, &chunk, &mut layout.info)?;
        } else if info_frame(&chunk).is_some() {
            // Recover misplaced INFO fields written directly inside WAVE.
            layout.info.push(chunk);
        } else if !is_id3(&chunk) {
            layout.passthrough.push(chunk);
        }
    }

    // Older One Tagger versions appended LIST/INFO after the RIFF boundary.
    // Recover those fields, then preserve any other trailing bytes verbatim.
    while file_len - layout.trailing_start >= 4 {
        file.seek(SeekFrom::Start(layout.trailing_start))?;
        let mut id = [0; 4];
        file.read_exact(&mut id)?;
        if id != LIST_ID.value {
            break;
        }
        let list = read_chunk(file, layout.trailing_start, file_len)?;
        if !is_info(file, &list)? {
            break;
        }
        read_info(file, &list, &mut layout.info)?;
        layout.trailing_start = padded_end(&list);
    }
    Ok(layout)
}

fn info_text(file: &mut BufReader<File>, chunk: &Chunk) -> Result<Option<String>, Error> {
    Ok(String::from_utf8(chunk.read_contents(file)?).ok().map(|mut text| {
        text.truncate(text.trim_end_matches('\0').len());
        text
    }))
}

pub(crate) fn read_wav(path: impl AsRef<Path>) -> Result<(Tag, Option<Version>), Error> {
    let mut file = BufReader::new(File::open(path)?);
    let layout = read_layout(&mut file)?;
    file.rewind()?;
    let embedded = id3::no_tag_ok(Tag::read_from2(&mut file))?;
    let version = embedded.as_ref().map(Tag::version);
    let mut tag = embedded.unwrap_or_default();

    for chunk in &layout.info {
        let Some(frame) = info_frame(chunk) else { continue };
        if tag.get(frame).is_some() {
            continue;
        }
        let Some(text) = info_text(&mut file, chunk)? else { continue };
        if frame == "COMM" {
            tag.add_frame(Comment { lang: "XXX".into(), description: String::new(), text });
        } else {
            tag.add_frame(Frame::text(frame, text));
        }
    }
    Ok((tag, version))
}

fn write_chunk(
    file: &mut BufWriter<File>,
    id: &[u8; 4],
    contents: impl FnOnce(&mut BufWriter<File>) -> Result<(), Error>,
) -> Result<(), Error> {
    let start = file.stream_position()?;
    file.write_all(id)?;
    file.write_all(&[0; 4])?;
    contents(file)?;
    let end = file.stream_position()?;
    let length = u32::try_from(end - start - 8)?;
    if length % 2 == 1 {
        file.write_all(&[0])?;
    }
    let padded = file.stream_position()?;
    file.seek(SeekFrom::Start(start + 4))?;
    file.write_all(&length.to_le_bytes())?;
    file.seek(SeekFrom::Start(padded))?;
    Ok(())
}

fn copy_range(
    source: &mut BufReader<File>,
    staged: &mut BufWriter<File>,
    start: u64,
    end: u64,
) -> Result<(), Error> {
    source.seek(SeekFrom::Start(start))?;
    let expected = end - start;
    ensure!(io::copy(&mut source.take(expected), staged)? == expected, "WAV changed while being saved");
    Ok(())
}

/// Rewrite ID3 and INFO while retaining complete non-metadata chunks, including pad bytes.
pub(crate) fn write_wav(source: &Path, staged: &Path, tag: &Tag, version: Version) -> Result<(), Error> {
    let mut source = BufReader::new(File::open(source)?);
    let layout = read_layout(&mut source)?;
    let mut id3 = Vec::new();
    tag.write_to(&mut id3, version)?;
    let mirrors: Vec<_> = INFO_FRAMES.iter().filter_map(|(id, frame)| {
        let text = if *frame == "COMM" {
            tag.comments().find(|comment| comment.description.is_empty()).map(|comment| comment.text.as_str())
        } else {
            tag.get(frame).and_then(|frame| frame.content().text())
        }?;
        Some((id, text))
    }).collect();
    let mut output = BufWriter::new(File::create(staged)?);
    write_chunk(&mut output, b"RIFF", |output| {
        output.write_all(b"WAVE")?;
        for chunk in &layout.passthrough {
            copy_range(&mut source, output, chunk.offset(), padded_end(chunk))?;
        }
        write_chunk(output, b"id3 ", |output| Ok(output.write_all(&id3)?))?;
        if !mirrors.is_empty() || !layout.info.is_empty() {
            write_chunk(output, b"LIST", |output| {
                output.write_all(b"INFO")?;
                for (id, text) in &mirrors {
                    write_chunk(output, id, |output| {
                        output.write_all(text.as_bytes())?;
                        Ok(output.write_all(&[0])?)
                    })?;
                }
                for chunk in &layout.info {
                    if mirrors.iter().any(|(id, _)| **id == chunk.id().value) {
                        continue;
                    }
                    if info_frame(chunk).is_some_and(|frame| tag.get(frame).is_none())
                        && info_text(&mut source, chunk)?.is_some()
                    {
                        // The readable fallback was removed from the edited tag.
                        continue;
                    }
                    copy_range(&mut source, output, chunk.offset(), padded_end(chunk))?;
                }
                Ok(())
            })?;
        }
        Ok(())
    })?;
    copy_range(&mut source, &mut output, layout.trailing_start, layout.file_len)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{id3::ID3Tag, TagImpl};
    use std::fs;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = id.to_vec();
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
        if body.len() % 2 == 1 {
            bytes.push(0x7f);
        }
        bytes
    }

    #[test]
    fn wav_save_preserves_cue_lists_wave_lists_unknown_chunks_and_the_tail() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lists.wav");
        let cue = chunk(b"cue ", &[1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, b'd', b'a', b't', b'a',
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mut associated = b"adtl".to_vec();
        associated.extend(chunk(b"labl", b"\x01\0\0\0Cue 1!\0"));
        let associated = chunk(b"LIST", &associated);
        let mut wave_list = b"wavl".to_vec();
        wave_list.extend(chunk(b"data", &[1, 2, 3, 4]));
        wave_list.extend(chunk(b"slnt", &2u32.to_le_bytes()));
        wave_list.extend(chunk(b"data", &[5, 6]));
        let wave_list = chunk(b"LIST", &wave_list);
        let unknown = chunk(b"JUNK", &[0x11, 0x22, 0x33]);
        let private_info = chunk(b"IXYZ", b"odd");
        let mut wave = b"WAVE".to_vec();
        wave.extend(chunk(b"fmt ", &[1, 0, 1, 0, 0x44, 0xac, 0, 0, 0x88, 0x58, 1, 0, 2, 0, 16, 0]));
        for bytes in [&cue, &associated, &wave_list, &unknown] {
            wave.extend_from_slice(bytes);
        }
        let mut bytes = chunk(b"RIFF", &wave);
        let mut old_info = b"INFO".to_vec();
        old_info.extend(chunk(b"INAM", b"Old title\0"));
        old_info.extend(chunk(b"ICMT", b"Legacy comment\0"));
        old_info.extend_from_slice(&private_info);
        bytes.extend(chunk(b"LIST", &old_info));
        bytes.extend(b"opaque trailing bytes");
        fs::write(&path, bytes).unwrap();

        let mut tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_raw("TIT2").unwrap(), ["Old title"]);
        assert_eq!(tag.get_comments()[0].text, "Legacy comment");
        tag.set_raw("TIT2", vec!["New title".into()], true);
        tag.save_file(&path).unwrap();

        let written = fs::read(&path).unwrap();
        let riff_end = 8 + u32::from_le_bytes(written[4..8].try_into().unwrap()) as usize;
        for original in [&cue, &associated, &wave_list, &unknown, &private_info] {
            assert!(written[..riff_end].windows(original.len()).any(|chunk| chunk == original.as_slice()));
        }
        assert!(written[..riff_end].windows(4).any(|id| id == b"INFO"));
        assert_eq!(&written[riff_end..], b"opaque trailing bytes");
        let tag = ID3Tag::load_file(&path).unwrap();
        assert_eq!(tag.get_raw("TIT2").unwrap(), ["New title"]);
        assert_eq!(tag.get_comments()[0].text, "Legacy comment");
    }
}
