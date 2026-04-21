use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const STARTING_TOKENS: u64 = 404;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameStats {
    pub played: u64,
    pub won: u64,
    pub lost: u64,
    pub biggest_win: u64,
    pub biggest_loss: u64,
}

impl Default for GameStats {
    fn default() -> Self {
        Self { played: 0, won: 0, lost: 0, biggest_win: 0, biggest_loss: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlackjackStats {
    #[serde(flatten)]
    pub base: GameStats,
    pub blackjacks: u64,
    pub pushes: u64,
}

impl Default for BlackjackStats {
    fn default() -> Self {
        Self { base: GameStats::default(), blackjacks: 0, pushes: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlotsStats {
    #[serde(flatten)]
    pub base: GameStats,
    pub jackpots: u64,
}

impl Default for SlotsStats {
    fn default() -> Self {
        Self { base: GameStats::default(), jackpots: 0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveData {
    pub version: u32,
    pub tokens: u64,
    pub total_earned: u64,
    pub total_lost: u64,
    pub games_played: u64,
    pub resets: u64,
    pub blackjack: BlackjackStats,
    pub slots: SlotsStats,
    pub roulette: GameStats,
}

impl Default for SaveData {
    fn default() -> Self {
        Self {
            version: 1,
            tokens: STARTING_TOKENS,
            total_earned: 0,
            total_lost: 0,
            games_played: 0,
            resets: 0,
            blackjack: BlackjackStats::default(),
            slots: SlotsStats::default(),
            roulette: GameStats::default(),
        }
    }
}

fn save_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gamblers-den")
        .join("save.json")
}

/// Load save data from disk. Returns default (404 tokens) if missing or corrupt.
pub fn load() -> SaveData {
    let path = save_path();
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
        Err(_) => SaveData::default(),
    }
}

/// Save data to disk. Atomic write (tmp + rename).
pub fn save(data: &SaveData) -> std::io::Result<()> {
    let path = save_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let json = serde_json::to_string_pretty(data)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;

    Ok(())
}

/// Reset tokens to 404 and increment reset counter.
pub fn reset(data: &mut SaveData) {
    data.tokens = STARTING_TOKENS;
    data.resets += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn default_save_has_404_tokens() {
        let save = SaveData::default();
        assert_eq!(save.tokens, 404);
        assert_eq!(save.version, 1);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test-save.json");

        let mut data = SaveData::default();
        data.tokens = 999;
        data.blackjack.blackjacks = 5;
        data.resets = 2;

        let json = serde_json::to_string_pretty(&data).unwrap();
        fs::write(&path, &json).unwrap();

        let loaded: SaveData = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.tokens, 999);
        assert_eq!(loaded.blackjack.blackjacks, 5);
        assert_eq!(loaded.resets, 2);
    }

    #[test]
    fn corrupt_file_returns_default() {
        let data: SaveData = serde_json::from_str("not json").unwrap_or_default();
        assert_eq!(data.tokens, 404);
    }

    #[test]
    fn reset_restores_404_and_increments_counter() {
        let mut data = SaveData::default();
        data.tokens = 0;
        reset(&mut data);
        assert_eq!(data.tokens, 404);
        assert_eq!(data.resets, 1);
    }
}
