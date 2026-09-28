//! NAA6 audio-frame section rewriting.
//!
//! The PCM/DSD section is deliberately treated as opaque.  Only the length-delimited META and
//! PIC sections and missing position updates are enriched without changing audio samples.

pub const TYPE_POSITION: u32 = 0x04;
pub const TYPE_META: u32 = 0x08;
pub const TYPE_PIC: u32 = 0x10;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataPayload {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub picture: Option<Vec<u8>>,
    pub position: Option<std::time::Duration>,
    pub duration: Option<std::time::Duration>,
}

/// Build the NAA6 metadata section used by HQPlayer's NAA endpoints.
pub fn metadata_section(meta: &MetadataPayload) -> Vec<u8> {
    let mut out = format!(
        "[metadata]\nsong={}\nartist={}\nalbum={}\n",
        escape_line(&meta.title),
        escape_line(&meta.artist),
        escape_line(&meta.album)
    )
    .into_bytes();
    out.push(0);
    out
}

fn escape_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ")
}

/// Rewrite a complete NAA6 frame body. `pcm_len` is a sample count, while the other lengths are
/// byte counts. The input section order is PCM, POS, META, PIC, matching the 32-byte header.
/// When `metadata` is `None`, all sections are preserved byte-for-byte.
#[cfg(test)]
pub fn rewrite_sections(
    header: &mut [u8; 32],
    body: &[u8],
    metadata: Option<&MetadataPayload>,
    sample_bytes: usize,
) -> Result<Vec<u8>, String> {
    rewrite_sections_since(header, body, metadata, None, false, sample_bytes)
}

