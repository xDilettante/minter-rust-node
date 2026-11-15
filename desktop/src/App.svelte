<script lang="ts">
  import { onMount } from "svelte";
  import { listen } from "@tauri-apps/api/event";
  import {
    Activity,
    ArrowDownToLine,
    ArrowUpRight,
    ChevronRight,
    CircleHelp,
    Database,
    FolderOpen,
    HardDrive,
    Layers,
    List,
    Monitor,
    Play,
    Radio,
    RotateCw,
    Settings2,
    ShieldCheck,
    Square,
    Terminal,
    X,
  } from "@lucide/svelte";
  import { api, type Settings, type Status } from "./api";

  let settings = $state<Settings | null>(null);
  let saved = $state<Settings | null>(null);
  let status = $state<Status>({
    phase: "stopped",
    managed: false,
    controllable: false,
    controlId: null,
    node: null,
    error: null,
    previousError: null,
    freeBytes: null,
  });
  let page = $state<"overview" | "settings" | "logs">("overview");
  let busy = $state(false);
  let error = $state("");
  let pollError = $state("");
  let notice = $state("");
  let log = $state("");
  let peerText = $state("");
  let refreshed = $state("");
  let polling = false;
  let mounted = true;

  const labels = {
    stopped: "Остановлена",
    starting: "Запускается",
    running: "Работает",
    syncing: "Синхронизируется",
    attached: "Подключена локально",
    error: "Ошибка ноды",
  };
  const modeName = $derived(
    (saved ?? settings)?.mode === "full" ? "Полная нода" : "Лёгкий клиент",
  );
  const configured = $derived(
    saved?.mode === "full"
      ? saved.fullStart === "resume" ||
          Boolean(saved.genesis || saved.checkpoint)
      : Boolean(saved?.primary && saved.witness),
  );
  const active = $derived(status.controllable || status.phase === "attached");
  const healthy = $derived(
    ["running", "attached"].includes(status.phase) &&
      !status.node?.stale &&
      !status.error,
  );
  const height = $derived(
    status.node
      ? new Intl.NumberFormat("ru-RU").format(BigInt(status.node.height))
      : "—",
  );

  function size(bytes: number | null) {
    if (bytes === null) return "—";
    const unit = bytes >= 1024 ** 3 ? "ГиБ" : "МиБ";
    return `${(bytes / 1024 ** (unit === "ГиБ" ? 3 : 2)).toLocaleString("ru-RU", { maximumFractionDigits: 1 })} ${unit}`;
  }

  async function poll() {
    if (polling || !mounted) return;
    polling = true;
    try {
      const [current, lines] = await Promise.all([api.status(), api.logs()]);
      if (!mounted) return;
      status = current;
      log = lines;
      pollError = "";
      refreshed = new Date().toLocaleTimeString("ru-RU");
    } catch (reason) {
      if (mounted) pollError = String(reason);
    } finally {
      polling = false;
    }
  }

  async function action(work: () => Promise<void>) {
    busy = true;
    error = "";
    notice = "";
    try {
      await work();
    } catch (reason) {
      error = String(reason);
    } finally {
      busy = false;
      await poll();
    }
  }

  async function save() {
    if (!settings) return;
    settings.peers = peerText
      .split(/\r?\n/)
      .map((peer) => peer.trim())
      .filter(Boolean);
    const value = $state.snapshot(settings);
    await api.save(value);
    saved = structuredClone(value);
    notice = "Настройки сохранены";
  }

  async function start() {
    if (!configured) {
      page = "settings";
      return;
    }
    await action(() => api.start());
  }

  async function restart() {
    const target = status.controlId;
    await action(async () => {
      await api.stop(target);
      await api.start();
    });
  }

  async function choose(
    field: "dataDir" | "checkpoint" | "genesis" | "lightAnchor",
    directory: boolean,
  ) {
    try {
      const path = await api.choose(directory);
      if (path && settings) settings[field] = path;
    } catch (reason) {
      error = String(reason);
    }
  }

  onMount(() => {
    mounted = true;
    let unlisten: (() => void) | undefined;
    void listen<string>("node-control-error", (event) => {
      error = event.payload;
    }).then((dispose) => {
      if (mounted) unlisten = dispose;
      else dispose();
    });
    void (async () => {
      try {
        const loaded = await api.settings();
        settings = loaded;
        saved = structuredClone(loaded);
        peerText = loaded.peers.join("\n");
        await poll();
      } catch (reason) {
        error = String(reason);
      }
    })();
    const timer = setInterval(() => {
      void poll();
    }, 2500);
    return () => {
      mounted = false;
      unlisten?.();
      clearInterval(timer);
    };
  });
