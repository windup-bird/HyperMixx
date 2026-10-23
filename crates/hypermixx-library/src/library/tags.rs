//! Tag reading for the folder source. This probes the container, not the audio: a folder scan must
//! not decode every file just to show its title.

use std::fs::File;
use std::path::Path;

use hypermixx_core::Key;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Tag, Value};
use symphonia::core::probe::Hint;

use super::db::{LibraryError, Result};
use super::model::FileTags;

/// Reads the tags and stream geometry of one audio file.
pub fn read_tags(path: &Path) -> Result<FileTags> {
    let file = File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str()) {
        hint.with_extension(extension);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            stream,
            &FormatOptions {
                enable_gapless: true,
                ..Default::default()
            },
            &MetadataOptions::default(),
        )
        .map_err(|err| LibraryError::Invalid(format!("{}: {err}", path.display())))?;
    let mut format = probed.format;

    let mut tags = FileTags::default();
    if let Some(revision) = format.metadata().current() {
        let tags_slice = revision.tags();
        tags.title = tag_text(tags_slice, StandardTagKey::TrackTitle);
        tags.artist = tag_text(tags_slice, StandardTagKey::Artist);
        tags.album = tag_text(tags_slice, StandardTagKey::Album);
        tags.genre = tag_text(tags_slice, StandardTagKey::Genre);
        tags.year = tag_text(tags_slice, StandardTagKey::Date)
            .or_else(|| tag_text(tags_slice, StandardTagKey::ReleaseDate))
            .or_else(|| tag_text(tags_slice, StandardTagKey::OriginalDate))
            .and_then(|text| text.get(..4)?.parse().ok());
        tags.bpm = tag_text(tags_slice, StandardTagKey::Bpm)
            .and_then(|text| text.trim().replace(',', ".").parse::<f32>().ok())
            .filter(|bpm| bpm.is_finite() && *bpm > 0.0);
        tags.key = tags_slice
            .iter()
            .find(|tag| is_key_tag(tag))
            .and_then(tag_text_value)
            .and_then(|text| Key::parse(&text));
    }

    let params = format
        .tracks()
        .iter()
        .map(|track| &track.codec_params)
        .find(|params| params.sample_rate.is_some())
        .ok_or_else(|| LibraryError::Invalid(format!("{}: no audio stream", path.display())))?;
    tags.sample_rate = params.sample_rate.unwrap_or(44_100);
    tags.channels = params
        .channels
        .map(|channels| channels.count() as u32)
        .unwrap_or(2);
    tags.duration_frames = params.n_frames.unwrap_or(0);
    Ok(tags)
}

fn tag_text(tags: &[Tag], key: StandardTagKey) -> Option<String> {
    tags.iter()
        .find(|tag| tag.std_key == Some(key))
        .and_then(tag_text_value)
}

fn tag_text_value(tag: &Tag) -> Option<String> {
    match &tag.value {
        Value::String(value) => Some(value.clone()),
        Value::UnsignedInt(value) => Some(value.to_string()),
        Value::SignedInt(value) => Some(value.to_string()),
        Value::Float(value) => Some(value.to_string()),
        Value::Boolean(value) => Some(value.to_string()),
        _ => None,
    }
}

/// Some taggers write the key as a free-form `initialkey`/`tkey` tag instead of a standard key.
fn is_key_tag(tag: &Tag) -> bool {
    matches!(
        tag.key.to_ascii_lowercase().as_str(),
        "initialkey" | "initial_key" | "key" | "tkey"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_tag_names_are_recognised() {
        let tag = Tag::new(None, "initialkey", Value::String("8A".to_owned()));
        assert!(is_key_tag(&tag));
        assert!(!is_key_tag(&Tag::new(
            None,
            "comment",
            Value::String("8A".to_owned())
        )));
    }
}