/// META is a track boundary to NAA clients. Same-track timing/art updates must not emit META.
pub fn rewrite_sections_since(
    header: &mut [u8; 32],
    body: &[u8],
    metadata: Option<&MetadataPayload>,
    previous: Option<&MetadataPayload>,
    refresh_artwork: bool,
    sample_bytes: usize,
) -> Result<Vec<u8>, String> {
    if metadata.is_none() {
        return Ok(body.to_vec());
    }
    let read_len = |range: std::ops::Range<usize>| -> Result<usize, String> {
        Ok(u32::from_le_bytes(
            header[range]
                .try_into()
                .map_err(|_| "invalid NAA frame header".to_string())?,
        ) as usize)
    };
    let lengths = [
        read_len(4..8)?
            .checked_mul(sample_bytes)
            .ok_or("PCM length overflow")?,
        read_len(8..12)?,
        read_len(12..16)?,
        read_len(16..20)?,
    ];
    let total: usize = lengths.iter().sum();
    if total != body.len() {
        return Err("NAA frame section lengths do not match body".into());
    }
    let mut at = 0;
    let pcm = &body[at..at + lengths[0]];
    at += lengths[0];
    let pos = &body[at..at + lengths[1]];
    at += lengths[1];
    let old_meta = &body[at..at + lengths[2]];
    at += lengths[2];
    let old_pic = &body[at..];
    let metadata = metadata.ok_or("metadata unexpectedly absent")?;
    let mask = u32::from_le_bytes(
        header[0..4]
            .try_into()
            .map_err(|_| "invalid NAA frame header")?,
    );
    let track_changed = previous.is_none_or(|old| {
        old.title != metadata.title || old.artist != metadata.artist || old.album != metadata.album
    });
    let picture_changed = previous.is_none_or(|old| old.picture != metadata.picture);
    let fallback_text = if track_changed {
        metadata_section(metadata)
    } else {
        Vec::new()
    };
    let meta = if old_meta.is_empty() {
        fallback_text.as_slice()
    } else {
        old_meta
    };
    // Native metadata also starts a new client track, so pair it with available art.
    let picture = if old_pic.is_empty()
        && (track_changed || picture_changed || refresh_artwork || !old_meta.is_empty())
    {
        metadata.picture.as_deref().unwrap_or(old_pic)
    } else {
        old_pic
    };
    // Refresh the existing track through POS, never by declaring another track. Preserve native
    // position, including explicit empty/clear packets. Unknown position/duration stay unknown.
    let mut fallback_position = String::from("[position]\nstate=PLAYING\n");
    if let Some(position) = metadata.position {
        fallback_position.push_str(&format!("position={}\n", position.as_secs_f64()));
    }
    if let Some(duration) = metadata.duration {
        fallback_position.push_str(&format!("length={}\n", duration.as_secs_f64()));
    }
    fallback_position.push('\0');
    let pos = if pos.is_empty() && mask & TYPE_POSITION == 0 {
        fallback_position.as_bytes()
    } else {
        pos
    };
    let mut new_mask = mask;
    if !meta.is_empty() {
        new_mask |= TYPE_META;
    }
    if !pos.is_empty() {
        new_mask |= TYPE_POSITION;
    }
    if picture.is_empty() {
        new_mask &= !TYPE_PIC;
    } else {
        new_mask |= TYPE_PIC;
    }
    if old_pic.is_empty() && !picture.is_empty() {
        let selector = if is_url_picture(picture) {
            1
        } else if picture.starts_with(&[255, 216, 255]) {
            2
        } else if picture.starts_with(b"\x89PNG\r\n\x1a\n") {
            3
        } else {
            0
        };
        new_mask = (new_mask & 0x00ff_ffff) | (selector << 24);
    }
    header[0..4].copy_from_slice(&new_mask.to_le_bytes());
    header[8..12].copy_from_slice(&(pos.len() as u32).to_le_bytes());
    header[12..16].copy_from_slice(&(meta.len() as u32).to_le_bytes());
    header[16..20].copy_from_slice(&(picture.len() as u32).to_le_bytes());
    let mut out = Vec::with_capacity(pcm.len() + pos.len() + meta.len() + picture.len());
    out.extend_from_slice(pcm);
    out.extend_from_slice(pos);
    out.extend_from_slice(meta);
    out.extend_from_slice(picture);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(mask: u32, pcm: u32, pos: u32, meta: u32, pic: u32) -> [u8; 32] {
        let mut h = [0; 32];
        h[0..4].copy_from_slice(&mask.to_le_bytes());
        h[4..8].copy_from_slice(&pcm.to_le_bytes());
        h[8..12].copy_from_slice(&pos.to_le_bytes());
        h[12..16].copy_from_slice(&meta.to_le_bytes());
        h[16..20].copy_from_slice(&pic.to_le_bytes());
        h
    }

    #[test]
    fn replacement_preserves_pcm_and_existing_picture() {
        let mut h = header(TYPE_META | TYPE_PIC, 4, 2, 0, 3);
        let body = b"PCM!POPIC";
        let replacement = MetadataPayload {
            title: "New\nTitle".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            picture: Some(b"JPEG".to_vec()),
            ..Default::default()
        };
        let out = rewrite_sections(&mut h, body, Some(&replacement), 1).unwrap();
        assert_eq!(&out[..6], b"PCM!PO");
        assert!(out.ends_with(b"PIC"));
        assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 3);
        assert!(String::from_utf8_lossy(&out).contains("song=New Title"));
    }

    #[test]
    fn absent_metadata_is_transparent() {
        let mut h = header(TYPE_META, 2, 0, 2, 0);
        let body = b"PCM!xx";
        assert_eq!(rewrite_sections(&mut h, body, None, 1).unwrap(), body);
        assert_eq!(h, header(TYPE_META, 2, 0, 2, 0));
    }
}

#[cfg(test)]
mod artwork_regressions {
    use super::*;
    #[test]
    fn missing_picture_is_filled_without_replacing_native_text() {
        let native = b"[metadata]\nsong=Native title\n\0";
        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(&TYPE_META.to_le_bytes());
        header[12..16].copy_from_slice(&(native.len() as u32).to_le_bytes());
        let meta = MetadataPayload {
            title: "Fallback".into(),
            picture: Some(vec![255, 216, 255, 217]),
            ..Default::default()
        };
        let body = rewrite_sections(&mut header, native, Some(&meta), 1).unwrap();
        let position_len = u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize;
        let metadata_len = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
        assert_eq!(&body[position_len..position_len + metadata_len], native);
        assert!(body.ends_with(meta.picture.as_ref().unwrap()));
        assert_eq!(
            u32::from_le_bytes(header[0..4].try_into().unwrap()) >> 24,
            2
        );
    }
    #[test]
    fn upstream_picture_and_selector_are_preserved() {
        let mut header = [0u8; 32];
        header[0..4].copy_from_slice(&(TYPE_PIC | (3u32 << 24)).to_le_bytes());
        header[16..20].copy_from_slice(&3u32.to_le_bytes());
        let meta = MetadataPayload {
            picture: Some(vec![255, 216, 255, 217]),
            ..Default::default()
        };
        let body = rewrite_sections(&mut header, b"PNG", Some(&meta), 1).unwrap();
        assert!(body.ends_with(b"PNG"));
        assert_eq!(
            u32::from_le_bytes(header[0..4].try_into().unwrap()) >> 24,
            3
        );
    }
}

