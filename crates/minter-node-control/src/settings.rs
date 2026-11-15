use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Light,
    Full,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FullStart {
    Resume,
    Checkpoint,
    Genesis,
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StorageProfile {
    Full,
    Archive,
    #[default]
    Custom,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub mode: Mode,
    pub data_dir: PathBuf,
    pub api_port: u16,
    pub p2p_port: u16,
    #[serde(default)]
    pub storage_profile: StorageProfile,
    pub keep_versions: u64,
    #[serde(default)]
    pub keep_event_blocks: u64,
    #[serde(default)]
    pub keep_native_blocks: u64,
    #[serde(
        default,
        rename = "archive",
        skip_serializing,
        deserialize_with = "legacy_archive"
    )]
    legacy_archive: Option<bool>,
    pub full_start: FullStart,
    pub genesis: PathBuf,
    pub checkpoint: PathBuf,
    pub checkpoint_sha256: String,
    pub peers: Vec<String>,
    pub light_anchor: PathBuf,
    pub primary: String,
    pub witness: String,
}

fn legacy_archive<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    bool::deserialize(deserializer).map(Some)
}

impl Settings {
    pub fn defaults(data_dir: PathBuf) -> Self {
        Self {
            mode: Mode::Light,
            data_dir,
            api_port: 8841,
            p2p_port: 26656,
            storage_profile: StorageProfile::Full,
            keep_versions: 120,
            keep_event_blocks: 100_000,
            keep_native_blocks: 100_000,
            legacy_archive: None,
            full_start: FullStart::Checkpoint,
            genesis: PathBuf::new(),
            checkpoint: PathBuf::new(),
            checkpoint_sha256: String::new(),
            peers: Vec::new(),
            light_anchor: PathBuf::new(),
            primary: String::new(),
            witness: String::new(),
        }
    }

