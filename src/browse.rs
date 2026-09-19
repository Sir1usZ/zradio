use std::collections::BTreeMap;

use crate::meta::TrackMeta;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistPage {
    pub name: String,
    pub tracks: Vec<usize>,
}

pub fn artist_pages(metas: &[Option<TrackMeta>]) -> Vec<ArtistPage> {
    let mut map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, meta) in metas.iter().enumerate() {
        let name = meta
            .as_ref()
            .map(|m| m.artist.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into());
        map.entry(name).or_default().push(i);
    }
    map.into_iter()
        .map(|(name, tracks)| ArtistPage { name, tracks })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::TrackMeta;

    fn meta(artist: &str) -> TrackMeta {
        TrackMeta {
            artist: artist.into(),
            title: "t".into(),
            ..TrackMeta::default()
        }
    }

    #[test]
    fn empty_artist_stays_unknown() {
        let pages = artist_pages(&[Some(meta("")), None]);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, "unknown");
        assert_eq!(pages[0].tracks, vec![0, 1]);
    }

    #[test]
    fn artists_group_by_name() {
        let metas = vec![
            Some(meta("Avicii")),
            Some(meta("Other")),
            Some(meta("Avicii")),
        ];
        let pages = artist_pages(&metas);
        assert_eq!(pages[0].name, "Avicii");
        assert_eq!(pages[0].tracks, vec![0, 2]);
        assert_eq!(pages[1].name, "Other");
    }
}