#[cfg(test)]
mod url_picture_tests {
    use super::*;
    #[test]
    fn url_picture_uses_selector_one_and_raw_url_bytes() {
        let url = b"http://192.168.1.2:8088/roon/image?image_key=cover%2F1";
        let meta = MetadataPayload {
            picture: Some(url.to_vec()),
            ..Default::default()
        };
        let mut header = [0; 32];
        let body = rewrite_sections(&mut header, &[], Some(&meta), 1).unwrap();
        assert_eq!(u32::from_le_bytes(header[..4].try_into().unwrap()) >> 24, 1);
        assert_eq!(
            u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize,
            url.len()
        );
        assert!(body.ends_with(url));
    }
}

pub fn is_url_picture(picture: &[u8]) -> bool {
    picture.starts_with(b"http://") || picture.starts_with(b"https://")
}

#[cfg(test)]
mod stable_track_tests {
    use super::*;
    use std::time::Duration;

    fn audio_header() -> [u8; 32] {
        let mut h = [0; 32];
        h[..4].copy_from_slice(&2u32.to_le_bytes());
        h[4..8].copy_from_slice(&4u32.to_le_bytes());
        h
    }
    fn metadata() -> MetadataPayload {
        MetadataPayload {
            title: "Track one".into(),
            picture: Some(b"http://endpoint/art".to_vec()),
            position: Some(Duration::from_secs(42)),
            duration: Some(Duration::from_secs(180)),
            ..Default::default()
        }
    }

    #[test]
    fn timing_updates_do_not_reset_identity_or_artwork() {
        let previous = metadata();
        let mut current = previous.clone();
        current.position = Some(Duration::from_secs(43));
        let mut h = audio_header();
        let body =
            rewrite_sections_since(&mut h, b"PCM!", Some(&current), Some(&previous), false, 1)
                .unwrap();
        assert_eq!(&body[..4], b"PCM!");
        assert_eq!(
            u32::from_le_bytes(h[..4].try_into().unwrap()),
            2 | TYPE_POSITION
        );
        assert_eq!(&h[12..20], &[0; 8]);
        let position = String::from_utf8_lossy(&body[4..]);
        assert!(position.contains("position=43\n"));
        assert!(position.contains("length=180\n"));
    }

    #[test]
    fn late_artwork_does_not_start_another_track() {
        let mut previous = metadata();
        previous.picture = None;
        let current = metadata();
        let mut h = audio_header();
        let body =
            rewrite_sections_since(&mut h, b"PCM!", Some(&current), Some(&previous), false, 1)
                .unwrap();
        assert_eq!(
            u32::from_le_bytes(h[..4].try_into().unwrap()) & TYPE_META,
            0
        );
        assert!(body.ends_with(current.picture.as_ref().unwrap()));
    }

    #[test]
    fn actual_track_change_still_sends_identity_and_artwork() {
        let previous = metadata();
        let mut current = previous.clone();
        current.title = "Track two".into();
        let mut h = audio_header();
        let body =
            rewrite_sections_since(&mut h, b"PCM!", Some(&current), Some(&previous), false, 1)
                .unwrap();
        assert_ne!(
            u32::from_le_bytes(h[..4].try_into().unwrap()) & TYPE_META,
            0
        );
        assert!(String::from_utf8_lossy(&body).contains("song=Track two"));
        assert!(body.ends_with(current.picture.as_ref().unwrap()));
    }

    #[test]
    fn explicit_native_position_clear_is_preserved() {
        let current = metadata();
        let mut h = audio_header();
        h[..4].copy_from_slice(&(2 | TYPE_POSITION).to_le_bytes());
        let body =
            rewrite_sections_since(&mut h, b"PCM!", Some(&current), Some(&current), false, 1)
                .unwrap();
        assert_eq!(body, b"PCM!");
        assert_eq!(&h[8..12], &[0; 4]);
    }
}