</script>

<svelte:head><title>Minter Node</title></svelte:head>

<div class="app">
  <aside class="sidebar">
    <a
      class="brand"
      href="#overview"
      onclick={() => (page = "overview")}
      aria-label="Minter Node, обзор"
    >
      <span class="mark">M</span><span
        >Minter<span class="brand-sub">Node</span></span
      >
    </a>
    <div class="workspace">
      <span class="workspace-icon"><Radio size={17} /></span>
      <div>Локальная нода<small>{modeName}</small></div>
    </div>
    <nav aria-label="Разделы">
      <button
        class:chosen={page === "overview"}
        onclick={() => (page = "overview")}><Activity size={19} />Обзор</button
      >
      <button
        class:chosen={page === "settings"}
        onclick={() => (page = "settings")}
        ><Settings2 size={19} />Настройки</button
      >
      <button class:chosen={page === "logs"} onclick={() => (page = "logs")}
        ><Terminal size={19} />Журнал</button
      >
    </nav>
    <div class="sidebar-bottom">
      <div class="local-note">
        <Monitor size={17} /><span>Работает на вашем устройстве</span>
      </div>
      <a
        class="docs"
        href="https://www.minter.network/docs"
        target="_blank"
        rel="noreferrer"
        ><CircleHelp size={17} />Документация<ArrowUpRight size={14} /></a
      >
      <button
        class="exit"
        onclick={() =>
          action(() => api.exit(status.controlId, status.phase === "error"))}
        disabled={busy}
        >{status.controllable ? "Остановить ноду и выйти" : "Выйти"}</button
      >
      <span class="version">Minter Node · 0.1.0</span>
    </div>
  </aside>

  <main>
    <header class="topbar">
      <span
        >Моя нода <ChevronRight size={14} />
        <strong
          >{page === "overview"
            ? "Обзор"
            : page === "settings"
              ? "Настройки"
              : "Журнал"}</strong
        ></span
      ><span class="local-pill"
        ><span class="dot"></span>Локальное управление</span
      >
    </header>
    <div class="content">
      {#if error || pollError || status.error}
        <div class="alert" role="alert">
          <span>{error || pollError || status.error}</span><button
            aria-label="Скрыть сообщение"
            onclick={() => {
              error = "";
              pollError = "";
            }}><X size={17} /></button
          >
        </div>
      {/if}
      {#if status.previousError}<div class="notice" role="status">
          <CircleHelp size={17} /><span
            >Предыдущий локальный запуск: {status.previousError}</span
          >
        </div>{/if}
      {#if notice}<div class="notice" role="status">
          <ShieldCheck size={17} />{notice}
        </div>{/if}
      {#if page === "overview"}
        <div class="heading">
          <div>
            <p class="eyebrow">ВАША ЧАСТЬ СЕТИ</p>
            <h1>Обзор ноды</h1>
            <p>Состояние сети и проверка блоков на вашем устройстве.</p>
          </div>
          <button class="secondary" onclick={() => (page = "settings")}
            ><Settings2 size={16} />Настроить</button
          >
        </div>
        <section class="node-panel" aria-label="Состояние ноды">
          <div class="node-panel-top">
            <div class="node-symbol"><Layers size={26} /></div>
            <div class="node-description">
              <h2>{modeName}</h2>
              <p>{status.node?.network || "Minter · minter-mainnet-5"}</p>
            </div>
            <span class:healthy class="status-pill"
              ><span class="dot"></span>{labels[status.phase]}</span
            >
          </div>
          <div class="node-summary">
            <div>
              <p class="field-label">
                {saved?.mode === "light"
                  ? "ПРОВЕРЕННЫЙ ЗАГОЛОВОК"
                  : "ВЫСОТА БЛОКА"}
              </p>
              <div class="height" aria-live="polite">{height}</div>
              <p class="block-time">
                {status.node?.blockTime
                  ? new Date(status.node.blockTime).toLocaleString("ru-RU")
                  : "Появится после подключения к сети"}
              </p>
            </div>
            <div class="node-actions">
              {#if status.controllable}
                <button class="secondary" disabled={busy} onclick={restart}
                  ><RotateCw size={16} />Перезапустить</button
                >
                <button
                  class="secondary"
                  disabled={busy}
                  onclick={() => action(() => api.stop(status.controlId))}
                  ><Square size={14} />Остановить</button
                >
              {:else if status.phase === "attached"}
                <span class="external-note"
                  >Доступно наблюдение. Для остановки используйте приложение,
                  запустившее ноду.</span
                >
              {:else}
                <button
                  class="primary"
                  disabled={busy || !settings}
                  onclick={start}
                  ><Play size={16} fill="currentColor" />{busy
                    ? "Запускается…"
                    : configured
                      ? "Запустить ноду"
                      : "Настроить и запустить"}</button
                >
              {/if}
            </div>
          </div>
          <div class="verification">
            <ShieldCheck size={17} /><span
              >{status.node?.stale
                ? "Срок доверия истёк. Остановите клиент и выберите новую папку данных с новой доверенной точкой."
                : saved?.mode === "light"
                  ? "Подписи заголовков и наборы валидаторов проверяются локально."
                  : "Блоки исполняются локально с проверкой подписей и состояния."}</span
            >
          </div>
        </section>
        <div class="detail-grid">
          <section class="detail">
            <div class="section-icon"><HardDrive size={19} /></div>
            <h2>Хранилище</h2>
            <dl>
              <div>
                <dt>Режим</dt>
                <dd>
                  {saved?.mode === "light"
                    ? "Заголовки · redb"
                    : saved?.storageProfile === "archive"
                      ? "Архив · RocksDB"
                      : saved?.storageProfile === "full"
                        ? "Полная с очисткой · RocksDB"
                        : "Свои лимиты · RocksDB"}
                </dd>
              </div>
              <div>
                <dt>Свободно на диске</dt>
                <dd>{size(status.freeBytes)}</dd>
              </div>
              {#if saved?.mode === "full"}<div>
                  <dt>Версии состояния</dt>
                  <dd>
                    {saved.storageProfile === "archive"
                      ? "Все"
                      : saved.storageProfile === "full"
                        ? 120
                        : saved.keepVersions || "Все"}
                  </dd>
                </div>
                <div>
                  <dt>Блоки / события</dt>
                  <dd>
                    {saved.storageProfile === "archive"
                      ? "Все / все"
                      : saved.storageProfile === "full"
                        ? "100 000 / 100 000"
                        : `${saved.keepNativeBlocks || "Все"} / ${saved.keepEventBlocks || "все"}`}
                  </dd>
                </div>{/if}
            </dl>
            <p class="data-path" title={saved?.dataDir}>
              <FolderOpen size={14} />{saved?.dataDir ||
                "Выберите папку в настройках"}
            </p>
          </section>
          <section class="detail">
            <div class="section-icon"><ShieldCheck size={19} /></div>
            <h2>Проверка сети</h2>
            <dl>
              <div>
                <dt>Подключение</dt>
                <dd>
                  {status.phase === "syncing"
                    ? "Догоняет сеть"
                    : status.node && active
                      ? "Активно"
                      : "Ожидает запуска"}
                </dd>
              </div>
              <div>
                <dt>Источник</dt>
                <dd>
                  {saved?.mode === "light"
                    ? "Основной RPC + свидетель"
                    : "Tendermint P2P"}
                </dd>
              </div>
              <div>
                <dt>Последний опрос</dt>
                <dd>{refreshed || "—"}</dd>
              </div>
            </dl>
            <p class="hash" title={status.node?.appHash}>
              AppHash <span
                >{status.node?.appHash
                  ? `${status.node.appHash.slice(0, 14)}…${status.node.appHash.slice(-8)}`
                  : "—"}</span
              >
            </p>
          </section>
        </div>
        {#if !configured && !active}
          <section class="setup-note">
            <div class="section-icon"><ArrowDownToLine size={21} /></div>
            <div>
              <h2>Подготовьте первый запуск</h2>
              <p>
                Выберите режим и папку данных. Для лёгкого клиента добавьте
                доверенную точку и два RPC. Полная нода может начать с
                проверенного снимка.
              </p>
            </div>
            <button class="text-button" onclick={() => (page = "settings")}
              >Открыть настройки<ChevronRight size={17} /></button
            >
          </section>
        {/if}
        <section class="recent">
          <div class="section-heading">
            <h2>Журнал ноды</h2>
            <button class="text-button" onclick={() => (page = "logs")}
              >Весь журнал<ChevronRight size={16} /></button
            >
          </div>
          {#if log}<pre class="log-preview">{log
                .split("\n")
                .slice(-4)
                .join("\n")}</pre>{:else}<div class="empty-log">
              <List size={18} /><span
                >Здесь появятся сообщения запуска и синхронизации.</span
              >
            </div>{/if}
        </section>
      {:else if page === "settings" && settings}
        <div class="heading">
          <div>
            <p class="eyebrow">ЛОКАЛЬНАЯ КОНФИГУРАЦИЯ</p>
            <h1>Настройки ноды</h1>
            <p>Сохраняются на этом устройстве и применяются при запуске.</p>
          </div>
        </div>
        {#if active}<div class="info">
            Для изменения параметров остановите управляемую ноду. Состояние
            работающей ноды продолжает отображаться в обзоре.
          </div>{/if}
        <form
          onsubmit={(event) => {
            event.preventDefault();
            void action(save);
          }}
        >
          <fieldset disabled={status.controllable || busy}>
            <legend>Режим работы</legend>
            <div class="mode-grid">
              <label
                class:selected={settings.mode === "light"}
                class="mode-option"
                ><input
                  type="radio"
                  name="mode"
                  value="light"
                  bind:group={settings.mode}
                /><Radio size={21} /><strong>Лёгкий клиент</strong><span
                  >Проверяет заголовки и доказательства. Хранит ограниченный
                  кэш.</span
                ></label
              >
              <label
                class:selected={settings.mode === "full"}
                class="mode-option"
                ><input
                  type="radio"
                  name="mode"
                  value="full"
                  bind:group={settings.mode}
                /><Database size={21} /><strong>Полная нода</strong><span
                  >Исполняет блоки и хранит актуальное состояние блокчейна.</span
                ></label
              >
            </div>
            <label class="form-label" for="data-dir">Папка данных</label>
            <div class="input-path">
              <input
                id="data-dir"
                bind:value={settings.dataDir}
                required
                spellcheck="false"
              /><button
                type="button"
                class="icon-button"
                title="Выбрать папку данных"
                onclick={() => choose("dataDir", true)}
                ><FolderOpen size={18} /></button
              >
            </div>
            {#if settings.mode === "light"}
              <h2 class="form-section">Источники проверки</h2>
              <p class="help">
                Укажите RPC независимых операторов и JSON доверенного заголовка.
              </p>
              <label class="form-label" for="anchor">Доверенная точка</label>
              <div class="input-path">
                <input
                  id="anchor"
                  bind:value={settings.lightAnchor}
                  placeholder="trust.json"
                  spellcheck="false"
                /><button
                  type="button"
                  class="icon-button"
                  title="Выбрать доверенную точку"
                  onclick={() => choose("lightAnchor", false)}
                  ><FolderOpen size={18} /></button
                >
              </div>
              <p class="help">
                При первом запуске в новой папке клиент проверит выбранный
                заголовок. Для обновления доверия выберите новую папку данных.
              </p>
              <div class="form-grid">
                <label
                  >Основной Tendermint RPC<input
                    type="url"
                    bind:value={settings.primary}
                    placeholder="https://rpc.example.org:26657"
                  /></label
                ><label
                  >RPC-свидетель<input
                    type="url"
                    bind:value={settings.witness}
                    placeholder="https://witness.example.org:26657"
                  /></label
                >
              </div>
            {:else}
              <h2 class="form-section">Начальное состояние</h2>
              <label class="form-label" for="bootstrap">Способ запуска</label
              ><select id="bootstrap" bind:value={settings.fullStart}
                ><option value="checkpoint"
                  >Восстановить из пакета со снимком</option
                ><option value="resume">Продолжить существующую базу</option
                ><option value="genesis">С начала эпохи, из genesis</option
                ></select
              >
              {#if settings.fullStart === "checkpoint"}
                <label class="form-label" for="checkpoint"
                  >Папка пакета со снимком</label
                >
                <div class="input-path">
                  <input
                    id="checkpoint"
                    bind:value={settings.checkpoint}
                    spellcheck="false"
                  /><button
                    type="button"
                    class="icon-button"
                    title="Выбрать пакет со снимком"
                    onclick={() => choose("checkpoint", true)}
                    ><FolderOpen size={18} /></button
                  >
                </div>
                <label class="form-label" for="digest"
                  >Доверенный SHA-256 манифеста</label
                ><input
                  id="digest"
                  class="mono"
                  bind:value={settings.checkpointSha256}
                  maxlength="64"
                  spellcheck="false"
                />
                <p class="help">
                  Возьмите контрольную сумму из проверенного источника пакета.
                </p>
              {:else if settings.fullStart === "genesis"}
                <label class="form-label" for="genesis">Файл genesis.json</label
                >
                <div class="input-path">
                  <input
                    id="genesis"
                    bind:value={settings.genesis}
                    spellcheck="false"
                  /><button
                    type="button"
                    class="icon-button"
                    title="Выбрать genesis"
                    onclick={() => choose("genesis", false)}
                    ><FolderOpen size={18} /></button
                  >
                </div>
              {/if}
              <label class="form-label" for="peers"
                >P2P-пиры, по одному адресу в строке</label
              ><textarea
                id="peers"
                bind:value={peerText}
                rows="3"
                placeholder="ID@IP:26656"
                spellcheck="false"></textarea>
              <h2 class="form-section">Хранение и локальные порты</h2>
              <label class="form-label" for="storage-profile"
                >Профиль хранения</label
              >
              <select
                id="storage-profile"
                aria-describedby="storage-description"
                bind:value={settings.storageProfile}
              >
                <option value="full">Полная с очисткой — рекомендуется</option>
                <option value="archive"
                  >Архивная — сохранять дальнейшую историю</option
                >
                <option value="custom">Свои лимиты</option>
              </select>
              <p class="help" id="storage-description">
                {settings.storageProfile === "full"
                  ? "120 версий состояния, 100 000 блоков и высот событий. Старые данные удаляются по мере работы."
                  : settings.storageProfile === "archive"
                    ? "Состояние, блоки и события сохраняются без очистки. Расход диска растёт со временем."
                    : "Настройте каждый лимит отдельно. Ноль сохраняет все дальнейшие данные."}
                Профиль не восстанавливает удалённую историю. Снимок задаёт начальную
                границу данных.
              </p>
              {#if settings.storageProfile === "custom"}
                <div class="form-grid">
                  <label
                    >Версий состояния<input
                      id="keep-versions"
                      type="number"
                      min="0"
                      step="1"
                      bind:value={settings.keepVersions}
                    /></label
                  >
                  <label
                    >Полных блоков<input
                      id="keep-native-blocks"
                      type="number"
                      min="0"
                      step="1"
                      bind:value={settings.keepNativeBlocks}
                    /></label
                  >
                  <label
                    >Высот событий<input
                      id="keep-event-blocks"
                      type="number"
                      min="0"
                      step="1"
                      bind:value={settings.keepEventBlocks}
                    /></label
                  >
                </div>
              {/if}
              <div class="form-grid">
                <label
                  >Порт HTTP API<input
                    type="number"
                    min="1"
                    max="65535"
                    bind:value={settings.apiPort}
                  /></label
                ><label
                  >Порт P2P<input
                    type="number"
                    min="1"
                    max="65535"
                    bind:value={settings.p2pPort}
                  /></label
                >
              </div>
              <p class="help">
                API и P2P слушают 127.0.0.1. Фактически доступная история
                зависит от исходного снимка и ранее применявшихся лимитов.
              </p>
            {/if}
          </fieldset>
          <div class="form-actions">
            <button
              class="primary"
              type="submit"
              disabled={status.controllable || busy}
              >{busy ? "Сохраняется…" : "Сохранить настройки"}</button
            ><button
              type="button"
              class="secondary"
              onclick={() => (page = "overview")}>К обзору</button
            >
          </div>
        </form>
      {:else if page === "logs"}
        <div class="heading">
          <div>
            <p class="eyebrow">ДИАГНОСТИКА</p>
            <h1>Журнал ноды</h1>
            <p>Последние 120 строк. Файлы журнала автоматически ротируются.</p>
          </div>
          <button class="secondary" onclick={() => poll()}
            ><RotateCw size={16} />Обновить</button
          >
        </div>
        <textarea
          class="log-full"
          readonly
          aria-label="Сообщения ноды"
          value={log || "Нода ещё не записала сообщения."}></textarea>
      {/if}
      <footer>
        <span class="footer-mark">M</span><span
          >Закрытие окна оставляет ноду в трее.</span
        ><span class="footer-right">Настройки и данные хранятся локально</span>
      </footer>
    </div>
  </main>
</div>
