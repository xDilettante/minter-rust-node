# Minter Rust Node

Экспериментальная реализация ноды Minter на Rust для Windows 11 x64 и Ubuntu 24.04 x86_64.

[![CI](https://github.com/xDilettante/minter-rust-node/actions/workflows/ci.yml/badge.svg)](https://github.com/xDilettante/minter-rust-node/actions/workflows/ci.yml)

Статус: **alpha**. Поддерживаемый сценарий первого выпуска — обычная полная нода с запуском из доверенного снимка, синхронизацией и локальным API. Полная историческая и валидаторская приёмка продолжается; [подтверждённые проверки и ограничения](docs/validation.md) перечислены отдельно.

## Возможности

- `minterd`: исполнение Minter, native Tendermint 0.34 P2P, State Sync, Minter v2 HTTP/gRPC API и переходный ABCI-режим.
- RocksDB для состояния и истории. Профиль full сохраняет 120 версий IAVL и 100 000 блоков/пакетов событий; archive сохраняет дальнейшую историю.
- `minterctl`: управление процессом, проверка состояния и IAVL-доказательств, работа с checkpoint и лёгким клиентом.
- Лёгкий клиент на redb: проверенные заголовки, независимые RPC и поддерживаемые доказательства. Источники и доверенная точка задаются явно.
- GUI на Tauri 2, Svelte и TypeScript: настройка, запуск, остановка, состояние и журнал.
- Все 37 активных типов закреплённого Go имеют обработчики. Исторические варианты продолжают сверяться с Go.

## Сборка CLI

Нужны Rust 1.98.0, C++ toolchain и libclang. Версия Rust закреплена в `rust-toolchain.toml`.

Ubuntu 24.04:

```sh
sudo apt update
sudo apt install build-essential libclang-dev llvm-dev pkg-config libssl-dev
cargo build --workspace --release --locked
```

Windows: установите Visual Studio 2022 Build Tools с C++ и Windows SDK, а также LLVM с `libclang.dll`. При нестандартной установке задайте `LIBCLANG_PATH`. Подготовка окружения также описана в `.github/workflows/ci.yml`.

```powershell
cargo build --workspace --release --locked
.\target\release\minterd.exe --help
.\target\release\minterctl.exe --help
```

## Полная нода из снимка

Пакет содержит `manifest.json`, `snapshot.zlib`, `state.pb`, `blocks.pb` и при необходимости `history.pb`. Получите пакет и SHA-256 manifest из доверенного источника. Пиры задаются как `NODE_ID@HOST:PORT`.

```sh
./target/release/minterd native-sync \
  --db ./node/state \
  --checkpoint ./checkpoint \
  --checkpoint-sha256 TRUSTED_MANIFEST_SHA256 \
  --peer NODE_ID@HOST:26656 --follow \
  --storage-profile full \
  --p2p-listen 127.0.0.1:26656 \
  --api-listen 127.0.0.1:8841
```

Замените два значения-заполнителя своими проверенными данными. Для Windows используйте `.exe`; команду можно записать одной строкой. После успешного импорта продолжайте с тем же `--db`, пиром и сетевыми настройками. Настройки хранения сохраняются в базе. Ctrl-C завершает процесс штатно.

Запуск из снимка не требует загрузки всей предыдущей истории. Архивный профиль сохраняет данные с точки начала запуска; недостающая старая история сама не появляется. Автоматическая публичная доставка доверенных пакетов ещё готовится.

Для foreground-управления и GUI используется общий JSON настроек:

```sh
./target/release/minterctl node init --config ./settings.json --data-dir ./node --mode full
./target/release/minterctl node settings --config ./settings.json
./target/release/minterctl node run --config ./settings.json
./target/release/minterctl node status --config ./settings.json
./target/release/minterctl node stop --config ./settings.json
```

Укажите в настройках checkpoint, доверенный SHA и пиры до первого запуска. Папка данных и конфигурация защищены от одновременного управления.

## GUI

Нужен Node.js 24.19.0. На Linux также установите [системные зависимости Tauri](https://v2.tauri.app/start/prerequisites/), включая WebKitGTK 4.1 и Ayatana AppIndicator. На Windows требуется WebView2.

```sh
cd desktop
npm ci
npm run format:check
npm run check
node tools/prepare-binaries.mjs ../target/release
npm run tauri -- build --no-bundle -- --locked
```

GUI предлагает полную и лёгкую ноду, каталог данных и политику хранения. Настройте источники доверия в форме. [Подробности интерфейса и упаковки](desktop/README.md).

## Проверка разработки

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --release --locked -- -D warnings
cargo test --workspace --release --locked
```

Большие mainnet-корпуса не входят в исходники; требующие их тесты запускаются отдельно по инструкциям. Приложение и блокчейн-данные хранятся раздельно. Для чтения работающей ноды используйте её API; команды прямого открытия RocksDB выполняйте после остановки процесса.

[Техническая справка](TECHNICAL.md) содержит расширенные команды и протокольные детали. Ранние названия лабораторных ZIP в ней относятся к истории разработки и не означают наличие готового публичного пакета.

## Статус alpha и вклад

[Issues](https://github.com/xDilettante/minter-rust-node/issues) принимают воспроизводимые ошибки с версией ОС, commit и журналом без ключей/секретов. Проверки чистых установок, полный исторический replay, непрерывный mainnet-пилот и допуск валидатора остаются в плане.

Собственный код распространяется по [MIT](LICENSE). [Лицензии и атрибуция компонентов](THIRD_PARTY_NOTICES.md) сохраняются отдельно. Это самостоятельная экспериментальная реализация; статус alpha не является заявлением официального выпуска Minter.
