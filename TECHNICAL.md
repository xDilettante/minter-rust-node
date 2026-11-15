# Перенос ядра Minter на Rust

`minterctl` читает Go-снимок формата 1, заново вычисляет корень IAVL и открывает подтверждённое состояние в RocksDB. Он также применяет журнал мутаций IAVL и читает значения из сохранённых версий дерева. Узлы и корни используют раскладку ключей Go IAVL `n<hash>` и `r<height>`. Записи AppDB сохраняются побайтно на высоте импорта. `minterd` обслуживает ABCI 0.34. Rust исполняет переводы, выпуск и перевыпуск монет и токенов, обмены Bancor и SwapV2, ордера, операции с ликвидностью и стейкинг. Поддерживаются проверка весов мультиподписи, выплаты v330, обновление цены награды и отключение валидаторов за пропуски.

Диспетчер содержит все 37 активных типов закреплённого Go. Перевыпуск сохраняет старые ID, балансы, историю версий и владельца символа; Go-совместимость проверяется корпусом, включая uint16-границу и gas той же монетой. Наличие всех обработчиков не заменяет проверку исторических правил. [Решение и проверки перевыпуска](docs/decisions/0009-coin-recreation.md).

Для трёх настоящих `RecreateToken` добавлены [векторы mainnet](crates/minter-app/tests/fixtures/mainnet-recreation-history.json) и регрессионная проверка подписи, полей и gas. `tools/verify_recreation_history.go` проверяет 12 исходных блоков и шесть коммитов по сохранённым наборам валидаторов. Полный путь доверия от genesis и исполнение исторического состояния требуют отдельной проверки; соответствующие флаги корпуса остаются `false`.

[Обновление существующей нативной базы](docs/operations/native-node-upgrade.md) описывает однократное восстановление индекса из исходного доверенного checkpoint. Этот путь проверен на рабочих mainnet-базах Windows и Linux с сохранением высоты, AppHash и окна IAVL.

Go-эталон `tools/replay_mainnet_genesis.go` поддерживает префикс `--checkpoint PACKAGE MANIFEST_SHA256` для продолжения из ранее проверенного нативного пакета. Он проверяет снимок, состояние консенсуса и якорные коммиты, затем использует тот же цикл исполнения и проверки подписанных результатов. [Контракт и запуск](docs/specs/checkpoint-replay.md).

## Настольное управление

Приложение в `desktop` использует Tauri 2, Svelte 5 и TypeScript. Оно запускает полную ноду и лёгкий клиент, читает статус из локального API и redb, сохраняет настройки и показывает журнал. Дочерний `minterd` получает `--shutdown-on-stdin-close` и завершает работу при закрытии канала родителя. [Сборка и проверка GUI](desktop/README.md).

## Сборка и проверка

Проверено с Rust 1.98.0. Для сборки RocksDB нужны C++ toolchain и libclang. Установить MSVC Build Tools и Windows SDK на Windows либо C++ toolchain и libclang на Linux. При нестандартной установке указать `LIBCLANG_PATH`.

В архиве `minter-rust-assets.zip` есть `minterctl`, `minterd` и `minter-go-abci` x86-64 для Windows 11 и Ubuntu 24.04, компактный корпус и журналы. Перед запуском Linux-бинарников из ZIP выполните `chmod +x bin/linux-x64/minterctl bin/linux-x64/minterd bin/linux-x64/minter-go-abci`.

```text
cargo build --workspace --release --locked
cargo test --workspace --release --locked
```

