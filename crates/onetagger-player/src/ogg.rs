use std::path::{Path, PathBuf};
use anyhow::Error;
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use rodio::Source;
use crate::AudioSource;

pub struct OGGSource { path: PathBuf, duration: u128, codec: FileType }
impl OGGSource {
    pub fn new(path: impl AsRef<Path>) -> Result<Self, Error> {
        let file = lofty::read_from_path(&path)?;
        Ok(Self { path: path.as_ref().into(), duration: file.properties().duration().as_millis(), codec: file.file_type() })
    }
}
impl AudioSource for OGGSource {
    fn duration(&self) -> u128 { self.duration }
    fn get_source(&self) -> Result<Box<dyn Source<Item = i16> + Send>, Error> {
        if self.codec == FileType::Speex {
            return Err(anyhow!("This build supports tag editing only for Speex audio"));
        }
        crate::file_source(&self.path, self.duration)
    }
}
