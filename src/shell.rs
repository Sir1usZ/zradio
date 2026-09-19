#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Music,
    Player,
    Me,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Self::Music => "音乐",
            Self::Player => "播放器",
            Self::Me => "我",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Music => Self::Player,
            Self::Player => Self::Me,
            Self::Me => Self::Music,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Self::Music => Self::Me,
            Self::Player => Self::Music,
            Self::Me => Self::Player,
        }
    }

    pub fn all() -> [Tab; 3] {
        [Self::Music, Self::Player, Self::Me]
    }
}

pub fn format_listen_time(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let rem = secs % 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else if mins > 0 {
        format!("{mins}m {rem}s")
    } else {
        format!("{rem}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_and_e_cycle_three_tabs() {
        let mut tab = Tab::Music;
        tab = tab.next();
        assert_eq!(tab, Tab::Player);
        tab = tab.next();
        assert_eq!(tab, Tab::Me);
        tab = tab.next();
        assert_eq!(tab, Tab::Music);
        tab = tab.prev();
        assert_eq!(tab, Tab::Me);
    }

    #[test]
    fn listen_time_formats_hours() {
        assert_eq!(format_listen_time(45), "45s");
        assert_eq!(format_listen_time(125), "2m 5s");
        assert_eq!(format_listen_time(3661), "1h 1m");
    }
}