    pub fn validate(&self, starting: bool) -> Result<()> {
        ensure!(
            self.data_dir.is_absolute(),
            "Выберите абсолютный путь к данным"
        );
        ensure!(
            self.api_port != 0 && self.p2p_port != 0 && self.api_port != self.p2p_port,
            "Порты API и P2P должны различаться и быть больше нуля"
        );
        ensure!(
            self.legacy_archive.is_none(),
            "Загрузите прежние настройки из файла перед сохранением"
        );
        ensure!(
            self.storage_profile != StorageProfile::Custom || self.keep_native_blocks != 1,
            "Нужно сохранять не меньше двух полных блоков; ноль сохраняет все"
        );
        ensure!(self.peers.len() <= 16, "Можно указать до 16 пиров");
        for peer in &self.peers {
            let (id, address) = peer
                .split_once('@')
                .context("Адрес пира должен иметь вид ID@IP:порт")?;
            ensure!(
                id.len() == 40 && id.bytes().all(|c| c.is_ascii_hexdigit()),
                "Неверный ID пира"
            );
            let address: SocketAddr = address.parse().context("Неверный IP или порт пира")?;
            ensure!(
                address.port() != 0 && !address.ip().is_unspecified(),
                "Укажите адрес пира"
            );
        }
        if !starting {
            return Ok(());
        }
        if self.mode == Mode::Light {
            for url in [&self.primary, &self.witness] {
                let url = reqwest::Url::parse(url)
                    .context("Укажите HTTP-адрес основного RPC и свидетеля")?;
                ensure!(
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
                    "RPC должен использовать HTTP или HTTPS"
                );
            }
            ensure!(
                self.primary.trim_end_matches('/') != self.witness.trim_end_matches('/'),
                "Укажите два независимых RPC"
            );
            if !self.data_dir.join("light.redb").is_file() {
                ensure!(
                    self.light_anchor.is_file(),
                    "Выберите JSON доверенной точки"
                );
            }
        } else {
            match self.full_start {
                FullStart::Resume => ensure!(
                    self.data_dir.join("state/CURRENT").is_file(),
                    "В выбранной папке отсутствует база полной ноды"
                ),
                FullStart::Genesis => ensure!(self.genesis.is_file(), "Выберите файл genesis.json"),
                FullStart::Checkpoint => {
                    ensure!(
                        self.checkpoint.join("manifest.json").is_file(),
                        "Выберите папку пакета со снимком"
                    );
                    ensure!(
                        self.checkpoint_sha256.len() == 64
                            && self
                                .checkpoint_sha256
                                .bytes()
                                .all(|c| c.is_ascii_hexdigit()),
                        "Укажите доверенный SHA-256 манифеста из 64 символов"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn load(path: &Path, data_dir: PathBuf) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::defaults(data_dir));
        }
        let mut bytes = Vec::new();
        fs::File::open(path)
            .context("Не удалось прочитать настройки")?
            .take(64 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 64 * 1024, "Файл настроек слишком велик");
        // Deserialize the struct first: unlike Value this rejects duplicate fields.
        let mut settings: Self =
            serde_json::from_slice(&bytes).context("Не удалось разобрать настройки")?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        if value.get("archive").is_some() {
            let archive = settings
                .legacy_archive
                .take()
                .context("Поле archive должно быть boolean")?;
            ensure!(
                value.get("storageProfile").is_none()
                    && value.get("keepEventBlocks").is_none()
                    && value.get("keepNativeBlocks").is_none(),
                "Прежнее поле archive нельзя совмещать с новым профилем хранения"
            );
            settings.storage_profile = if archive {
                StorageProfile::Archive
            } else {
                StorageProfile::Custom
            };
            settings.keep_event_blocks = 0;
            settings.keep_native_blocks = 0;
        } else {
            ensure!(
                value.get("storageProfile").is_some(),
                "Укажите профиль хранения"
            );
        }
        settings.validate(false)?;
        Ok(settings)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.write(path, true)
    }

    /// Create an initial configuration without replacing an existing file.
    pub fn save_new(&self, path: &Path) -> Result<()> {
        self.write(path, false)
    }

    fn write(&self, path: &Path, replace: bool) -> Result<()> {
        self.validate(false)?;
        let parent = path.parent().context("Каталог настроек отсутствует")?;
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        file.as_file().sync_all()?;
        if replace {
            file.persist(path)
        } else {
            file.persist_noclobber(path)
        }
        .context("Не удалось сохранить настройки")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_settings_without_introducing_history_pruning() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        for (archive, expected) in [(false, "custom"), (true, "archive")] {
            let mut old = serde_json::to_value(Settings::defaults(dir.path().join("node")))?;
            let object = old.as_object_mut().unwrap();
            object.remove("storageProfile");
            object.remove("keepEventBlocks");
            object.remove("keepNativeBlocks");
            object.insert("archive".into(), archive.into());
            object.insert("keepVersions".into(), 17.into());
            fs::write(&path, serde_json::to_vec(&old)?)?;
            let restored = serde_json::to_value(Settings::load(&path, PathBuf::new())?)?;
            assert_eq!(restored["storageProfile"], expected);
            assert_eq!(restored["keepVersions"], 17);
            assert_eq!(restored["keepEventBlocks"], 0);
            assert_eq!(restored["keepNativeBlocks"], 0);
            assert!(restored.get("archive").is_none());
        }
        Ok(())
    }

    #[test]
    fn settings_roundtrip_preserves_unicode_paths_and_rejects_invalid_peers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        let mut settings = Settings::defaults(dir.path().join("Нода Minter"));
        settings.save(&path)?;
        let restored = Settings::load(&path, PathBuf::new())?;
        assert_eq!(restored.data_dir, settings.data_dir);
        settings.peers = vec!["bad@127.0.0.1:26656".into()];
        assert!(settings.save(&path).is_err());
        assert!(Settings::load(&path, PathBuf::new())?.peers.is_empty());
        let original = fs::read(&path)?;
        settings.peers.clear();
        assert!(settings.save_new(&path).is_err());
        assert_eq!(fs::read(&path)?, original);
        Ok(())
    }

    #[test]
    fn rejects_ambiguous_duplicate_and_invalid_retention_settings() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("settings.json");
        let mut settings = Settings::defaults(dir.path().join("node"));
        let mut value = serde_json::to_value(&settings)?;
        value["archive"] = true.into();
        fs::write(&path, serde_json::to_vec(&value)?)?;
        assert!(Settings::load(&path, PathBuf::new()).is_err());
        value["archive"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<Settings>(value).is_err());
        let mut json = serde_json::to_string(&settings)?;
        json.insert_str(1, "\"storageProfile\":\"archive\",");
        fs::write(&path, json)?;
        assert!(Settings::load(&path, PathBuf::new()).is_err());
        settings.storage_profile = StorageProfile::Custom;
        settings.keep_native_blocks = 1;
        assert!(settings.validate(false).is_err());
        settings.keep_native_blocks = 0;
        settings.keep_versions = 0;
        settings.validate(false)?;
        Ok(())
    }
}
