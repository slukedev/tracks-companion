//! Writing tags into a staged copy before import.

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::picture::{Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::Tag;

use crate::verify::TagValues;

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error(transparent)]
    Read(#[from] lofty::error::FileParseError),
    #[error(transparent)]
    Write(#[from] lofty::error::FileEncodingError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Sets title, artist, album, album artist, track, disc, year and, when given, the cover.
/// Existing tags the source embedded are kept unless these replace them.
pub fn write_tags(path: &Path, values: &TagValues, cover: Option<&Path>) -> Result<(), TagError> {
    let mut file = lofty::read_from_path(path)?;
    let tag_type = file.primary_tag_type();
    if file.primary_tag_mut().is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let Some(tag) = file.primary_tag_mut() else {
        return Ok(());
    };
    tag.set_title(values.title.clone());
    tag.set_artist(values.artist.clone());
    if let Some(album) = &values.album {
        tag.set_album(album.clone());
    }
    if let Some(album_artist) = &values.album_artist {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.clone());
    }
    if let Some(track) = values.track {
        tag.set_track(track);
    }
    if let Some(disc) = values.disc {
        tag.set_disk(disc);
    }
    if let Some(year) = values.year {
        tag.insert_text(ItemKey::Year, year.to_string());
    }
    if let Some(cover) = cover {
        let bytes = std::fs::read(cover)?;
        if let Ok(mut picture) = Picture::from_reader(&mut bytes.as_slice()) {
            picture.set_pic_type(PictureType::CoverFront);
            tag.remove_picture_type(PictureType::CoverFront);
            tag.push_picture(picture);
        }
    }
    file.save_to_path(path, WriteOptions::default())?;
    Ok(())
}
