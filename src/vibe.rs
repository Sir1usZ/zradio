use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Genre {
    Lofi,
    Jazz,
    Ambient,
    Electronic,
    Synthwave,
    Lounge,
    Indie,
    Live,
}

impl Genre {
    pub const ALL: [Genre; 8] = [
        Genre::Lofi,
        Genre::Jazz,
        Genre::Ambient,
        Genre::Electronic,
        Genre::Synthwave,
        Genre::Lounge,
        Genre::Indie,
        Genre::Live,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Lofi => "lofi",
            Self::Jazz => "jazz",
            Self::Ambient => "ambient",
            Self::Electronic => "electronic",
            Self::Synthwave => "synthwave",
            Self::Lounge => "lounge",
            Self::Indie => "indie",
            Self::Live => "live",
        }
    }

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|g| *g == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|g| *g == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Station {
    pub genre: Genre,
    pub name: &'static str,
    pub url: &'static str,
    pub blurb: &'static str,
}

pub const STATIONS: &[Station] = &[
    Station {
        genre: Genre::Live,
        name: "Xender Live",
        url: "https://www.youtube.com/watch?v=tRsQsTMvPNg",
        blurb: "your 24/7 livestream",
    },
    Station {
        genre: Genre::Lofi,
        name: "Lofi Girl",
        url: "https://www.youtube.com/watch?v=jfKfPfyJRdk",
        blurb: "beats to relax/study to",
    },
    Station {
        genre: Genre::Lofi,
        name: "Chillhop",
        url: "https://www.youtube.com/watch?v=5yx6BWlEVcY",
        blurb: "jazzy lofi coffee shop",
    },
    Station {
        genre: Genre::Jazz,
        name: "SomaFM Fluid",
        url: "http://ice4.somafm.com/fluid-128-mp3",
        blurb: "future soul / liquid trap",
    },
    Station {
        genre: Genre::Jazz,
        name: "Bossa Beyond",
        url: "http://ice4.somafm.com/bossa-128-mp3",
        blurb: "silky brazilian bossa",
    },
    Station {
        genre: Genre::Ambient,
        name: "Groove Salad",
        url: "http://ice2.somafm.com/groovesalad-128-mp3",
        blurb: "the coding classic",
    },
    Station {
        genre: Genre::Ambient,
        name: "Drone Zone",
        url: "http://ice2.somafm.com/dronezone-128-mp3",
        blurb: "atmospheric zero-distraction",
    },
    Station {
        genre: Genre::Electronic,
        name: "The Trip",
        url: "http://ice2.somafm.com/thetrip-128-mp3",
        blurb: "progressive house / trance",
    },
    Station {
        genre: Genre::Electronic,
        name: "Cliqhop IDM",
        url: "http://ice2.somafm.com/cliqhop-128-mp3",
        blurb: "blips, beeps, intricate beats",
    },
    Station {
        genre: Genre::Synthwave,
        name: "Nightwave Plaza",
        url: "http://radio.plaza.one/mp3",
        blurb: "vaporwave / city pop",
    },
    Station {
        genre: Genre::Synthwave,
        name: "Synthwave Radio",
        url: "https://www.youtube.com/watch?v=4xDzrJKXOOY",
        blurb: "neon late-night sessions",
    },
    Station {
        genre: Genre::Lounge,
        name: "Secret Agent",
        url: "http://ice2.somafm.com/secretagent-128-mp3",
        blurb: "cinematic spy lounge",
    },
    Station {
        genre: Genre::Indie,
        name: "SomaFM Lush",
        url: "http://ice2.somafm.com/lush-128-mp3",
        blurb: "mellow vocals, human touch",
    },
];

pub fn stations_for(genre: Genre) -> Vec<&'static Station> {
    STATIONS.iter().filter(|s| s.genre == genre).collect()
}

pub fn pick_vibe() -> Genre {
    let hour = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs() / 3600) % 24)
        .unwrap_or(12);
    match hour {
        0..=5 => Genre::Ambient,
        6..=10 => Genre::Jazz,
        11..=16 => Genre::Lofi,
        17..=20 => Genre::Electronic,
        _ => Genre::Synthwave,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabMood {
    Idle,
    Code,
    Mix,
    Fetch,
}

impl CrabMood {
    pub fn from_state(decoding: bool, fading: bool, streaming: bool, tick: u64) -> Self {
        if decoding {
            Self::Fetch
        } else if fading {
            Self::Mix
        } else if streaming && tick % 8 < 4 {
            Self::Code
        } else if tick.is_multiple_of(11) {
            Self::Fetch
        } else if tick % 5 < 2 {
            Self::Code
        } else {
            Self::Idle
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "chillin",
            Self::Code => "shipping",
            Self::Mix => "mixing",
            Self::Fetch => "fetching",
        }
    }

    pub fn frames(self, tick: u64) -> [&'static str; 4] {
        let phase = (tick / 3) % 2;
        match (self, phase) {
            (Self::Idle, 0) => ["  (•‿•)  ", "  /|_|\\  ", "  🦞     ", " /     \\ "],
            (Self::Idle, _) => ["  (•‿- ) ", "  /|_|\\  ", "  🦞     ", " /     \\ "],
            (Self::Code, 0) => ["  (•̀ᴗ•́)  ", "  /|█|\\  ", "  🦞 ~   ", " /     \\ "],
            (Self::Code, _) => ["  (•̀ᴗ•́)  ", "  /|_|\\  ", "  🦞 ~~  ", " /     \\ "],
            (Self::Mix, 0) => [" \\(•‿•)/ ", "  /|_|\\  ", "  🦞 ♪   ", " /     \\ "],
            (Self::Mix, _) => [" /(•‿•)\\ ", "  /|_|\\  ", "  🦞 ♫   ", " /     \\ "],
            (Self::Fetch, 0) => ["  (•o•)  ", "  /|_|\\  ", "  🦞 →   ", " /     \\ "],
            (Self::Fetch, _) => ["  (•o•)  ", "  /|_|\\  ", "→ 🦞     ", " /     \\ "],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_station_is_user_stream() {
        let live = stations_for(Genre::Live);
        assert_eq!(live[0].url, "https://www.youtube.com/watch?v=tRsQsTMvPNg");
    }

    #[test]
    fn genre_cycles() {
        assert_eq!(Genre::Lofi.next(), Genre::Jazz);
        assert_eq!(Genre::Live.next(), Genre::Lofi);
    }
}