Команды принимают манифест проверенной контрольной точки. Передавайте хеши из доверенного манифеста с аутентифицированным заголовком и хешем скачанного снимка. У формата Minter `AppHash` подтверждает дерево IAVL. Хеш всего файла также связывает записи AppDB с доверенной копией снимка. [Формат Go-снимка](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/appdb/snapshot.go), [импорт IAVL 0.17.3](https://github.com/cosmos/iavl/blob/v0.17.3/import.go).

```text
minterctl checkpoint import --db ./state --snapshot ./go_checkpoint/snapshot.zlib --manifest ./go_checkpoint/manifest.json
minterctl checkpoint status --db ./state
minterctl checkpoint get --db ./state --key 616263
```

В выводе `get` значение кодируется в hex. Отсутствующий ключ имеет значение `null`, пустой лист — пустую строку.

Импорт сверяет SHA-256 до открытия базы и по фактически разобранным байтам. Узлы пишутся пакетами с синхронным WAL. Корень, AppDB и отметка готовности публикуются одним финальным пакетом. Если процесс прерван до публикации, откройте тот же каталог повторным запуском команды импорта с тем же доверенным манифестом. Импорт повторно пройдёт снимок и запишет узлы по тем же хешам.

## Применение мутаций IAVL

Возьмите `abci-iavl.jsonl.gz` и `blocks.csv.gz` из проверенного корпуса `minter-mainnet-5`. Индекс получен при сверке подписей и блоков Tendermint. Команда `iavl replay` сверяет хеш блока на высоте `H` и корень состояния с полем `AppHash` заголовка `H+1`. Для другого корпуса заранее проверьте подписи и цепочку заголовков.

```text
minterctl iavl replay --db ./state --trace ./abci-iavl.jsonl.gz --headers ./blocks.csv.gz --through 40661287
minterctl iavl status --db ./state
minterctl iavl verify --db ./state --headers ./blocks.csv.gz
minterctl iavl get --db ./state --height 40559288 --key 64
```

В пределах блока Rust применяет операции в порядке из Go, повторно использует хеши неизменённых поддеревьев и сохраняет только новые узлы итогового дерева. Запись новых узлов, корня версии и высоты выполняется одним пакетом с WAL и `sync=true` после совпадения AppHash. При расхождении команда показывает первую несовпавшую высоту. Повторный запуск сверяет сохранённый корень и продолжает со следующего блока. `iavl verify` после перезапуска проверяет каждый сохранённый корень. `checkpoint get` читает высоту исходного снимка, а `iavl get` по умолчанию читает последнюю сохранённую версию. Журнал мутаций хранит все корни от высоты снимка. [Мутации Go IAVL 0.17.3](https://github.com/cosmos/iavl/blob/v0.17.3/mutable_tree.go).

Для исполнения блоков используйте отдельный каталог, созданный импортом снимка. После запуска исполнения `iavl replay` отклоняет запись в эту базу, поскольку журнал мутаций не содержит метаданных для исполнения следующего блока.

## Окно истории состояния

При запуске приложения укажите число сохраняемых версий IAVL вместе с текущей.

```text
minterd --db ./state --keep-iavl-versions 120
```

Настройка сохраняется в базе и действует при последующих запусках. Для нового каталога без настройки действует архивный режим. Значение `0` сохраняет все последующие версии. Чтобы восстановить уже удалённые версии, понадобится повторное исполнение из более раннего состояния.

При остановленном `minterd` можно посмотреть окно, изменить его или запустить компакцию RocksDB для освобождения места.

```text
minterctl iavl history --db ./state
minterctl iavl retain --db ./state --versions 120 --compact
minterctl iavl verify --db ./state --headers ./blocks.csv.gz
```

`iavl verify` использует манифест Go-снимка как начальную опору для сверки с заголовками. `iavl history` также работает с базой, созданной от genesis.

При первом сокращении существующей базы RocksDB строится индекс по сохранённым версиям. Индексация сохраняет ход работы в WAL. При прерывании повторите команду с теми же параметрами. Узлы, достижимые из сохранённых корней, остаются доступны. Чтения работающей ноды выполняются через ABCI, а команды, которые открывают RocksDB напрямую, запускаются при остановленном приложении. Лимит применяется к IAVL. Политика хранения блоков, результатов и индексов Tendermint задаётся отдельно. [Синхронная запись RocksDB](https://github.com/facebook/rocksdb/wiki/Basic-Operations), [снимки RocksDB](https://github.com/facebook/rocksdb/wiki/Snapshot).

## Окно истории Tendermint

Для профиля с очисткой соберите переходный движок из закреплённого Tendermint 0.34.19. Команда проверяет Go-модуль, применяет патч и собирает бинарник x64. В Windows используйте имя файла с `.exe`.

```text
python -B tools/build_tendermint_companion.py ./tendermint-minter --test
tendermint-minter node --home ./tm --keep_blocks 100000 --keep_index_blocks 100000
```

Эти настройки также можно сохранить в начале `config.toml`. Блоки, результаты ABCI и индексы движка хранятся в GoLevelDB. Для окна индексов выберите `[tx_index] indexer = "kv"`. Значение `0` сохраняет дальнейшую историю соответствующего хранилища. Для архива укажите также `--keep-iavl-versions 0` в `minterd`.

`keep_blocks` задаёт целевое окно блоков и результатов ABCI. Движок расширяет его на высоты, нужные для проверки evidence и настроенного поиска предыдущих подписей. Очистка идёт во время block sync и консенсуса. Старый архив обрабатывается порциями. Фактическая нижняя граница видна в `sync_info.earliest_block_height` ответа Tendermint `/status`. [Проверка evidence в Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/evidence/verify.go).

`keep_index_blocks` задаёт отдельное окно индекса транзакций и событий блоков. Транзакцию по хешу можно читать, пока её индекс доступен. Для доказательства включения требуется также сохранённый блок. Удаление записей и освобождение места на файловой системе происходят в разные моменты из-за журналов и компакции LevelDB. Для восстановления уже удалённой истории нужен архив или повторная синхронизация. [Индекс Tendermint 0.34](https://docs.tendermint.com/v0.34/app-dev/indexing-transactions.html).

## Подписант Rust для Tendermint 0.34

В смешанной тестовой сети Rust подписывает предложения блоков, prevote и precommit. Go Tendermint ведёт раунды консенсуса. Для TCP-соединения соберите переходный движок с `--signer`. Он использует свой постоянный `node_key.json` для Secret Connection и проверяет отдельный транспортный ID подписанта. Подписант сверяет ID движка с `--peer`.

```text
python -B tools/build_tendermint_companion.py ./tendermint-minter --signer
tendermint-minter show-node-id --home ./tm
minterd validator-signer transport-id --transport-key ./signer/transport.json
```

Остановите Go-валидатор и перенесите пару `priv_validator_key.json` и `priv_validator_state.json` в закрытый каталог подписанта с правами владельца. Для импорта нужен актуальный Go-файл состояния этой пары. Пустое состояние предназначено для нового ключа. Импорт проверяет последнюю подпись, если она есть, и создаёт новый каталог журнала.

```text
minterd validator-signer import --chain-id <chain-id> --key ./signer/priv_validator_key.json --go-state ./signer/priv_validator_state.json --journal ./signer/journal
```

В начале `./tm/config/config.toml` укажите локальный адрес и ID, выведенный командой `transport-id`.

```toml
priv_validator_laddr = "tcp://127.0.0.1:26659"
priv_validator_signer_id = "<signer-transport-id>"
```

Запустите Rust-подписант, затем приложение Minter и переходный движок с этим конфигом. В `--peer` передайте ID из `show-node-id`.

```text
minterd validator-signer run --chain-id <chain-id> --key ./signer/priv_validator_key.json --journal ./signer/journal --transport-key ./signer/transport.json --peer <go-node-id>@127.0.0.1:26659
```

Подписант фиксирует журнал до передачи подписи. Для каждого ключа используйте единственный актуальный журнал и один активный подписант. Версия журнала в резервной копии должна включать все выданные подписи. После начала подписи в Rust файл состояния исходного Go-валидатора устаревает. Для дальнейших перезапусков используйте каталог `journal` и конфигурацию внешнего подписанта. [Повтор подписи в Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/privval/file.go).

## Сверка с Go

Малая фикстура в `tests/fixtures` сформирована Go IAVL из Minter на ревизии `a3aac90a4f7995fccaf3cd4e4c34c89ff0c7544c`. Её можно пересоздать командой из корня Go-модуля.

```text
go run -mod=readonly /path/to/minter-rust/tools/make_checkpoint_fixture.go /path/to/minter-rust/tests/fixtures
```

Для сверки реального снимка скрипт `tools/verify_mainnet_checkpoint.py` принимает бинарник, файл снимка, манифест, JSON с чтениями Go, проверенный `blocks.csv.gz` и новый выходной каталог. Скрипт проверяет заголовок на высоте `H+1`, прерывает импорт после появления пакета IAVL в WAL, проверяет отказ открытия незавершённой базы и повторяет импорт в тот же каталог. Затем сверяет запросы по ключам и сохраняет `result.json`.

JSON с Go-значениями можно сформировать из корня того же Go-модуля. После высоты перечисляются ключи в hex.

```text
go run -mod=readonly /path/to/minter-rust/tools/read_go_checkpoint.go /path/to/snapshot.zlib 40559287 <key-hex> [...]
```

Чтобы получить ответ Go после применения мутаций, передайте файл трассы и целевую высоту до аргументов снимка.

```text
go run -mod=readonly /path/to/minter-rust/tools/read_go_checkpoint.go -trace /path/to/abci-iavl.jsonl.gz -through 40560269 /path/to/snapshot.zlib 40559287 64
```

Скрипт `tools/verify_mainnet_iavl.py` сверяет SHA-256 файлов с доверенным манифестом корпуса, импортирует снимок, подменяет одну мутацию и проверяет отказ записи. Затем он принудительно завершает процесс после фиксации части версий, проверяет сохранённые корни и повторяет запуск до конца корпуса. В `result.json` записываются восстановленная высота, итоговый AppHash, число операций, число сверенных версий и чтения на трёх высотах.

```text
python tools/verify_mainnet_iavl.py <minterctl> <corpus-manifest.json> <snapshot.zlib> <checkpoint-manifest.json> <abci-iavl.jsonl.gz> <blocks.csv.gz> <new-result-directory>
```

## Декодирование транзакций

Пакет `minter-protocol` разбирает RLP-конверт, вычисляет Keccak-256 для подписи и восстанавливает адрес из каждой подписи secp256k1. Для мультиподписи конверт содержит адрес аккаунта и подписи. Проверка прав подписантов, порога и весов требует состояния аккаунта и относится к этапу исполнения транзакций. [Хеш и восстановление подписи в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/transaction.go), [восстановление secp256k1 в k256](https://github.com/RustCrypto/elliptic-curves/tree/master/k256).

Для сверки возьмите эталонные ABCI-записи из проверенного корпуса. Первая команда запускается из корня Go-эталона Minter на ревизии `a3aac90a4f7995fccaf3cd4e4c34c89ff0c7544c`, вторая — из корня Rust workspace. Сравниваются поля конверта, хеш подписи, адрес одиночной подписи, адрес мультиподписи и адреса её подписантов.

```text
go run -mod=readonly /path/to/minter-rust/tools/read_go_transaction_trace.go /path/to/abci-iavl.jsonl.gz /path/to/go-transactions.jsonl
cargo run -p minter-protocol --release --locked --example compare_go -- /path/to/go-transactions.jsonl
```

## Локальный перевод через Tendermint

`minterd --db ./state --abci-listen 127.0.0.1:26658` открывает локальный socket ABCI 0.34. Для `EndBlock` передаётся частичное обновление `BlockParams` через `tendermint-proto` и `prost`. Tokio обслуживает соединения, выделенный поток последовательно исполняет запросы. Один синхронный пакет RocksDB фиксирует IAVL и метаданные исполнения. [EndBlock в Go Minter](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/minter/blockchain.go), [протокол сокета Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/abci/types/messages.go).

Для воспроизводимого прогона нужны Tendermint 0.34.19 и Python 3.11 или новее. `minter-go-abci` формирует genesis, локальный ключ валидатора и ответы Go в указанном новом каталоге. Скрипт запускает валидатор с Rust-приложением и узел-наблюдатель с Go-приложением. Для внешнего Go-сокета `CheckTx` вызывает оригинальный Go-исполнитель с минимальной ценой газа 1, как при пустом мемпуле. Скрипт сверяет результаты транзакций и подписанные заголовки, перезапускает Rust-узел и повторно проверяет исторические балансы. Узлы передают обычный перевод, `Multisend` с мультиподписью и `CreateToken` через P2P. Для выпуска сверяются начальный баланс и комиссия на нулевом адресе.

```text
minter-go-abci ./go-fixture
python tools/verify_send_devnet.py <minterd> <minter-go-abci> <tendermint> ./go-fixture ./new-devnet-results
```

На Windows используйте имена бинарников с расширением `.exe`. Собрать эталон можно из корня Go-модуля на ревизии `a3aac90a4f7995fccaf3cd4e4c34c89ff0c7544c`.

```text
go build -mod=readonly -o minter-go-abci /path/to/minter-rust/tools/make_abci_fixture.go
```

В локальной сети доступен запрос ABCI `/balance`. В поле `data` передайте 20 байт адреса. Для другой монеты допишите четырёхбайтовый coin ID в little-endian. Поле `height` задаёт сохранённую высоту, 0 выбирает последнюю. Ответ в `value` содержит JSON с балансом в минимальных единицах монеты и nonce. Для проверяемого баланса используйте лёгкий клиент и ABCI `/iavl/key`.

## Запуск от genesis minter-mainnet-5

Поместите официальный `genesis.json` из релиза v3.0.3 в `config/genesis.json` каталога Tendermint 0.34.19. Укажите адрес ABCI `minterd` в параметре Tendermint `proxy_app`. При запуске с пустой RocksDB Tendermint передаст исходный `app_state` размером 127 580 513 байт. Rust сформирует начальное состояние на высоте 10 196 999 и вернёт 44 обновления валидаторов. Повторный запуск до первого блока принимает тот же genesis из базы Tendermint. [Официальный genesis](https://github.com/MinterTeam/minter-go-node/releases/tag/v3.0.3).

При остановленном приложении проверьте начальный корень командой `minterctl iavl history --db ./state`. Ожидаемый AppHash равен `c88e583eecef3cb49965bb61fe35450c430cf6ae968fc09850d4ca670503620f`. При прерывании импорта запустите те же процессы с прежними каталогами.

Для локальной проверки нужны Python 3.11 или новее и сборки `minterd`, `minterctl` и Tendermint для вашей ОС. Сформируйте эталон из корня Go-модуля на ревизии `a3aac90a4f7995fccaf3cd4e4c34c89ff0c7544c` с `iavl.MutationHook`. Затем запустите проверку из корня Rust workspace. Скрипт дважды выполняет handshake с Tendermint, проверяет восстановление и прогоняет два синтетических блока на копии начальной базы.

```text
go run -mod=readonly /path/to/minter-rust/tools/make_official_genesis_reference.go /path/to/genesis.json /path/to/go-genesis-reference
python tools/verify_official_genesis.py --genesis <genesis.json> --go-reference <go-genesis-reference> --minterd <minterd> --minterctl <minterctl> --tendermint <tendermint> --output <new-results>
```

### Исполнение подписанных блоков начала эпохи

`replay_mainnet_genesis.go` берёт исходный genesis и сырые сообщения Tendermint `BlockResponse`, сохранённые последовательными частями `<first>-<last>.pb`. Он сверяет начальный набор валидаторов с Go `InitChain`, проверяет коммиты, исполняет блоки и сравнивает корни с подписанными заголовками. После последнего исполняемого блока нужны два следующих, чтобы проверить подписи заголовка с его AppHash.

```text
go run -mod=readonly /path/to/minter-rust/tools/replay_mainnet_genesis.go <genesis.json> <block-shards> <go-results> 10217520 10215251 10216999
python -B tools/verify_early_checkpoint.py tools <minterd> <minterctl> <go-results> <go-results>/10216999 <rust-results> --through 10217520 --vote-target 10217000
```

Сверка от genesis охватывает 20 521 последовательный блок, включая активацию v310 и первую выплату по его правилам. Исполняются исторические `EditMultisig`, `CreateMultisig`, `RedeemCheck` и `EditCandidate`. Для чека проверяются подпись, право предъявления и одноразовое использование. Сумма чека и комиссия списываются с выпустившего его аккаунта, а отметка погашения сохраняется вместе с IAVL. [Go RedeemCheck](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/redeem_check.go), [Go EditCandidate](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/edit_candidate.go).

Укажите каталог остановленной архивной базы с импортированным genesis. Скрипт копирует базу, сверяет каждый ответ ABCI и AppHash и дважды прерывает процесс. Можно начать с ранее сверенной архивной базы. В этом случае скрипт сверяет её последний корень с Go и продолжает со следующего блока.

```text
python -B tools/verify_signed_genesis_block.py tools <minterd> <minterctl> <archive-db> <go-results> <new-results> --through 10217520 --interrupt-before 10198259 --interrupt-after-commit 10217000
cargo test --release --locked -p minter-app --test update_votes matches_go_for_historical_transactions
```

`EditCoinOwner` меняет владельца тикера с сохранением ID и эмиссии. `CreateSwapPool` фиксирует резервы, идентификатор пула и выпуск токена ликвидности в одном `Commit`. Go-фикстура сверяет права нового владельца, два пула в одном блоке, минимальный выпуск, отказ повторной пары и выделение следующего ID после перезапуска. [Go EditCoinOwner](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/edit_coin_owner.go), [Go CreateSwapPool](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/create_swap_pool.go).

## Голосование за обновление сети

`VoteUpdate` проверяет владельца кандидата и уникальность голоса на выбранную высоту. `EndBlock` учитывает стейк валидаторов, подписавших предыдущий блок, и обрабатывает предложения по правилам Go. Выбранная версия и `UpdateNetworkEvent` записываются в одной фиксации с корнем IAVL. Если следующему блоку нужна неизвестная версия, `minterd` отклоняет `BeginBlock` и сохраняет последний подтверждённый корень. [Транзакция Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/update_network_v230.go), [подсчёт голосов Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/minter/minter.go#L387).

Фикстура Go включает голоса из genesis и снимка, конкурирующие предложения, отказы, смену версий и остановку на неизвестной версии. Тест Rust сравнивает ответы ABCI, корни, порядок событий и историю версий. Скрипт повторяет восстановление через сокет `minterd` после принудительной остановки до и после `Commit`.

```text
cargo test --release --locked -p minter-app --test update_votes
python tools/verify_update_votes.py <minterd> <minterctl> crates/minter-app/tests/fixtures/go-update-votes-v034.json <new-results> --oracle <minter-api-oracle>
```

Передайте `--oracle`, чтобы сверить историю версий с Go по HTTP и gRPC. Для проверки только Rust этот параметр можно опустить.

## Голосование за остановку на высоте

`SetHaltBlock` проверяет владельца кандидата, высоту и отсутствие дубликата. Перед исполнением блока учитываются голоса с зафиксированной высоты и стейк подписавших предыдущий блок валидаторов. При кворуме `minterd` возвращает ошибку ABCI для `BeginBlock`, а последний подтверждённый корень остаётся доступен при новом подключении. При недостаточном кворуме существующая запись удаляется вместе с фиксацией блока. [Транзакция Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/set_halt_block.go), [проверка кворума Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/minter/minter.go#L292).

Из корня закреплённого Go-модуля создайте эталон в новом каталоге. Затем запустите проверку через сокет из Rust workspace.

```text
go run -mod=readonly /path/to/minter-rust/tools/make_update_vote_fixture.go <new-fixture-dir> --halt
python -B tools/verify_halt_votes.py <minterd> <minterctl> crates/minter-app/tests/fixtures/go-halt-votes-v034.json <new-results>
```

## Голосование за комиссии

`VoteCommission` принимает предложение владельца кандидата. Для цены в токене требуется пул с BIP. На высоте голосования `EndBlock` выбирает предложение по стейку подписавших валидаторов, а `Commit` сохраняет таблицу комиссий, удаление голосов и `UpdateCommissionsEvent` вместе с AppHash. Транзакции следующего блока используют новую таблицу. [Транзакция Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/vote_commission_v3.go), [подсчёт и применение в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/minter/blockchain.go#L489).

Фикстура Go проверяет смену цены с BIP на токен, плату за успешные и отклонённые транзакции, порядок событий при одновременном обновлении версии и восстановление до и после `Commit`. Скрипт также сверяет HTTP и gRPC для чтения голосов, событий и таблиц комиссий.

```text
go run -mod=readonly /path/to/minter-rust/tools/make_update_vote_fixture.go <new-fixture-dir> --commission
cargo test --release --locked -p minter-app --test update_votes
python -B tools/verify_governance_api.py <minterd> <minterctl> <minter-api-oracle> crates/minter-app/tests/fixtures <new-results>
```

## Minter v2 API

Укажите адрес HTTP, gRPC или обоих интерфейсов при запуске. Параметр `--tendermint-rpc` задаёт RPC того же Tendermint 0.34, который подключён к ABCI `minterd`.

```text
minterd --db ./state --api-listen 127.0.0.1:8841 --grpc-listen 127.0.0.1:8842 --tendermint-rpc 127.0.0.1:26657
```

| gRPC `api_pb.ApiService` | HTTP GET |
| --- | --- |
| `Subscribe` | `/v2/subscribe?query=…` (HTTP или WebSocket) |
| `Status` | `/v2/status` |
| `VersionNetwork` | `/v2/version_network` |
| `PriceCommission` | `/v2/price_commissions?height=…` |
| `CommissionVotes` | `/v2/commission_votes/{target_version}?height=…` |
| `UpdateVotes` | `/v2/update_votes/{target_version}?height=…` |
| `Halts` | `/v2/halts/{height}` |
| `Address` | `/v2/address/{address}` |
| `CoinInfo` | `/v2/coin_info/{symbol}` |
| `CoinInfoById` | `/v2/coin_info_by_id/{id}` |
| `Block` | `/v2/block/{height}` |
| `Events` | `/v2/events/{height}?search=…` |
| `Transaction` | `/v2/transaction/{hash}` |
| `Transactions` | `/v2/transactions?query=…&page=1&per_page=30` |
| `SendTransaction` | `/v2/send_transaction/{tx}` |

`VersionNetwork` возвращает текущую версию сети и высоты зафиксированных обновлений. После импорта Go-снимка доступны версии из его AppDB. Следующее обновление появляется в ответе после `Commit`. [Реализация Minter v2 в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/api/v2/service/version.go).

В запросах комиссий и голосов параметр `height` выбирает зафиксированное состояние. Если его опустить или передать 0, используется последняя высота. `Halts` читает из текущего состояния голоса за целевую высоту в пути. [Контракты Minter v2](https://minterteam.github.io/node-gateway-api-v2-doc/).

`Subscribe` передаёт новые события Tendermint по выражению подписки. HTTP выдаёт по одному JSON-сообщению на строку. WebSocket на том же пути выдаёт одно текстовое сообщение на событие. gRPC отдаёт поток `SubscribeResponse`. Можно фильтровать по `tm.event`, хешу транзакции и тегам ABCI. [Контракт Subscribe](https://raw.githubusercontent.com/MinterTeam/node-grpc-gateway/53ffbb191668/api.proto), [грамматика запросов Tendermint 0.34](https://github.com/tendermint/tendermint/blob/v0.34.19/libs/pubsub/query/query.peg).

```sh
curl -N -G http://127.0.0.1:8841/v2/subscribe --data-urlencode "query=tm.event = 'NewBlock'"
```

Подписка действует 60 секунд. Каждый поток занимает одно соединение с RPC Tendermint, освобождаемое при отмене и остановке. `minterd` принимает до 100 потоков, а подключённый Tendermint применяет свой `max_subscription_clients`, включая подписку индексатора. При разрыве связи с Tendermint поток завершается с `UNAVAILABLE`. Новая подписка после восстановления получает следующие события. [Go Subscribe](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/api/v2/service/subscribe.go), [Tendermint 0.34.19 WebSocket RPC](https://github.com/tendermint/tendermint/blob/v0.34.19/rpc/core/events.go).

В запросах адреса и монеты `height=0` выбирает последнее зафиксированное состояние. Для исторического запроса укажите сохранённую версию IAVL по правилам Go API. `delegated=true` добавляет стейки и суммарную стоимость в BIP. Удалённая версия возвращает `NOT_FOUND` и HTTP 404. `keep_last_states` в `Status` показывает настройку Rust, где положительный лимит включает текущую версию. В Go положительное значение считает прошлые версии. [Схема protobuf](crates/minter-api/schema/README.md), [маршруты Minter v2](https://raw.githubusercontent.com/MinterTeam/node-grpc-gateway/53ffbb191668/api.proto).

Для поиска и чтения по хешу включите штатный индекс Tendermint `[tx_index] indexer = "kv"`. `Transactions` использует запросы Tendermint и сортирует результаты от новых к старым. `Block` принимает повторяющийся `fields` и включает неуспешные транзакции при `failed_txs=true`. После очистки версии IAVL историческое вознаграждение v300 может быть недоступно при сохранённом блоке. [Индекс Tendermint 0.34](https://docs.tendermint.com/v0.34/app-dev/indexing-transactions.html).

`Events` возвращает события Minter по высоте. Повторяющийся `search` выбирает события стейкинга по точному совпадению с адресом `Mx…` или ключом валидатора `Mp…`. `Block?events=true` добавляет события в типизированных protobuf `Any`. История начинается с исполнения блоков этой версией `minterd`. Пустой исполненный блок возвращает `events: []`. Для высоты вне истории `Events` возвращает HTTP 404, а `Block` с доступным блоком возвращает пустой список событий. [Events в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/api/v2/service/events.go), [Block в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/api/v2/service/block.go).

`--keep-event-blocks 100000` задаёт окно событий в RocksDB в высотах, включая текущую. Настройка сохраняется после перезапуска. Ноль сохраняет дальнейшие события и используется по умолчанию. Окно событий действует независимо от IAVL и истории Tendermint. Импорт Go-снимка восстанавливает состояние приложения на высоте снимка. Ранее записанные события требуют исполнения соответствующих блоков или переноса отдельного архива Go.

`SendTransaction` принимает уже подписанную транзакцию с префиксом `0x`. Для REST также доступен `POST /v2/send_transaction` с JSON вида `{"tx":"0x…"}`. Успешный ответ подтверждает `CheckTx`. Дождитесь появления хеша в `Transaction` и проверьте `code`, чтобы узнать результат исполнения в блоке. [Семантика broadcast_tx_sync](https://docs.tendermint.com/v0.34/tendermint-core/using-tendermint.html).

REST использует имена protobuf, строки для 64-битных чисел и `null` для отсутствующих сообщений. Суммы хранятся и выдаются в минимальных единицах. Данные о монетах в транзакциях читаются из одного закреплённого снимка RocksDB на запрос. Сравнение с Go запускается скриптами `tools/verify_minter_api_v2.py`, `tools/verify_mainnet_api_v2.py` и `tools/verify_minter_events.py`.

## Лёгкий клиент

В `trust.json` укажите `chainId`, `height` и `hash` независимо проверенного заголовка Tendermint. Поля `initialHeight` и `trustingPeriodSecs` задают начальную высоту сети и срок доверия в секундах. Для `minter-mainnet-5` начальная высота в Go-снимке равна `10197000`. Срок доверия должен быть короче периода разблокировки валидаторов. Используйте RPC двух независимых операторов. [Параметры Tendermint Light Client](https://docs.rs/tendermint-light-client/0.40.4/tendermint_light_client/light_client/struct.Options.html).

```text
minterctl light init --db light.redb --anchor trust.json --primary <rpc> --witness <witness-rpc>
minterctl light sync --db light.redb --primary <rpc> --witness <witness-rpc>
minterctl light status --db light.redb
```

`sync` проверяет подписи и наборы валидаторов на каждом источнике и сверяет хеш целевого заголовка. По умолчанию выбирает высоту на один блок ниже общего RPC-head. `--height` позволяет указать конкретную высоту, включая историческую при наличии подходящего доверенного заголовка. redb хранит до 256 проверенных заголовков с наборами валидаторов. `status` показывает последнюю проверенную высоту и срок доверия даже при недоступных RPC.

Для баланса на высоте `H` сначала сохраните проверенный заголовок `H+1`. RPC-провайдер баланса должен обслуживать `/iavl/key` с операцией `ics23:iavl`. Запрос доступен в Rust и тестовом Go-приложении. Проверяются ключ аккаунта, список его монет и значение выбранной монеты. [Чтение баланса в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/state/accounts/accounts.go#L192), [доказательства Go IAVL](https://github.com/cosmos/iavl/blob/v0.17.3/proof_ics23.go).

```text
minterctl light balance --db light.redb --rpc <proof-rpc> --address <Mx-address> --coin <coin-id> --height <H>
minterctl light tx --db light.redb --rpc <rpc> --hash <tx-hash> --height <H>
minterctl light tx --db light.redb --rpc <rpc> --hash <tx-hash> --height <H> --result
```

Для включения транзакции нужен проверенный заголовок `H`. Команда сверяет хеш байтов и доказательство с `DataHash`. С флагом `--result` она дополнительно требует заголовок `H+1` и проверяет `Code`, `Data`, `GasWanted` и `GasUsed` по `LastResultsHash`. [Доказательство транзакции Tendermint 0.34](https://github.com/tendermint/tendermint/blob/v0.34.19/types/tx.go), [хеш результатов исполнения](https://github.com/tendermint/tendermint/blob/v0.34.19/types/results.go).

Доказательство ключа из локальной RocksDB можно сохранить и сверить с независимо подтверждённым AppHash. Пустое значение и доказанное отсутствие представлены в JSON как `""` и `null`.

```text
minterctl iavl prove --db state --height <H> --key <hex> > proof.json
minterctl iavl verify-proof --proof proof.json --height <H> --app-hash <trusted-root> --key <hex>
```

Для проверки на локальной сети с двумя Go-нодами и Rust-наблюдателем соберите `go-abci` и `go-fixture` по инструкции для локального перевода. Скрипт проверяет запуск, сохранение доверия, баланс, включение, исполнение и ответы с подменой данных.

```text
python -B tools/verify_light_devnet.py <minterctl> <minterd> <go-abci> <tendermint-0.34.19> <go-fixture> <new-result-directory>
```

## Получение Go-снимка через State Sync

Запустите `minterd` с доверенным манифестом Go-снимка формата 1 и новым каталогом RocksDB.

```text
minterd --db ./state --state-sync-checkpoint ./checkpoint.json
```

В конфигурации Tendermint 0.34 включите `[statesync]`, задайте `rpc_servers`, `trust_height`, `trust_hash`, `trust_period` и подключите пир, который раздаёт этот снимок. Tendermint проверяет AppHash высоты снимка по заголовку следующего блока. Манифест задаёт SHA-256 всего zlib-потока, включая поля Go AppDB, и должен быть получен из доверенного источника. [Проверки Tendermint](https://github.com/tendermint/tendermint/blob/v0.34.19/statesync/syncer.go), [формат Go Minter](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/appdb/snapshot.go).

Rust проверяет размер и SHA-256 каждой части, собирает поток во временный файл, пересчитывает корень IAVL и публикует AppDB с корнем одной записью RocksDB. Повреждённую часть можно получить повторно. Если процесс остановлен до публикации, запустите его с тем же манифестом и повторите получение снимка. После успешного импорта нода стартует из RocksDB. Публикация снимков из Rust и получение манифестов операторов находятся в плане.

Стенд с Go-провайдером, отдельным RPC-свидетелем и Rust-узлом запускается следующей командой. `go-abci` и `go-fixture` собираются по инструкциям для локального перевода выше.

```text
python tools/verify_state_sync_devnet.py <minterd> <go-abci> <tendermint-0.34.19> <go-fixture> <new-result-directory>
python tools/verify_state_sync_mainnet.py <minterd> <corpus-manifest.json> <snapshot.zlib> <checkpoint-manifest.json> <abci-iavl.jsonl.gz> <blocks.csv.gz> <new-result-directory>
```

На Windows 11 и Ubuntu 24.04 в QEMU Tendermint получил снимок по P2P, подтвердил Rust ABCI и продолжил с блока после снимка. Перевод и повторное открытие базы прошли на обеих ОС. Тест на реальном снимке 40 559 287 из четырёх частей проверил неверный AppHash, повреждение части, прерывание загрузки и завершение процесса во время записи IAVL в WAL. После повторного импорта совпали корень снимка и три следующих блока.

## Исполнение блоков после Go-снимка

Полный прогон корпуса `minter-mainnet-5` на Windows 11, Ubuntu 24.04 в QEMU и Ubuntu 26.04 в WSL2 сверил **102 000 блоков** с 40 559 288 по 40 661 287, включая 1 255 результатов транзакций и 1 976 обновлений валидаторов. На каждом стенде процесс остановили перед `Commit` пересчёта награды на 40 593 601. После перезапуска он повторил блок с корня 40 593 600, дошёл до подписанного конечного AppHash и выдал 46 проверенных исторических чтений IAVL. В Ubuntu 24.04 в QEMU отдельные прогоны проверили `Unbond`, `MoveStake`, обновление цены награды, отключение валидатора за пропуски, `SetCandidateOnline` и `Delegate`.

Для прогона с исходным корпусом передайте его `manifest.json`, `go_checkpoint/snapshot.zlib`, `go_checkpoint/manifest.json`, `abci-iavl.jsonl.gz`, `blocks.csv.gz` и флаги `--through 40661287 --interrupt-before 40593601`. Сверка использует входы и ответы ABCI. Мутации из трассы приложение вычисляет при исполнении.

Скрипт передаёт в Rust записанные входы ABCI для высот **40 559 288–40 559 912**, включая девять `Multisend`, два `CreateToken` и периодическую выплату. Rust рассчитывает состояние самостоятельно. Каждый ответ сверяется с Go, каждый AppHash — с аутентифицированным индексом следующего заголовка. Прогон останавливает процесс после `EndBlock` выплаты и повторяет блок с предыдущей высоты. После повторного открытия RocksDB скрипт сверяет исторические ключи, включая стейки кандидатов, накопленные награды, валидаторов и лист ожидания.

```text
python tools/verify_mainnet_empty.py <minterd> <minterctl> <corpus-manifest.json> <snapshot.zlib> <checkpoint-manifest.json> <abci-iavl.jsonl.gz> <blocks.csv.gz> <new-result-directory> --through 40559912 --interrupt-before 40559760 --go-state <state-40559759.json> <state-40559760.json>
```

Режим `-balances` инструмента `read_go_checkpoint.go` читает эталонные значения и nonce через Go Accounts. Аргументы после высоты снимка задаются как `ADDRESS_HEX:COIN_ID`. В обычном режиме инструмент читает ключи IAVL в hex. Оба флага Python `--go-balances` и `--go-state` принимают несколько файлов. Скрипт `slice_mainnet_corpus.py` сохраняет нужный диапазон из исходного корпуса с проверкой хешей файлов и корней.

В mainnet сверены тикеры TETHERMINT и DELREVO, начальные балансы, право владельца и комиссия за тикер. Фикстура Go дополнительно проверяет резервную монету, выпуск и сжигание токенов и случаи отказа. При нулевой комиссии за тикер Go сохраняет выпуск и возвращает код 119. Rust воспроизводит этот порядок. [Создание токена в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/create_token.go), [расчёт комиссии за тикер](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/executor_v3.go).

В блоке **40 559 760** Rust распределил награды по правилам v330, пересчитал стейки, перенёс вытесненные позиции в лист ожидания и вернул 14 весов валидаторов. AppHash совпал с Go на обеих ОС. Для оценки резервных монет используется арифметика с округлением до 100 значащих двоичных бит. `make_bancor_fixture.go` воспроизводит эталонные значения теста. Выплаты до v330, одноразовая коррекция при его активации и удаление кандидатов сверх лимита остаются в плане. [Расчёт наград](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/state/validators/validators.go), [перерасчёт стейков](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/state/candidates/candidates.go), [формула резервной монеты](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/formula/formula.go).

`make_mainnet_checkpoint.go` экспортирует снимок перед выбранным блоком и сверяет повтор блока после импорта с исходными ответами и заголовками.

На высоте **40 559 913** Rust исполнил `SellSwapPool` через четыре пула SwapV2 с оплатой газа в BIP. На **40 559 944** прошёл `BuySwapPool` с оплатой газа монетой 3494. Прогон по **40 560 344** сверил 1 057 последовательных блоков, включая переводы с комиссией в монетах 1993 и 3494. На обеих ОС остановка перед `Commit` покупки сохранила прежний корень. Для повторения используйте `--through 40560344 --interrupt-before 40559944`. [Продажа через SwapV2 в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/sell_swap_pool_v260.go), [покупка](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/buy_swap_pool_v260.go).

`SellAllSwapPool` проверен отдельно на **40 590 062** из Go-снимка 40 590 061. Rust списывает газ в продаваемой монете и учитывает изменение резервов, когда газ и обмен используют один пул. Для выборки с таким снимком передайте `--go-reference` в `slice_mainnet_corpus.py`. [Продажа всего баланса в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/sell_all_swap_pool_v260.go).

Сверены первое частичное исполнение лимитного ордера на **40 560 345**, размещение на **40 560 543**, возврат замороженных средств на **40 565 674** и `AddLiquidity` на **40 567 481**. Windows 11 и Ubuntu 26.04 в WSL2 исполнили **13 422** последовательных блока по 40 572 709. Ubuntu 24.04 в QEMU исполнила первые 8 193 блока и следующие три отдельным прогоном с проверкой восстановленного корня. Сверено 56 исторических чтений IAVL на каждой платформе. [Сопоставление ордеров в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/state/swap/orderV2.go), [обработка замороженных средств](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/minter/blockchain.go#L358), [добавление ликвидности](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/add_liquidity_v260.go).

Сверены `SellCoin` на **40 572 710** и **40 572 888**. Windows 11 и Ubuntu 26.04 в WSL2 исполнили **14 211** последовательных блоков до 40 573 498. Ubuntu 24.04 в QEMU исполнила последние 789 блоков из Go-снимка 40 572 709. На каждом стенде проверены остановка перед `Commit` первой продажи, повтор блока и 16 исторических чтений резервов, объёмов и балансов. [Продажа в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/sell_coin.go), [формулы Bancor](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/formula/formula.go).

Команда `go run -mod=readonly /path/to/minter-rust/tools/make_abci_fixture.go swaps <new-directory>` воспроизводит 69 синтетических блоков. Фикстура покрывает покупки и продажи, лимиты, ликвидность, ордера, газ из пула, ордеров и резерва, возвраты средств, операции со стейками и смену статуса кандидата. Флаг `--existing-db` у `verify_mainnet_empty.py` продолжает тестовый прогон в имеющейся RocksDB после проверки совпадения высоты и корня с Go-снимком.

`make_mainnet_checkpoint.go` готовит снимок перед первым реальным `Send` этого прогона на высоте **40 574 360**. Команда запускается из корня Go-эталона на указанной выше ревизии.

```text
go run -mod=readonly /path/to/minter-rust/tools/make_mainnet_checkpoint.go <corpus-manifest.json> <snapshot.zlib> <checkpoint-manifest.json> <abci-iavl.jsonl.gz> <blocks.csv.gz> 40574360 <new-checkpoint-directory>
```

Go заново исполняет 15 073 блока, сверяет ответы ABCI и AppHash с корпусом, экспортирует состояние после 40 574 359 и повторяет целевой блок с восстановленного снимка. В `reference.json` записаны проверенный корень, исходные резервы пула и фактическая комиссия.

```text
python tools/verify_mainnet_send.py <minterd> <minterctl> <fixture-directory> <new-result-directory>
```

В каталоге `fixture` из архива прогона лежат снимок, манифест, эталон Go, записи ABCI для **40 574 360–40 574 393** и строки проверенного индекса заголовков. Скрипт сверяет ответы ABCI и AppHash всех 34 блоков, включая два BIP-перевода. Он останавливает Rust перед `Commit` первого перевода, повторяет блок с сохранённого корня и проверяет состояние после повторного открытия RocksDB.

Для этих переводов цена комиссии задана в монете 1993, а комиссия списана в BIP. Rust читает резервы и индекс ордеров из IAVL и учитывает округление Go при расчёте котировки. [Расчёт цены в Go](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/transaction/executor_v3.go#L178), [формулы SwapV2](https://github.com/MinterTeam/minter-go-node/blob/v3.3.0/coreV2/state/swap/orderV2.go#L196).

## Подключение к пиру Tendermint 0.34

`minterctl peer` открывает шифрованное соединение, сверяет ключ пира с указанным ID и проверяет сеть, версию блоков и доступность канала блоков. `status` показывает диапазон, который сообщает пир. `block` запрашивает указанные высоты через одно соединение и сверяет `DataHash` каждого блока с транзакциями. Для другой сети укажите `--chain`. [Рукопожатие Tendermint](https://docs.tendermint.com/master/spec/p2p/peer.html), [формат пакетов 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/p2p/conn/connection.go).

```text
minterctl peer status --peer <id@IP:port>
minterctl peer block --peer <id@IP:port> --height 10197000 --height 10523520 --output ./blocks.pb
```

Опциональный выходной файл содержит исходные ответы `BlockResponse` с varint-длинами, в порядке запроса. Он публикуется после получения всей выборки. Для аутентификации цепочки используйте сверку подписанных коммитов из доверенного начального набора валидаторов.

Воспроизводимые проверки используют локальный Go-узел и архивный пир mainnet. В `<blocks>` лежат части корпуса `<first>-<last>.pb`, а в `<go-reference>` — `result.json` и `abci.jsonl.gz` подписанного прогона Go.

```text
python -B tools/verify_native_peer.py <minterctl> <tendermint-0.34.19> <new-devnet-results>
python -B tools/verify_native_peer_mainnet.py <minterctl> <blocks> <go-reference> <new-mainnet-results> --peer <id@IP:port> --heights 10197000 10292713 10523520
```

## Нативная синхронизация блоков Tendermint 0.34

`minterd native-sync` запускает Minter из genesis и получает блоки по P2P. Для `minter-mainnet-5` нужен официальный genesis с SHA-256 `906c2ab969389853247305b0930c92f5906dc1962cd3075832ca4d7ab0fe55a5`. После первой фиксации блока повторный запуск открывает сохранённый курсор RocksDB. `--through` задаёт последнюю высоту исполнения. В режиме P2P значение по умолчанию равно высоте, объявленной пиром при подключении, за вычетом двух блоков.

```text
minterd native-sync --db ./state --genesis ./genesis.json --peer <id@IP:port> --through 10197007
minterd native-sync --db ./state --blocks ./10197000-10199047.pb --through 10197365
```

При воспроизведении укажите `--blocks` для каждой части по порядку. В источнике нужны два блока после целевой высоты. Первый несёт коммит исполненного блока и новый AppHash. Второй позволяет проверить подписи под заголовком с этим AppHash. Rust сверяет идентификатор и PartSet, подписи и голосующую силу, время, результаты транзакций и применение обновлений валидаторов через две высоты. RocksDB фиксирует IAVL, сырой блок, коммит, результаты и курсор одной записью после сверки AppHash. [Реактор Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/blockchain/v0/reactor.go#L328-L371), [обновление состояния Tendermint](https://github.com/tendermint/tendermint/blob/v0.34.19/state/execution.go).

`--keep-iavl-versions 4` оставляет четыре версии IAVL. Значение `0` сохраняет архив IAVL. Параметр записывается в базу. В нативном режиме записи блоков сохраняются в RocksDB. `--trace` выводит сведения о каждом сохранённом блоке для сверки с Go.

### Выдача проверенных блоков

`--p2p-listen` включает реактор блоков Tendermint 0.34. При запуске с источником блоков архив доступен другим пирам во время синхронизации. После достижения `--through` слушатель продолжает работать. Для выдачи существующего архива достаточно указать базу и адрес.

```text
minterd native-sync --db ./state --p2p-listen 127.0.0.1:26656
minterd native-sync --db ./state --peer <id@IP:port> --follow --p2p-listen 0.0.0.0:26656 --p2p-advertise <public-IP:port>
```

Адрес в `--p2p-advertise` передаётся в Tendermint `NodeInfo`. По умолчанию ключ идентичности пира сохраняется в `node_key.json` внутри каталога RocksDB. `--node-key` позволяет указать другой путь или совместимый ключ Tendermint. ID для `persistent_peers` отображается в журнале запуска.

Границы P2P-архива определяют сохранённые блоки. После быстрого старта нижняя граница равна высоте снимка, а при синхронизации от genesis — высоте первого блока. Версии IAVL удаляются отдельно. Другой пир может запросить сохранённый блок, даже если версия состояния на его высоте уже очищена.

```text
python -B tools/verify_native_sync.py <minterd> <minterctl> <genesis.json> <10197000-10199047.pb> <go-reference> <new-results> --peer <id@IP:port>
python -B tools/verify_native_block_server.py <minterd> <minterctl> <tendermint-0.34.19> <minter-go-abci> <go-send-fixture> <new-results>
```

### Обмен транзакциями с Tendermint 0.34

`--mempool` включает приём и рассылку подписанных транзакций. Используйте `--mempool-peer` для исходящих соединений с пирами, которые поддерживают каналы mempool и состояния раунда. Go-нода может также подключиться к адресу Rust из `--p2p-listen`. Rust проверяет транзакции через Minter CheckTx, отклоняет дубли и удаляет из очереди транзакции, вошедшие в проверенный блок. Очередь хранится в памяти. После перезапуска транзакции повторно поступают от пиров или клиентов.

```text
minterd native-sync --db ./state --peer <id@IP:port> --follow --p2p-listen 127.0.0.1:26656 --mempool --mempool-peer <id@IP:port> --api-listen 127.0.0.1:8841 --grpc-listen 127.0.0.1:8842
```

Укажите один и тот же ID и адрес в `--peer` и `--mempool-peer`, чтобы загружать блоки и обмениваться транзакциями с этим пиром по одному исходящему соединению. Это работает с настройкой Tendermint `allow_duplicate_ip = false`. Для постоянного обмена по общему соединению оставьте `--follow` и укажите этот пир первым в списке источников блоков.

Локальная отправка через HTTP и gRPC использует Minter v2 `SendTransaction`. Успешный ответ возвращает хеш принятой транзакции. Включение в блок подтверждается отдельно. `Status` и запросы к состоянию работают из нативной RocksDB. Запросы блока, поиска транзакций и подписок пока требуют переходного режима с Tendermint RPC. [Протокол mempool Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/mempool/reactor.go).

```text
python -B tools/verify_native_mempool.py <minterd> <minterctl> <tendermint-0.34.19> <minter-go-abci> <go-send-fixture> <new-results> --oracle <minter-api-oracle>
```

Добавьте `--shared`, чтобы прогнать сценарий с одним соединением между Rust и Go-валидатором. Используйте новый каталог результатов для каждого прогона.

### Нативные раунды в смешанной тестовой сети

`--consensus` включает приём предложений, частей блока и голосов Tendermint 0.34. Пир подключается к Rust по адресу `--p2p-listen`. Наблюдатель проверяет блок и подписанные precommit с суммарным весом строго больше двух третей. RocksDB атомарно сохраняет блок, коммит и вычисленный AppHash. Для старта нужны genesis либо база с сохранёнными приоритетами proposer.

```text
minterd native-sync --db ./state --genesis ./genesis.json --consensus --mempool --p2p-listen 127.0.0.1:26656 --api-listen 127.0.0.1:8841
```

Для локального испытания валидатора подготовьте выделенный журнал через `minterd validator-signer import` и добавьте `--validator-key` и `--validator-journal`. Используйте отдельный ключ стенда. Rust сохраняет последнюю подпись, предложение и блокировку голосования одной записью журнала, затем может повторно передать уже подписанный голос. До первой фиксации блока для повторного запуска снова укажите genesis.

```text
python -B tools/verify_native_consensus.py <minterd> <minterctl> <tendermint-0.34.19> <minter-go-abci> <go-send-fixture> <new-results>
python -B tools/verify_native_consensus.py <minterd> <minterctl> <tendermint-0.34.19> <minter-go-abci> <go-send-fixture> <new-results> --validator-fixture-tool <make-consensus-fixture>
```

Режим валидатора экспериментальный. В локальной смешанной сети проверены обмен конфликтующими голосами, `DuplicateVoteEvidence` и вариант `LightClientAttackEvidence` с изменённым AppHash. Ожидающие доказательства переживают перезапуск; запись блока, индекса уже включённых доказательств и обновлённой очереди атомарна. Для готовности к mainnet остаются другие варианты доказательств, контекст до высоты снимка, поздние раунды и испытания на большем числе пиров.

Отставшему пиру передаются сохранённые части и precommit для любой доступной высоты. Используются исходные подписи из записи блока, включая после перезапуска; кеш ограничен одним блоком и не меняет последний commit для собственного следующего предложения. [Догон в Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/consensus/reactor.go).

История наборов валидаторов хранится отдельно от IAVL. При первом запуске старой базы от genesis нода восстанавливает её из сохранённых блоков и ответов `EndBlock`, не исполняя транзакции повторно. Учитывается применение обновлений на H+2. Завершённый индекс публикуется после сверки с текущим курсором; прерванное восстановление можно повторить. [Обновление состояния в Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/state/execution.go).

Для старой базы от Go-снимка один раз запустите ноду с исходными `--checkpoint` и `--checkpoint-sha256`. Пакет повторно проверяется и даёт начальные наборы валидаторов. После восстановления можно запускать ноду без пакета. История ниже высоты снимка пока недоступна.

```text
python -B tools/verify_native_evidence.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results> --legacy-daemon <previous-minterd>
python -B tools/verify_native_evidence.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results> --legacy-daemon <previous-minterd> --light-attack
python -B tools/verify_native_history_upgrade.py <minterd> <minterctl> <legacy-db> <checkpoint> <new-results> --checkpoint-sha256 <trusted-digest>
```

Первый и второй сценарии создают базу старой сборкой с хранением четырёх версий IAVL и проверяют доказательство с прежней высоты после обновления. Третий работает с копией уже остановленной базы и проверяет её миграцию, сохранность AppHash и последующий запуск без пакета.

Сценарий самостоятельного предложения Rust использует тестовые веса 80% Rust и 20% Go. Доказательство сохраняется в процессе наблюдателя, затем Rust перезапускается с подписантом и включает его в блок при выключенном Go. После возвращения Go сверяются фактический proposer, AppHash, PartSet, полный EndBlock и изменение валидаторов на H+2. Следующий перезапуск проверяет отказ при повторе. Пересоберите Go-помощник из текущего `tools/make_evidence_fixture.go`: для сверки EndBlock ему нужен режим `end-block`.

```text
python -B tools/verify_native_evidence.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results> --rust-proposer
python -B tools/verify_native_evidence.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results> --rust-proposer --light-attack --light-short-nil
```

Разбор доказательств сверяется с отдельным корпусом из 39 входов Go 0.34.19: проверяются результат приёма, декодирование блока, хеши и байты ABCI. При подсчёте `Evidence.MaxBytes` сохраняется размер исходного Go protobuf до канонизации. Части блока строятся из тех же канонических байтов, что использованы для PartSetHash; исходный ответ сохраняется для архива. Проверяются размер, число частей и сборка в обратном порядке, включая блок из двух частей. [Решение о формате](docs/decisions/0001-tendermint-evidence-wire.md).

```text
# Из корня закреплённого Go-эталона:
go run -mod=readonly <source>/tools/make_light_wire_reference.go <source>/crates/minter-p2p/tests/fixtures/go-light-wire.json
# Из корня Rust:
cargo test --locked -p minter-p2p --test light_wire
cargo test --locked -p minter-app --lib raw_light_evidence_has_the_same_abci_bytes_as_go
```

Добавьте `--light-short-nil` к сквозному сценарию с `--light-attack`, чтобы проверить Go-допустимую короткую подпись Nil в конфликтующем коммите. Для offline replay реального диапазона служит `tools/verify_native_archive.py`; он сравнивает подписанные блоки и полный ABCI-транскрипт с Go.

Стенд из двух Go- и двух Rust-валидаторов использует по 25% начального веса. Go-помощник должен быть собран из текущего `tools/make_evidence_fixture.go` с режимом `multinode`, а каталог исходной Go-фикстуры должен содержать `genesis.json` и `go-abci-v034.json`. Каждый запуск требует нового каталога результатов.

```text
python -B tools/test_tcp_link.py
python -B tools/verify_native_multinode.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results>
```

Сценарий сверяет блоки, AppHash, PartSet, результаты транзакций и полный EndBlock на четырёх узлах. Он проверяет две транзакции, остановку и возврат Go и Rust, новые подписи и предложения вернувшихся валидаторов, разделение 2+2 через TCP-прокси на loopback и восстановление связи. Во время разделения процессы продолжают работать, а новые commit отсутствуют в течение пяти секунд после завершения уже начатого обмена. Затем проверяются переход 4→3 валидатора на H+2 и повтор ранее включённого доказательства после перезапуска Rust. Событие `evidence_already_committed` с хешем подтверждает чтение сохранённой отметки. Прокси не изменяют передаваемые байты; отдельные тесты покрывают разрыв активных и ещё устанавливаемых соединений. Этот стенд использует исходящие соединения Go.

Для соединений Rust→Rust укажите `--consensus-peer <id@IP:port>`, повторяя параметр для нескольких пиров. Он требует `--consensus`; на обеих сторонах нужны каналы консенсуса. `--mempool` и выдача снимков согласуются отдельно. Узел проверяет ID и цепочку пира, повторяет соединение после разрыва и прекращает ожидание при остановке. Можно настроить до 16 исходящих пиров с различными ID. Входящие соединения используют тот же обработчик. Для родительского desktop-процесса доступен `--shutdown-on-stdin-close`.

```text
minterd native-sync --db ./state --genesis ./genesis.json --p2p-listen 127.0.0.1:26656 --consensus --mempool --validator-key ./validator-key.json --validator-journal ./signer --consensus-peer <id@IP:port>
python -B tools/verify_native_peer_shutdown.py <minterd> <genesis.json> <new-results>
python -B tools/verify_native_mesh.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results>
```

Последний сценарий создаёт четыре Rust-валидатора по 25% веса и шесть исходящих Rust-соединений через TCP-прокси. Он проверяет две транзакции, остановку и возврат одного валидатора, разделение 2+2 с сохранением внутренних соединений, возврат связи и принудительный перезапуск всех четырёх узлов. До и после перезапуска требуются подписи и предложения каждого валидатора; после восстановления проходят ещё 32 блока, и незапланированное изменение набора считается ошибкой. Go запускается после завершения этих проверок: Rust уже не подписывает, а только выдаёт сохранённые блоки. Обычная Go-нода затем проверяет commits и исполняет завершённую цепочку для сравнения AppHash, PartSet, результатов транзакций и EndBlock. Отдельный CLI-сценарий проверяет ограничения пиров и остановку при незавершённом рукопожатии.

При переходе на новую высоту Rust принимает поздние precommit предыдущего блока и дополняет сохранённый commit после проверки по прежнему набору валидаторов. Исходный блок и состояние исполнения сохраняются. Новые подписи распространяются во время `NewHeight`; локальное голосование продвигается как таймером, так и входящими сообщениями. [Хранение и границы этой операции](docs/decisions/0002-native-commit-completion.md).

Отдельный семивалидаторный сценарий проверяет раунд 2. Go-помощник должен поддерживать режимы `quorum` и `proposers`. Начальное состояние Go определяет первых предлагающих; два первых валидатора не запускаются, а остальные пять сохраняют кворум. Один процесс останавливается после долговечного precommit раунда 1 и восстанавливается из журнала. Затем запускаются два оставшихся валидатора, и все семь должны подписать и предложить блоки в следующем окне из 24 блоков.

```text
python -B tools/verify_native_rounds.py <minterd> <tendermint-0.34.19> <minter-go-abci> <make-evidence-fixture> <go-send-fixture> <new-results>
```

Проверочный Go стартует с отдельной свежей базой после остановки Rust-валидаторов. В результате явно указаны сравниваемые высоты и дополнительные блоки, используемые как свидетели последующих commits. Все 21 соединение проверяются по событиям `consensus_peer_ready`: событие появляется один раз на соединение после приёма корректного сообщения о раунде через аутентифицированную сессию. ID и порты обеих сторон сопоставляются с текущими TCP-соединениями стенда. `validator_vote_signed` сообщает сохранённую позицию голосования в раундах выше нулевого; оно содержит `consensusHeight`, `round` и `voteType`, без подписи или ключа.

### Быстрый старт из Go-снимка

Укажите каталог с `manifest.json`, `snapshot.zlib`, `state.pb` Tendermint 0.34 и тремя последовательными ответами в `blocks.pb`. Для `--checkpoint-sha256` используйте SHA-256 манифеста из доверенного источника. Хеши в манифесте защищают файлы пакета, в том числе данные AppDB вне IAVL. Rust сверяет состояние консенсуса и AppHash с подписанными блоками вокруг высоты снимка, затем восстанавливает корень IAVL. [Состояние Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/statesync/stateprovider.go#L114-L174).

```text
minterd native-sync --db ./state --checkpoint ./checkpoint --checkpoint-sha256 <trusted-digest> --keep-iavl-versions 4 --peer <id@IP:port> --peer <backup-id@IP:port> --follow
```

С `--snapshot-peer` Rust скачивает `snapshot.zlib` через каналы State Sync 0.34. В каталоге чекпоинта нужны только `manifest.json`, `state.pb` и `blocks.pb`. Пир должен объявлять снимок на указанной высоте с форматом и SHA-256 из манифеста. Rust сверяет размер и хеш каждой части, затем хеш всего снимка и корень IAVL. При ошибке передачи он начинает загрузку заново у следующего `--snapshot-peer`. Для продолжения после импорта отдельно укажите `--peer` или `--blocks`. [Каналы и сообщения State Sync](https://github.com/tendermint/tendermint/blob/v0.34.19/statesync/reactor.go).

```text
minterd native-sync --db ./state --checkpoint ./checkpoint-metadata --checkpoint-sha256 <trusted-digest> --snapshot-peer <id@IP:port> --snapshot-peer <backup-id@IP:port> --keep-iavl-versions 4 --peer <block-peer-id@IP:port> --follow
```

При перезапуске после опубликованного импорта достаточно `--db` и источников блоков. Для повтора незавершённого импорта укажите тот же пакет и его SHA-256. С `--follow` процесс ждёт новые блоки и при сбое перебирает указанных пиров. Без `--follow` команда завершает выбранный диапазон. Для воспроизведения из локального корпуса укажите `--blocks` и `--through`.

Для пользовательской сети передайте доверенный genesis вместе с пакетом. Манифест должен содержать `genesisSha256` точных байтов файла и `transactionChainId`. Rust проверяет имя цепи, ID транзакций, первую высоту genesis, `state.pb` и AppDB. Файл genesis и подписанные якоря проверяются до открытия базы; AppDB — перед публикацией состояния.

```text
minterd native-sync --db ./custom-state --genesis ./genesis.json --checkpoint ./custom-checkpoint --checkpoint-sha256 <trusted-digest> --snapshot-peer <id@IP:port> --peer <id@IP:port> --follow
```

Mainnet без явного genesis сохраняет прежний путь импорта. Если файл с именем цепи `minter-mainnet-5` передан явно, принимаются только точные байты официального genesis. Для экспорта пакета пользовательской сети применяется тот же `native-snapshot --anchor-blocks`: идентичность берётся из сохранённого состояния. [Контракт и совместимость прежних баз](docs/decisions/0008-checkpoint-genesis.md).

`tools/export_native_checkpoint.go` готовит пакет из подписанного Go-корпуса. Запустите из корня закреплённого Go-эталона. Передайте доверенную трассу Go от genesis, сверенную с подписанной цепочкой. Эта трасса определяет историю весов и приоритетов proposer. Манифест фиксирует их в `state.pb` через SHA-256 и помечает поле `proposerPriorities`.

```text
go run -mod=readonly <source>/tools/export_native_checkpoint.go <genesis.json> <init-response.pb> <abci.jsonl.gz> <block-shards> <go-snapshot> <new-checkpoint>
```

Если доверенный корпус содержит `validator-sets.json` по хешам наборов, можно собрать пакет для исполнения от снимка по трём соседним блокам. Для этого режима нужны параметры genesis, совместимые с подписанным `ConsensusHash` на высоте снимка.

```text
go run -mod=readonly <source>/tools/export_native_checkpoint.go --archive <genesis.json> <validator-sets.json> <block-shards> <go-snapshot> <new-checkpoint>
```

Пакеты из `--archive` и ранее созданные базы без приоритетов подходят для синхронизации обычного узла. Заголовки подтверждают ключи и веса валидаторов. Для восстановления очередности proposer требуется непрерывное исполнение от genesis или доверенный пакет с `proposerPriorities`.

Для проверки сетевой загрузки соберите Go-пир из корня того же Go-эталона. Сценарий проверяет обрыв загрузки, повреждение части, переход на второй пир и продолжение блоков после перезапуска.

```text
go build -mod=readonly -o <snapshot-peer> <source>/tools/serve_native_sync_fixture.go
python -B <source>/tools/verify_native_state_sync.py <minterd> <minterctl> <snapshot-peer> <checkpoint> <10522675-10523522.pb> <abci.jsonl.gz> <new-results> --checkpoint-sha256 <trusted-digest>
```

### Экспорт и выдача снимка из Rust

Остановите процесс, который держит RocksDB, и выгрузите последнюю зафиксированную версию. Укажите новый каталог. После завершения в нём появятся `snapshot.zlib` формата Go 1 и `manifest.json` с высотой, хешами блока, состояния и файла. Экспорт поддерживает базы, в которых номер версии IAVL совпадает с высотой блока, включая `minter-mainnet-5`.

```text
minterd native-snapshot --db ./state --output ./snapshot
minterd native-sync --db ./state --p2p-listen 127.0.0.1:26656 --serve-snapshot ./snapshot
```

Слушатель проверяет привязку манифеста к зафиксированным блокам и SHA-256 файла. Используйте каталог снимка, подготовленный и проверенный оператором ноды. Слушатель объявляет один снимок и отдаёт части до 10 000 000 байт через Tendermint State Sync. Чтобы заменить снимок, запустите слушатель с новым каталогом.

Go-наблюдателю нужны два доверенных RPC для State Sync и недавняя доверенная высота с хешем блока. Укажите ID и адрес Rust в `persistent_peers`. Снимок и следующие блоки Go запрашивает по P2P. [State Sync Tendermint 0.34.19](https://github.com/tendermint/tendermint/blob/v0.34.19/statesync/stateprovider.go).

Для быстрого старта другой Rust-ноды можно выгрузить готовый пакет. Запишите три последовательных блока на высотах `H`, `H+1` и `H+2`, где `H` — сохранённая высота базы. Для захвата подходит `minterctl peer block`. Экспорт проверяет эти блоки по локальному курсору и наборам валидаторов, сохраняет `state.pb` и `blocks.pb` и выводит `checkpointSha256`. Получателю нужен этот SHA-256 из доверенного источника. В пакете зафиксированы данные для импорта снимка в нативный Rust-узел. [Проверка состояния Tendermint](https://github.com/tendermint/tendermint/blob/v0.34.19/statesync/stateprovider.go#L114-L188).

```text
minterctl peer block --peer <id@IP:port> --height <H> --height <H+1> --height <H+2> --output ./anchor.pb
minterd native-snapshot --db ./state --output ./checkpoint --anchor-blocks ./anchor.pb
```

Нативный пакет Rust сохраняет служебные ссылки замороженных средств в поле `frozenReferences`, когда они нужны для продолжения исполнения после смены ключа кандидата. Поле покрыто `checkpointSha256`; общий предел манифеста — 1 МиБ. Отдельный `snapshot.zlib` сохраняет формат Go. [Контекст смены ключа и поведение импорта](docs/decisions/0007-candidate-key-context.md).

```text
python -B tools/verify_rust_snapshot_devnet.py <minterd> <minterctl> <tendermint-0.34.19> <minter-go-abci> <go-send-fixture> <new-results>
go run -mod=readonly <source>/tools/make_mainnet_checkpoint.go --verify <rust-snapshot> <abci.jsonl.gz> <through>
```

Сценарий devnet восстанавливает базу Go по сети, применяет следующие блоки и повторно открывает её. Go-верификатор пересчитывает корень дерева при импорте и сверяет исполнение с исходным журналом ABCI. Запускайте Go-верификатор из корня закреплённого Go-эталона Minter.

### История до снимка без старых транзакций

Остановленная нода может добавить старые заголовки, составы валидаторов и включённые доказательства отдельным файлом. Это позволяет проверяющему коду использовать контекст до высоты снимка без повторного исполнения блоков. Экспортируйте нужный непрерывный диапазон из имеющего его архива; `--anchor` должен совпадать с высотой исходного снимка получателя.

```text
minterd native-history export --db ./archive --anchor 40700741 --from 40699717 --output ./context.pb
minterd native-history import --db ./state --input ./context.pb
minterd native-history status --db ./state
```

Данные проверяются по цепочке от уже доверенного снимка. Импорт не меняет AppHash, версии IAVL или диапазон полных блоков. После обрыва повторите ту же команду: она проверит сохранённую часть и продолжит. Для старой базы без индекса при первом открытии укажите её исходные `--checkpoint` и `--checkpoint-sha256`. [Формат и проверки](docs/decisions/0003-pre-snapshot-context.md).

Для автоматического выбора полного окна по числу блоков и времени добавьте флаг к экспорту снимка:

```text
minterd native-snapshot --db ./archive --output ./snapshot --anchor-blocks ./anchor.pb --with-history-context
```

`anchor.pb` содержит три захваченных BlockResponse на H, H+1 и H+2. Экспорт добавляет `history.pb` и его описание в доверенный манифест. `native-sync --checkpoint ...` импортирует историю локально либо скачивает её с указанных `--snapshot-peer`. Эти пиры должны раздавать тот же снимок с историей через `--serve-snapshot`. Если передача прервётся, повторите запуск с прежними параметрами. До полной проверки требуемой истории нода не исполняет новые блоки и не запускает консенсус. `native-history status` показывает `requiredHistoryReady`. Go-ноды продолжают использовать обычный формат снимка. [Устройство, доверие и ограничения](docs/decisions/0004-snapshot-history-bundle.md).

В нативном режиме можно выбрать готовый профиль хранения:

```sh
minterd native-sync --db ./state --storage-profile full --peer ID@IP:PORT --follow
minterd native-sync --db ./state --storage-profile archive --peer ID@IP:PORT --follow
```

`full` оставляет 120 версий IAVL, 100 000 высот событий и 100 000 полных тел блоков. `archive` задаёт ноль для всех трёх лимитов и сохраняет дальнейшую историю. Настройки записываются в базу; следующий запуск без флагов использует сохранённые значения. На новой базе без выбора профиля сохраняются прежние архивные настройки. Эти же флаги работают с `--checkpoint` и сетевой синхронизацией после завершения восстановления.

Для своих лимитов используйте `--keep-iavl-versions`, `--keep-event-blocks` и `--keep-native-blocks`. Не передавайте их одновременно с `--storage-profile`. Полные тела блоков можно хранить отдельно от версий состояния: например, `--keep-native-blocks 10000`. Для тел положительное значение должно быть не меньше двух. Нужные старые заголовки и составы остаются в компактной истории, окно очищается по обоим ограничениям возраста. Увеличение лимитов и переход на `archive` не возвращают уже удалённые данные. Импорт снимка также не создаёт отсутствовавшую историю событий и версий состояния.

`native-history status` показывает `storageProfile`, объект `retention` с тремя лимитами и фактические диапазоны IAVL, событий, полных блоков и компактной истории. Имя `custom` означает собственное сочетание лимитов. Отсутствующий диапазон возвращается как `null`. Прежнее поле `keepNativeBlocks` сохранено.

`native-history maintain` завершает обслуживание и сбор неиспользуемых составов. Для возврата места файловой системе остановите ноду и выполните `minterd native-history maintain --db ./state --compact`. Компакция может временно потребовать дополнительное место. Обычная очистка также выполняется при запуске и после новых блоков. Раздача снимка автоматически прекращается, если полные блоки сразу после него уже удалены: подготовьте более свежий снимок. Обычный P2P продолжает работу. [Политика хранения](docs/decisions/0005-native-block-retention.md), [профили и загрузки](docs/decisions/0006-storage-profiles.md).

Нода автоматически убирает свои незавершённые временные загрузки истории. Полный скачанный файл сохраняется, пока обязательная история не проверена до конца, поэтому после остановки можно повторить прежнюю команду восстановления. После успешной проверки этот временный кэш удаляется. Исходный файл, явно переданный пользователем вне служебного кэша базы, остаётся на месте.
