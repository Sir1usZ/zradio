use std::collections::BTreeMap;

use crate::library::Track;
use crate::meta::TrackMeta;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistPage {
    pub name: String,
    pub tracks: Vec<usize>,
}

pub fn is_tagged(meta: Option<&TrackMeta>) -> bool {
    meta.is_some_and(|m| !m.artist.trim().is_empty() || !m.album.trim().is_empty())
}

pub fn tagged_indices(metas: &[Option<TrackMeta>]) -> Vec<usize> {
    metas
        .iter()
        .enumerate()
        .filter(|(_, m)| is_tagged(m.as_ref()))
        .map(|(i, _)| i)
        .collect()
}

pub fn untagged_indices(metas: &[Option<TrackMeta>]) -> Vec<usize> {
    metas
        .iter()
        .enumerate()
        .filter(|(_, m)| !is_tagged(m.as_ref()))
        .map(|(i, _)| i)
        .collect()
}

pub fn artist_pages(metas: &[Option<TrackMeta>]) -> Vec<ArtistPage> {
    group_pages(metas, |m| m.artist.trim())
}

pub fn album_pages(metas: &[Option<TrackMeta>]) -> Vec<ArtistPage> {
    group_pages(metas, |m| m.album.trim())
}

fn group_pages(metas: &[Option<TrackMeta>], key: impl Fn(&TrackMeta) -> &str) -> Vec<ArtistPage> {
    let mut map: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, meta) in metas.iter().enumerate() {
        let Some(meta) = meta.as_ref() else {
            continue;
        };
        let name = key(meta);
        if name.is_empty() {
            continue;
        }
        map.entry(name.to_string()).or_default().push(i);
    }
    map.into_iter()
        .map(|(name, tracks)| ArtistPage { name, tracks })
        .collect()
}

pub fn meta_search(tracks: &[Track], metas: &[Option<TrackMeta>], query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    tracks
        .iter()
        .enumerate()
        .filter(|(i, t)| {
            let meta = metas.get(*i).and_then(|m| m.as_ref());
            let title = meta
                .map(|m| m.title.as_str())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(t.title.as_str());
            let artist = meta.map(|m| m.artist.as_str()).unwrap_or("");
            let album = meta.map(|m| m.album.as_str()).unwrap_or("");
            title.to_lowercase().contains(&q)
                || artist.to_lowercase().contains(&q)
                || album.to_lowercase().contains(&q)
                || t.path.to_string_lossy().to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

pub fn open_artist(pages: &[ArtistPage], idx: usize) -> Option<(String, Vec<usize>)> {
    pages
        .get(idx)
        .map(|page| (page.name.clone(), page.tracks.clone()))
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
    fn empty_artist_is_not_a_named_page() {
        let pages = artist_pages(&[Some(meta("")), None]);
        assert!(pages.is_empty());
    }

    #[test]
    fn tagged_and_untagged_split() {
        let metas = vec![
            Some(meta("Avicii")),
            Some(TrackMeta::default()),
            None,
            Some(TrackMeta {
                album: "True".into(),
                ..TrackMeta::default()
            }),
        ];
        assert_eq!(tagged_indices(&metas), vec![0, 3]);
        assert_eq!(untagged_indices(&metas), vec![1, 2]);
    }

    #[test]
    fn artists_skip_untagged() {
        let metas = vec![Some(meta("")), None, Some(meta("Avicii"))];
        let pages = artist_pages(&metas);
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].name, "Avicii");
        assert_eq!(pages[0].tracks, vec![2]);
    }

    #[test]
    fn albums_group_tagged_only() {
        let metas = vec![
            Some(TrackMeta {
                artist: "Avicii".into(),
                album: "True".into(),
                ..TrackMeta::default()
            }),
            Some(TrackMeta::default()),
            Some(TrackMeta {
                artist: "Avicii".into(),
                album: "True".into(),
                ..TrackMeta::default()
            }),
            Some(TrackMeta {
                artist: "Zedd".into(),
                album: "Clarity".into(),
                ..TrackMeta::default()
            }),
        ];
        let pages = album_pages(&metas);
        assert_eq!(pages[0].name, "Clarity");
        assert_eq!(pages[0].tracks, vec![3]);
        assert_eq!(pages[1].name, "True");
        assert_eq!(pages[1].tracks, vec![0, 2]);
    }

    #[test]
    fn meta_search_matches_title_artist_album() {
        let tracks = vec![
            crate::library::Track {
                path: std::path::PathBuf::from("/m/a.flac"),
                title: "Levels".into(),
            },
            crate::library::Track {
                path: std::path::PathBuf::from("/m/b.flac"),
                title: "filename".into(),
            },
        ];
        let metas = vec![
            Some(TrackMeta {
                title: "Levels".into(),
                artist: "Avicii".into(),
                album: "True".into(),
                ..TrackMeta::default()
            }),
            Some(TrackMeta {
                title: "Clarity".into(),
                artist: "Zedd".into(),
                album: "Clarity".into(),
                ..TrackMeta::default()
            }),
        ];
        assert_eq!(meta_search(&tracks, &metas, "true"), vec![0]);
        assert_eq!(meta_search(&tracks, &metas, "zedd"), vec![1]);
        assert_eq!(meta_search(&tracks, &metas, "levels"), vec![0]);
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

    #[test]
    fn opening_artist_keeps_every_track() {
        let pages = artist_pages(&[
            Some(meta("Avicii")),
            Some(meta("Avicii")),
            Some(meta("Avicii")),
        ]);
        let (name, tracks) = open_artist(&pages, 0).expect("page");
        assert_eq!(name, "Avicii");
        assert_eq!(tracks, vec![0, 1, 2]);
        assert_ne!(tracks, vec![0], "must not collapse to the first song");
    }
}
