import init, { WoodlandApp } from './pkg/woodland.js';

const WORLD = new URL('./world.json', import.meta.url);
const SERVER_SETTING = document.querySelector('meta[name="woodland-server"]')?.content || '';
const SERVER_URL = SERVER_SETTING === 'self'
  ? location.origin
  : SERVER_SETTING.replace(/\/+$/, '');
const STORAGE_SCOPE = location.origin;
const WALLET_WRITER_LOCK = `woodland.sh:web:wallet-writer:${STORAGE_SCOPE}`;
const KEY = `woodland.sh:web:v2:key:${STORAGE_SCOPE}`;
const PROFILE = `woodland.sh:web:v2:profile:${STORAGE_SCOPE}`;
let profileStorageKey = PROFILE;
let pendingStorageKey = `woodland.sh:web:v2:pending:${STORAGE_SCOPE}`;
const POSITION = `woodland.sh:web:v2:position:${STORAGE_SCOPE}`;
const RESTORE_JOURNAL = `woodland.sh:web:v2:restore:${STORAGE_SCOPE}`;
const PLAYER_BACKUP_FORMAT = 'woodland.sh/player-backup';
const PLAYER_BACKUP_VERSION = 1;
const MAX_PLAYER_BACKUP_BYTES = 64 * 1024;

const element = (id) => document.getElementById(id);
const address = element('address');
const copyAddressButton = element('copy-address');
const fundingInstructionElement = element('funding-instruction');
const walletSats = element('wallet-sats');
const forestHealth = element('forest-health');
const playerLogs = element('player-logs');
const logSlot = element('log-slot');
const playerStone = element('player-stone');
const stoneSlot = element('stone-slot');
const playerIronOre = element('player-iron-ore');
const ironOreSlot = element('iron-ore-slot');
const playerAxe = element('player-axe');
const axeSlot = element('axe-slot');
const playerState = element('player-state');
const playerSession = element('player-session');
const xpBacking = element('xp-backing');
const logChance = element('log-chance');
const treeState = element('tree-state');
const emulator = element('emulator');
const status = element('status');
const log = element('log');
const walletHeading = element('wallet-heading');
const onboardingNote = element('onboarding-note');
const dashboard = element('dashboard');
const bagPanel = element('bag-panel');
const statsPanel = element('stats-panel');
const levelNumber = element('level-number');
const xpNumber = element('xp-number');
const xpNext = element('xp-next');
const map = element('map');
const mapViewport = element('map-viewport');
const cameraPlayer = element('camera-player');
const mapHud = element('map-hud');
const hudLevel = element('hud-level');
const hudXp = element('hud-xp');
const hudLogs = element('hud-logs');
const hudOnline = element('hud-online');
const hudPosition = element('hud-position');
const mapHint = element('map-hint');
const refreshButton = element('refresh');
const renewButton = element('renew-player');
const craftAxeButton = element('craft-axe');
const axeRecipe = element('axe-recipe');
const activateButton = element('activate');
const resetProfileButton = element('reset-profile');
const resetButton = element('reset');
const details = element('details');
const leaderboardPanel = element('leaderboard-panel');
const leaderboardRows = element('leaderboard-rows');
const leaderboardStatus = element('leaderboard-status');
const delegateRenewalButton = element('delegate-renewal');
const chatForm = element('chat-form');
const chatInput = element('chat-input');
const sendChatButton = element('send-chat');
const chatMessagesElement = element('chat-messages');
const chatStatus = element('chat-status');
const downloadBackupButton = element('download-backup');
const restoreBackupButton = element('restore-backup');
const restoreBackupFile = element('restore-backup-file');
const backupStatus = element('backup-status');

const DEFAULT_MAP_WIDTH = 425;
const DEFAULT_MAP_HEIGHT = 425;
const WALK_STEP_MS = 90;
const CHOP_FLASH_MS = 340;
const CHOP_CADENCE_MS = 1_000;
const CHOP_FEEDBACK_MS = 1_000;
const LOCATION_POST_INTERVAL_MS = 750;
const LOCATION_HEARTBEAT_MS = 30_000;
const DELEGATION_FRESHNESS_MS = 45_000;
// Leave at least 30 minutes for owner-funded renewal on long-lived batches,
// even if the watchtower keeps answering HTTP while making no progress.
const OWNER_RENEWAL_FALLBACK_SECONDS = 1_800;
const LOG_FLASH_MS = 800;
const DIRECTIONS = [[0, -1], [-1, 0], [1, 0], [0, 1]];
const TREE_GLYPH = '🌲';
const TILE_SIZE = 20;
const MAX_PATH_NODES = 262_144;
const AXE_NAMES = Object.freeze({
  none: 'No Axe',
  wooden: 'Wooden Axe',
  stone: 'Stone Axe',
  iron: 'Iron Axe',
});
const player = { x: 3, y: 17 };

let app;
let state;
let walletWriterOwned = false;
let walletWriterPromise;
let busy = false;
let polling = false;
let chopping = false;
let stopChopping = false;
let walking = false;
let walkGeneration = 0;
let walkingTarget = null;
let treeEffect = null;
let treeEffectTimer = null;
let treeEffectSerial = 0;
let walkingTargetTreeId = null;
let lockedTreeId = null;
let lastChopRun = null;
let copyResetTimer = null;
let focusedTreeId = null;
let leaderboardTotal = 0;
let appQueue = Promise.resolve();
let leaderboardPlayers = [];
let serverRegistered = false;
let delegatedPlayerAssets = [];
let presenceTruncated = false;
let serverRegistrationSyncing = false;
let serverRegistrationOutpoint = null;
let serverRegistrationRetryAfter = 0;
let remoteLocations = [];
let chatMessages = [];
let delegatedRenewal = false;
let delegationAvailable = false;
let delegationObservedAt = null;
let lastRenewalError = null;
let socialPosting = false;
let locationPosting = false;
let lastPublishedLocation = null;
let lastPublishedLocationAt = 0;
let nextLocationPostAt = 0;
let lastRenderedChatId = null;
let mapFrame = null;
let worldManifest = null;
let treeLayout = [];
const treeViews = new Map();
let treeViewportOverride = null;

let pendingRetryAfter = 0;
let nextWorldRefreshAt = 0;

function acquireWalletWriter() {
  if (walletWriterOwned) return Promise.resolve();
  if (walletWriterPromise) return walletWriterPromise;
  if (!globalThis.navigator?.locks?.request) {
    return Promise.reject(new Error('This browser cannot safely coordinate the player wallet. Open the game in a browser with Web Locks support over HTTPS or localhost.'));
  }
  walletWriterPromise = new Promise((resolve, reject) => {
    // Hold the lock until this document is destroyed. This also covers Rust's
    // localStorage journal writes, bootstrap, restores, and direct WASM calls.
    // A second tab must never initialize a stale copy of the signing wallet.
    navigator.locks.request(WALLET_WRITER_LOCK, { ifAvailable: true }, async (lock) => {
      if (!lock) {
        reject(new Error('This player wallet is already open in another tab. Close that tab, then press Refresh here.'));
        return;
      }
      walletWriterOwned = true;
      resolve();
      await new Promise(() => {});
    }).catch(reject);
  }).catch((error) => {
    walletWriterPromise = undefined;
    throw error;
  });
  return walletWriterPromise;
}

function requireWalletWriter() {
  if (!walletWriterOwned) throw new Error('This tab does not control the player wallet. Press Refresh after closing the other game tab.');
  if (localStorage.getItem(RESTORE_JOURNAL)) {
    throw new Error('A wallet restore is unfinished. Press Refresh to recover it before using the player.');
  }
}

function hasPendingWalletTransaction() {
  if (state?.pendingChopTxid || state?.pendingActivationTxid) return true;
  // Submission may have saved a journal and thrown before a new WASM snapshot
  // reached the UI. Never allow key replacement based only on rendered state.
  try {
    if (localStorage.getItem(RESTORE_JOURNAL) || localStorage.getItem(pendingStorageKey)) return true;
    const profile = JSON.parse(localStorage.getItem(profileStorageKey) || 'null');
    return Boolean(profile?.pendingActivation);
  } catch {
    return true;
  }
}

function recoverWalletRestore() {
  const encoded = localStorage.getItem(RESTORE_JOURNAL);
  if (!encoded) return;
  const journal = JSON.parse(encoded);
  const profile = typeof journal.profile === 'string' ? JSON.parse(journal.profile) : null;
  if (
    journal.version !== 1
    || !/^[0-9a-f]{64}$/u.test(journal.genesisTxid || '')
    || !/^[0-9a-f]{64}$/u.test(journal.secretKey || '')
    || profile?.genesisTxid !== journal.genesisTxid
    || typeof journal.pendingKey !== 'string'
    || !journal.pendingKey.startsWith('woodland.sh:web:v2:pending:')
    || !journal.pendingKey.endsWith(`:${journal.genesisTxid}`)
    || (journal.position !== null && typeof journal.position !== 'string')
  ) {
    throw new Error('Saved wallet restore journal is invalid; preserve browser storage for recovery.');
  }
  const restoredProfileKey = `${PROFILE}:${journal.genesisTxid}`;
  // Keep the complete target in one atomic localStorage entry until every
  // replacement succeeds. Free the old entries first to minimize quota use.
  for (const key of [KEY, PROFILE, restoredProfileKey, journal.pendingKey, POSITION]) {
    localStorage.removeItem(key);
  }
  localStorage.setItem(KEY, journal.secretKey);
  localStorage.setItem(restoredProfileKey, journal.profile);
  if (journal.position !== null) localStorage.setItem(POSITION, journal.position);
  localStorage.removeItem(RESTORE_JOURNAL);
}

function withApp(action) {
  const invoke = () => {
    requireWalletWriter();
    syncAppTreeViewport();
    return action();
  };
  const operation = appQueue.then(invoke, invoke);
  appQueue = operation.catch(() => {});
  return operation;
}

function appendLog(message) {
  const time = new Date().toLocaleTimeString();
  log.textContent = `[${time}] ${message}\n${log.textContent}`.slice(0, 8000);
}

function axeName(tier) {
  return AXE_NAMES[tier] || 'Unknown Axe';
}

function axeRecipeSummary(recipe) {
  if (!recipe) return 'Highest axe tier crafted';
  const costs = [
    recipe.logCost ? `${recipe.logCost} LOG` : '',
    recipe.stoneCost ? `${recipe.stoneCost} STONE` : '',
    recipe.ironOreCost ? `${recipe.ironOreCost} IRON ORE` : '',
  ].filter(Boolean);
  const requirement = recipe.axe === 'wooden'
    ? 'First successful chop (25 XP)'
    : `Level ${recipe.requiredLevel}`;
  return `${requirement} · ${costs.join(' + ')}`;
}

function setBusy(value, message = '') {
  busy = value;
  if (message) status.textContent = message;
  renderSocialControls();
  render();
}

function renderSocialControls() {
  const playerActive = Boolean(state?.playerActive);
  leaderboardPanel.hidden = !SERVER_URL;
  delegateRenewalButton.hidden = !playerActive || !serverRegistered || !delegationAvailable;
  delegateRenewalButton.disabled = busy || socialPosting;
  delegateRenewalButton.textContent = delegatedRenewal
    ? 'Stop delegated renewals'
    : 'Delegate renewals';
  chatInput.disabled = !playerActive || !serverRegistered || socialPosting;
  sendChatButton.disabled = chatInput.disabled || !chatInput.value.trim();
}


function renderLeaderboard() {
  if (!SERVER_URL) return;
  leaderboardRows.replaceChildren();
  if (!leaderboardPlayers.length) {
    const row = document.createElement('tr');
    const cell = document.createElement('td');
    cell.colSpan = 5;
    cell.textContent = 'No verified players yet.';
    row.append(cell);
    leaderboardRows.append(row);
    return;
  }
  leaderboardPlayers.forEach((entry, index) => {
    const row = document.createElement('tr');
    const values = [
      String(index + 1),
      `${entry.playerAsset.slice(0, 10)}…${entry.playerAsset.slice(-6)}${entry.playerAsset === state?.playerAsset ? ' (you)' : ''}`,
      String(entry.xp),
      String(entry.level),
      entry.active ? 'active' : 'expired',
    ];
    values.forEach((value, column) => {
      const cell = document.createElement('td');
      cell.textContent = value;
      if (column === 0) cell.className = 'rank';
      if (column === 2 || column === 3) cell.className = 'score';
      row.append(cell);
    });
    leaderboardRows.append(row);
  });
}

function renderChat() {
  chatMessagesElement.replaceChildren();
  if (!serverRegistered) {
    chatMessagesElement.textContent = state?.playerActive
      ? 'Registering player with server...'
      : 'Create a player to use chat.';
    return;
  }
  if (!chatMessages.length) {
    chatMessagesElement.textContent = 'No messages yet.';
    return;
  }
  for (const entry of chatMessages) {
    const row = document.createElement('div');
    row.className = 'chat-message';
    const playerName = document.createElement('span');
    playerName.className = 'chat-player';
    playerName.textContent = entry.playerAsset === state?.playerAsset
      ? 'you'
      : `${entry.playerAsset.slice(0, 8)}…`;
    const message = document.createElement('span');
    message.textContent = entry.message;
    row.append(playerName, message);
    chatMessagesElement.append(row);
  }
  const newest = chatMessages.at(-1)?.id ?? null;
  if (newest !== lastRenderedChatId) {
    chatMessagesElement.scrollTop = chatMessagesElement.scrollHeight;
    lastRenderedChatId = newest;
  }
}

function publishSocialSnapshot() {
  globalThis.__WOODLAND_E2E_LEADERBOARD = leaderboardPlayers;
  globalThis.__WOODLAND_E2E_SOCIAL = {
    locations: remoteLocations,
    messages: chatMessages,
    delegatedPlayerAssets,
    delegationAvailable,
    truncated: presenceTruncated,
  };
}

function nearbyPlayerCount() {
  return remoteLocations.filter((location) => location.playerAsset !== state?.playerAsset).length;
}

function updateServerStatus() {
  const nearby = `${nearbyPlayerCount()}${presenceTruncated ? '+' : ''} nearby`;
  leaderboardStatus.textContent = `${leaderboardTotal} verified · ${nearby}`;
}

function currentPresenceBounds() {
  const mapWidth = state?.mapWidth || worldManifest?.mapWidth;
  const mapHeight = state?.mapHeight || worldManifest?.mapHeight;
  if (!mapWidth || !mapHeight) return null;
  const halfWidth = Math.ceil((map.clientWidth || mapViewport.clientWidth) / TILE_SIZE / 2) + 4;
  const halfHeight = Math.ceil((map.clientHeight || mapViewport.clientHeight) / TILE_SIZE / 2) + 4;
  const focusX = state?.playerActive ? player.x : Math.floor(mapWidth / 2);
  const focusY = state?.playerActive ? player.y : Math.floor(mapHeight / 2);
  return {
    minX: Math.max(0, focusX - halfWidth),
    minY: Math.max(0, focusY - halfHeight),
    maxX: Math.min(mapWidth - 1, focusX + halfWidth),
    maxY: Math.min(mapHeight - 1, focusY + halfHeight),
  };
}

function syncAppTreeViewport() {
  const bounds = treeViewportOverride || currentPresenceBounds();
  if (app && bounds) app.setTreeViewport(bounds.minX, bounds.minY, bounds.maxX, bounds.maxY);
}

function adoptState(nextState) {
  const socialIdentityChanged = state?.playerAsset !== nextState.playerAsset
    || state?.playerActive !== nextState.playerActive;
  for (const tree of nextState.trees || []) treeViews.set(tree.treeId, tree);
  nextState.trees = treeLayout.map((tree) => treeViews.get(tree.treeId) || tree);
  state = nextState;
  if (socialIdentityChanged) {
    delegatedRenewal = delegationAvailable && delegatedPlayerAssets.includes(state.playerAsset);
    renderLeaderboard();
    renderChat();
    renderSocialControls();
  }
  return state;
}

async function refreshPresence() {
  if (!SERVER_URL || !state) return;
  const bounds = currentPresenceBounds();
  const query = new URLSearchParams(bounds);
  try {
    const response = await fetch(`${SERVER_URL}/v1/presence?${query}`, {
      cache: 'no-store',
      mode: 'cors',
    });
    if (!response.ok) throw new Error(`presence returned ${response.status}`);
    const payload = await response.json();
    remoteLocations = Array.isArray(payload.locations) ? payload.locations : [];
    presenceTruncated = payload.truncated === true;
    updateServerStatus();
    publishSocialSnapshot();
    renderMap();
    hudOnline.textContent = String(nearbyPlayerCount());
  } catch (error) {
    remoteLocations = [];
    presenceTruncated = false;
    publishSocialSnapshot();
    renderMap();
    leaderboardStatus.textContent = `Presence unavailable: ${error}`;
  }
}

async function refreshChat() {
  if (!SERVER_URL) return;
  try {
    const response = await fetch(`${SERVER_URL}/v1/chat`, {
      cache: 'no-store',
      mode: 'cors',
    });
    if (!response.ok) throw new Error(`chat returned ${response.status}`);
    const payload = await response.json();
    chatMessages = Array.isArray(payload.messages) ? payload.messages : [];
    publishSocialSnapshot();
    renderChat();
  } catch (error) {
    chatStatus.textContent = `Chat unavailable: ${error}`;
  }
}

async function refreshLeaderboard() {
  const requestedAt = Date.now();
  if (!SERVER_URL) return;
  try {
    const response = await fetch(`${SERVER_URL}/v1/leaderboard?limit=100`, {
      cache: 'no-store',
      mode: 'cors',
    });
    if (!response.ok) throw new Error(`leaderboard returned ${response.status}`);
    const payload = await response.json();
    if (delegationObservedAt != null && requestedAt < delegationObservedAt) return;
    leaderboardPlayers = Array.isArray(payload.players) ? payload.players : [];
    leaderboardTotal = Number(payload.total) || leaderboardPlayers.length;
    delegationAvailable = payload.delegationAvailable === true;
    delegationObservedAt = requestedAt;
    delegatedPlayerAssets = Array.isArray(payload.delegatedPlayerAssets)
      ? payload.delegatedPlayerAssets
      : [];
    delegatedRenewal = delegationAvailable && delegatedPlayerAssets.includes(state?.playerAsset);
    updateServerStatus();
    publishSocialSnapshot();
    renderLeaderboard();
    renderSocialControls();
  } catch (error) {
    if (delegationObservedAt != null && requestedAt < delegationObservedAt) return;
    delegationAvailable = false;
    delegationObservedAt = requestedAt;
    delegatedRenewal = false;
    publishSocialSnapshot();
    renderSocialControls();
    leaderboardStatus.textContent = `Server unavailable: ${error}`;
  }
}

async function syncServerRegistration(force = false) {
  if (
    !SERVER_URL
    || !state?.playerActive
    || !state.playerAsset
    || serverRegistrationSyncing
    || (!force && Date.now() < serverRegistrationRetryAfter)
    || (!force && serverRegistrationOutpoint === state.playerStateOutpoint)
  ) return;
  serverRegistrationSyncing = true;
  serverRegistrationRetryAfter = Date.now() + 15_000;
  try {
    const registration = await withApp(() => app.serverRegistration(SERVER_URL));
    const response = await fetch(`${SERVER_URL}/v1/players`, {
      method: 'POST',
      mode: 'cors',
      cache: 'no-store',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(registration),
    });
    const payload = await response.json().catch(() => ({}));
    if (!response.ok) throw new Error(payload.error || `registration returned ${response.status}`);
    serverRegistrationOutpoint = state.playerStateOutpoint;
    serverRegistered = true;
    renderChat();
    renderSocialControls();
    void Promise.all([refreshLeaderboard(), refreshPresence(), refreshChat()]);
    void publishLocation(true);
    serverRegistrationRetryAfter = 0;
  } finally {
    serverRegistrationSyncing = false;
  }
}

async function postServerAction(path, body) {
  const response = await fetch(`${SERVER_URL}${path}`, {
    method: 'POST',
    mode: 'cors',
    cache: 'no-store',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  const payload = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(payload.error || `server returned ${response.status}`);
  return payload;
}

async function publishLocation(force = false) {
  const now = Date.now();
  if (
    !SERVER_URL
    || !serverRegistered
    || !state?.playerActive
    || locationPosting
    || now < nextLocationPostAt
  ) return;
  const { x, y } = player;
  const location = `${x}:${y}`;
  if (
    !force
    && location === lastPublishedLocation
    && now - lastPublishedLocationAt < LOCATION_HEARTBEAT_MS
  ) return;
  locationPosting = true;
  nextLocationPostAt = now + LOCATION_POST_INTERVAL_MS;
  try {
    const locationRequest = await withApp(() => (
      app.serverLocation(SERVER_URL, x, y, Date.now())
    ));
    await postServerAction('/v1/location', locationRequest);
    lastPublishedLocation = location;
    lastPublishedLocationAt = now;
  } catch (error) {
    leaderboardStatus.textContent = `Location update failed: ${error}`;
  } finally {
    locationPosting = false;
  }
}

async function submitChat(message) {
  if (!SERVER_URL || !serverRegistered || !state?.playerActive || socialPosting) return;
  socialPosting = true;
  renderSocialControls();
  try {
    const chatRequest = await withApp(() => app.serverChat(SERVER_URL, message, Date.now()));
    await postServerAction('/v1/chat', chatRequest);
    chatInput.value = '';
    chatStatus.textContent = '';
    await refreshChat();
  } catch (error) {
    chatStatus.textContent = String(error);
  } finally {
    socialPosting = false;
    renderSocialControls();
  }
}

async function updateDelegation() {
  if (
    !SERVER_URL
    || !serverRegistered
    || !state?.playerActive
    || !delegationAvailable
    || socialPosting
  ) return;
  socialPosting = true;
  renderSocialControls();
  const enabled = !delegatedRenewal;
  try {
    const delegationRequest = await withApp(() => (
      app.serverDelegation(SERVER_URL, enabled, Date.now())
    ));
    await postServerAction('/v1/delegation', delegationRequest);
    delegatedRenewal = enabled;
    delegationObservedAt = Date.now();
    renderSocialControls();
    chatStatus.textContent = enabled
      ? 'Server renewal delegation enabled.'
      : 'Server renewal delegation disabled.';
    await refreshLeaderboard();
  } catch (error) {
    chatStatus.textContent = String(error);
  } finally {
    socialPosting = false;
    renderSocialControls();
  }
}

function worldTrees() {
  return state?.trees || [];
}


function coordinateKey(x, y) {
  return `${x}:${y}`;
}

function isMapCoordinate(x, y) {
  return x >= 0 && y >= 0 && x < state.mapWidth && y < state.mapHeight;
}

function findWalkPath(targets) {
  if (!state) return null;
  const targetKeys = new Set(targets.map(({ x, y }) => coordinateKey(x, y)));
  const startKey = coordinateKey(player.x, player.y);
  if (targetKeys.has(startKey)) return [];
  const blocked = new Set(worldTrees().map((tree) => coordinateKey(tree.x, tree.y)));
  const queue = [{ x: player.x, y: player.y }];
  const previous = new Map([[startKey, null]]);
  let foundKey = null;
  for (
    let index = 0;
    index < queue.length
      && foundKey == null
      && previous.size < Math.min(state.mapWidth * state.mapHeight, MAX_PATH_NODES);
    index += 1
  ) {
    const current = queue[index];
    for (const [dx, dy] of DIRECTIONS) {
      const next = { x: current.x + dx, y: current.y + dy };
      const key = coordinateKey(next.x, next.y);
      if (!isMapCoordinate(next.x, next.y) || blocked.has(key) || previous.has(key)) continue;
      previous.set(key, coordinateKey(current.x, current.y));
      queue.push(next);
      if (targetKeys.has(key)) {
        foundKey = key;
        break;
      }
    }
  }
  if (foundKey == null) return null;
  const path = [];
  for (let key = foundKey; key !== startKey; key = previous.get(key)) {
    const [x, y] = key.split(':').map(Number);
    path.push({ x, y });
  }
  return path.reverse();
}

function cancelWalking() {
  walkGeneration += 1;
  walking = false;
  walkingTarget = null;
  walkingTargetTreeId = null;
}

async function walkTo(targets, treeId = null) {
  if (!state?.playerActive) {
    status.textContent = 'Create a player before moving.';
    return false;
  }
  const path = findWalkPath(targets);
  if (path == null) {
    status.textContent = 'That tile is unreachable.';
    return false;
  }
  const startedAt = performance.now();
  const generation = ++walkGeneration;
  walking = true;
  walkingTarget = targets[0] || null;
  walkingTargetTreeId = treeId;
  render();
  for (const step of path) {
    if (generation !== walkGeneration) return false;
    player.x = step.x;
    player.y = step.y;
    persistPosition();
    render();
    await new Promise((resolve) => setTimeout(resolve, WALK_STEP_MS));
  }
  if (generation !== walkGeneration) return false;
  walking = false;
  walkingTarget = null;
  walkingTargetTreeId = null;
  render();
  globalThis.__WOODLAND_E2E_LAST_WALK = {
    steps: path.length,
    durationMs: Math.round(performance.now() - startedAt),
  };
  void publishLocation();
  return true;
}

async function handleMapPosition(x, y) {
  if (!state || !isMapCoordinate(x, y)) return;
  if (chopping) {
    attemptChop();
    return;
  }
  if (busy) return;
  cancelWalking();
  const tree = worldTrees().find((candidate) => candidate.x === x && candidate.y === y);
  if (tree?.health === 0) {
    focusedTreeId = tree.treeId;
    lockedTreeId = null;
    if (tree.depleted) {
      status.textContent = `Tree #${tree.treeId} has exhausted its local reserve.`;
      render();
      return;
    }
    run(
      `Submitting permissionless regrowth for tree #${tree.treeId}...`,
      () => withApp(() => app.regrow(tree.treeId)),
      () => `Tree #${tree.treeId} regrew with ${worldManifest.activeLogsPerTree} health.`,
    );
    return;
  }
  if (!state.playerActive) {
    status.textContent = 'Create a player before moving.';
    return;
  }
  if (tree) {
    focusedTreeId = tree.treeId;
    lockedTreeId = tree.treeId;
    const destinations = DIRECTIONS
      .map(([dx, dy]) => ({ x: tree.x + dx, y: tree.y + dy }))
      .filter((position) => (
        isMapCoordinate(position.x, position.y)
        && !worldTrees().some((candidate) => (
          candidate.x === position.x && candidate.y === position.y
        ))
      ));
    const movement = walkTo(destinations, tree.treeId);
    const generation = walkGeneration;
    const arrived = await movement;
    if (generation !== walkGeneration) return;
    if (arrived) {
      if (state.fundingReady) {
        attemptChop(tree.treeId);
      } else {
        lockedTreeId = null;
        status.textContent = 'Player state is reconciling.';
        render();
      }
    } else {
      lockedTreeId = null;
      render();
    }
    return;
  }
  lockedTreeId = null;
  await walkTo([{ x, y }]);
}

function handleMapClick(event) {
  if (!mapFrame) return;
  const bounds = map.getBoundingClientRect();
  const x = Math.floor((event.clientX - bounds.left - mapFrame.originX) / TILE_SIZE);
  const y = Math.floor((event.clientY - bounds.top - mapFrame.originY) / TILE_SIZE);
  void handleMapPosition(x, y);
}

function adjacentTree() {
  return worldTrees().find((tree) => (
    Math.abs(player.x - tree.x) + Math.abs(player.y - tree.y) === 1
  )) || null;
}

function focusedTree() {
  const adjacent = adjacentTree();
  if (adjacent) return adjacent;
  return worldTrees().find((tree) => tree.treeId === focusedTreeId) || worldTrees()[0] || null;
}

function flashTree(treeId, type, duration, renderNow = true) {
  const serial = ++treeEffectSerial;
  clearTimeout(treeEffectTimer);
  treeEffect = { treeId, type, serial };
  globalThis.__WOODLAND_E2E_LAST_TREE_EFFECT = { treeId, type };
  const effects = globalThis.__WOODLAND_E2E_TREE_EFFECTS || [];
  effects.push({ treeId, type });
  globalThis.__WOODLAND_E2E_TREE_EFFECTS = effects.slice(-100);
  if (renderNow) renderMap();

  treeEffectTimer = setTimeout(() => {
    if (treeEffect?.serial !== serial) return;
    treeEffect = null;
    renderMap();
  }, duration);
}

function drawTrees(context, visible, screenX, screenY) {
  let standingTreeCount = 0;
  let stumpCount = 0;
  context.textAlign = 'center';
  context.textBaseline = 'middle';
  for (const tree of worldTrees()) {
    if (tree.health > 0) standingTreeCount += 1;
    else stumpCount += 1;
    if (!visible(tree.x, tree.y)) continue;
    const x = screenX(tree.x);
    const y = screenY(tree.y);
    if (treeEffect?.treeId === tree.treeId) {
      context.fillStyle = treeEffect.type === 'log' ? '#6f8f43' : '#607d4d';
      context.fillRect(x, y, TILE_SIZE, TILE_SIZE);
    }
    if (tree.treeId === lockedTreeId) {
      context.strokeStyle = '#f2d475';
      context.lineWidth = 2;
      context.strokeRect(x + 1, y + 1, TILE_SIZE - 2, TILE_SIZE - 2);
      context.lineWidth = 1;
    }
    context.font = tree.health === 0 ? 'bold 14px ui-monospace' : '16px sans-serif';
    context.fillStyle = tree.health === 0 ? '#b58654' : '#dff5d5';
    context.fillText(
      tree.health === 0 ? '+' : TREE_GLYPH,
      x + TILE_SIZE / 2,
      y + TILE_SIZE / 2,
    );
  }
  return { standingTreeCount, stumpCount };
}

function drawRemotePlayers(context, visible, screenX, screenY) {
  const playersByPosition = new Map();
  for (const location of remoteLocations) {
    if (location.playerAsset === state?.playerAsset || !visible(location.x, location.y)) continue;
    const key = coordinateKey(location.x, location.y);
    playersByPosition.set(key, (playersByPosition.get(key) || 0) + 1);
  }
  for (const [key, present] of playersByPosition) {
    const [x, y] = key.split(':').map(Number);
    const centerX = screenX(x) + TILE_SIZE / 2;
    const centerY = screenY(y) + TILE_SIZE / 2;
    context.fillStyle = '#78c9e8';
    context.beginPath();
    context.arc(centerX, centerY, 7, 0, Math.PI * 2);
    context.fill();
    context.fillStyle = '#091014';
    context.font = 'bold 10px ui-monospace';
    context.fillText(present === 1 ? '&' : String(present), centerX, centerY);
  }
  return playersByPosition;
}

function updateMapCamera(originX, originY) {
  cameraPlayer.hidden = !state?.playerActive;
  mapHud.hidden = !state?.playerActive;
  map.classList.toggle('movement-disabled', !state?.playerActive);
  if (state?.playerActive) {
    cameraPlayer.title = `Player at (${player.x}, ${player.y})`;
  }
  const viewportBounds = mapViewport.getBoundingClientRect();
  const playerBounds = cameraPlayer.getBoundingClientRect();
  const camera = state?.playerActive
    ? {
      x: player.x,
      y: player.y,
      mapX: originX,
      mapY: originY,
      deltaX: playerBounds.left
        + playerBounds.width / 2
        - viewportBounds.left
        - viewportBounds.width / 2,
      deltaY: playerBounds.top
        + playerBounds.height / 2
        - viewportBounds.top
        - viewportBounds.height / 2,
    }
    : null;
  globalThis.__WOODLAND_E2E_CAMERA = camera;
  if (camera) {
    const trail = globalThis.__WOODLAND_E2E_CAMERA_TRAIL || [];
    const previous = trail.at(-1);
    if (!previous || previous.x !== camera.x || previous.y !== camera.y) {
      trail.push(camera);
      globalThis.__WOODLAND_E2E_CAMERA_TRAIL = trail.slice(-100);
    }
  }
}

function updateMapHint(adjacent) {
  if (!state) mapHint.textContent = 'Connecting...';
  else if (!worldTrees().length) mapHint.textContent = 'The shared woodland is unavailable.';
  else if (walking) {
    mapHint.textContent = walkingTargetTreeId == null
      ? 'Walking...'
      : `Walking to tree #${walkingTargetTreeId}...`;
  }
  else if (chopping) {
    mapHint.textContent = `Locked on tree #${lockedTreeId}. Auto-swinging until a LOG drops. Click the map to stop after this swing.`;
  }
  else if (state.pendingChopTxid) {
    mapHint.textContent = `Recovering submitted swing ${state.pendingChopTxid.slice(0, 12)}...`;
  }
  else if (adjacent?.health === 0) {
    mapHint.textContent = adjacent.depleted
      ? `Tree #${adjacent.treeId} has exhausted its local reserve.`
      : `Tree #${adjacent.treeId} can regrow in one fresh batch; click it to renew.`;
  }
  else if (!state.playerActive) mapHint.textContent = 'Fund and activate the player.';
  else if (!state.fundingReady) mapHint.textContent = 'Player state is reconciling.';
  else if (adjacent) {
    mapHint.textContent = `In range of tree #${adjacent.treeId}. Click the tree to chop until LOG.`;
  }
  else mapHint.textContent = 'Click a tile to walk, or click a tree to walk there and chop.';
}

function renderMap() {
  const worldWidth = state?.mapWidth || DEFAULT_MAP_WIDTH;
  const worldHeight = state?.mapHeight || DEFAULT_MAP_HEIGHT;
  const canvasWidth = map.clientWidth || mapViewport.clientWidth;
  const canvasHeight = map.clientHeight || mapViewport.clientHeight;
  if (!canvasWidth || !canvasHeight) return;
  const pixelRatio = Math.min(window.devicePixelRatio || 1, 2);
  const backingWidth = Math.round(canvasWidth * pixelRatio);
  const backingHeight = Math.round(canvasHeight * pixelRatio);
  if (map.width !== backingWidth || map.height !== backingHeight) {
    map.width = backingWidth;
    map.height = backingHeight;
  }
  const context = map.getContext('2d');
  context.setTransform(pixelRatio, 0, 0, pixelRatio, 0, 0);
  context.clearRect(0, 0, canvasWidth, canvasHeight);
  context.fillStyle = '#090c08';
  context.fillRect(0, 0, canvasWidth, canvasHeight);

  const focusX = state?.playerActive ? player.x : (worldWidth - 1) / 2;
  const focusY = state?.playerActive ? player.y : (worldHeight - 1) / 2;
  const originX = canvasWidth / 2 - (focusX + 0.5) * TILE_SIZE;
  const originY = canvasHeight / 2 - (focusY + 0.5) * TILE_SIZE;
  const minX = Math.max(0, Math.floor(-originX / TILE_SIZE) - 1);
  const maxX = Math.min(
    worldWidth - 1,
    Math.ceil((canvasWidth - originX) / TILE_SIZE) + 1,
  );
  const minY = Math.max(0, Math.floor(-originY / TILE_SIZE) - 1);
  const maxY = Math.min(
    worldHeight - 1,
    Math.ceil((canvasHeight - originY) / TILE_SIZE) + 1,
  );
  const visible = (x, y) => x >= minX && x <= maxX && y >= minY && y <= maxY;
  const screenX = (x) => originX + x * TILE_SIZE;
  const screenY = (y) => originY + y * TILE_SIZE;

  context.fillStyle = '#263123';
  for (let y = minY; y <= maxY; y += 1) {
    for (let x = minX; x <= maxX; x += 1) {
      context.fillRect(screenX(x) + TILE_SIZE / 2, screenY(y) + TILE_SIZE / 2, 1, 1);
    }
  }
  context.strokeStyle = '#222d1f';
  context.strokeRect(
    originX,
    originY,
    worldWidth * TILE_SIZE,
    worldHeight * TILE_SIZE,
  );

  const targetTree = walkingTargetTreeId == null
    ? null
    : worldTrees().find((tree) => tree.treeId === walkingTargetTreeId);
  const target = targetTree || walkingTarget;
  if (target && visible(target.x, target.y)) {
    context.fillStyle = '#1b2618';
    context.fillRect(screenX(target.x), screenY(target.y), TILE_SIZE, TILE_SIZE);
    context.strokeStyle = '#b9e879';
    context.strokeRect(
      screenX(target.x) + 0.5,
      screenY(target.y) + 0.5,
      TILE_SIZE - 1,
      TILE_SIZE - 1,
    );
  }

  const { standingTreeCount, stumpCount } = drawTrees(context, visible, screenX, screenY);
  const playersByPosition = drawRemotePlayers(context, visible, screenX, screenY);

  updateMapCamera(originX, originY);

  mapFrame = {
    originX,
    originY,
    tileSize: TILE_SIZE,
    minX,
    maxX,
    minY,
    maxY,
    visibleTileCount: (maxX - minX + 1) * (maxY - minY + 1),
    totalTileCount: worldWidth * worldHeight,
    standingTreeCount,
    stumpCount,
    remotePlayerCount: [...playersByPosition.values()]
      .reduce((total, count) => total + count, 0),
    clusterCount: playersByPosition.size,
  };
  globalThis.__WOODLAND_E2E_MAP_FRAME = mapFrame;

  const adjacent = adjacentTree();
  updateMapHint(adjacent);

  globalThis.__WOODLAND_E2E_PLAYER = { ...player };
  globalThis.__WOODLAND_E2E_ADJACENT = Boolean(adjacent);
  globalThis.__WOODLAND_E2E_ADJACENT_TREE = adjacent;
  globalThis.__WOODLAND_E2E_LOCKED_TREE = lockedTreeId;
}
function fundingMessage() {
  if (state.playerActive) {
    const available = (state.walletVtxos || [])
      .filter((vtxo) => vtxo.assets.length === 0)
      .reduce((sats, vtxo) => sats + vtxo.amountSats, 0);
    return `${available} asset-free wallet sats available for renewal fees`;
  }
  if (state.activationBlockedReason) return state.activationBlockedReason;
  if (state.fundingRequiredSats <= 0) return 'No additional player funding required';
  return `Deposit ${state.fundingRequiredSats} sats to the Arkade address above`;
}
function copyWithSelection(value) {
  const textarea = document.createElement('textarea');
  textarea.value = value;
  textarea.readOnly = true;
  textarea.style.position = 'fixed';
  textarea.style.opacity = '0';
  document.body.append(textarea);
  textarea.select();
  const copied = document.execCommand('copy');
  textarea.remove();
  return copied;
}


async function copyAddress() {
  const value = state?.address;
  if (!value) return;
  clearTimeout(copyResetTimer);
  try {
    let copied = false;
    let clipboardError;
    if (navigator.clipboard?.writeText) {
      try {
        await navigator.clipboard.writeText(value);
        copied = true;
      } catch (error) {
        clipboardError = error;
      }
    }
    if (!copied) copied = copyWithSelection(value);
    if (!copied) throw clipboardError || new Error('Clipboard API unavailable');
    copyAddressButton.textContent = 'Copied';
    onboardingNote.classList.remove('error');
    onboardingNote.textContent = 'Address copied to clipboard.';
    copyResetTimer = setTimeout(() => {
      copyAddressButton.textContent = 'Copy address';
    }, 1_500);
  } catch (error) {
    copyAddressButton.textContent = 'Copy failed';
    onboardingNote.classList.add('error');
    onboardingNote.textContent = 'Could not copy the address. Select it manually.';
    appendLog(`Address copy failed: ${error}`);
  }
}




function render() {
  renderMap();
  refreshButton.disabled = busy || walking;
  if (!state) {
    address.textContent = '';
    copyAddressButton.disabled = true;
    fundingInstructionElement.textContent = '';
    onboardingNote.textContent = '';
    walletSats.textContent = '-';
    playerState.textContent = 'Unavailable';
    playerSession.textContent = '-';
    element('player-asset').textContent = '';
    hudLevel.textContent = '-';
    hudXp.textContent = '-';
    hudLogs.textContent = '-';
    dashboard.classList.remove('player-active');
    bagPanel.hidden = true;
    statsPanel.hidden = true;
    details.hidden = true;
    for (const button of [activateButton, renewButton, craftAxeButton, resetProfileButton,
      resetButton, downloadBackupButton, restoreBackupButton]) button.disabled = true;
    return;
  }

  const totalLogs = worldTrees().reduce((sum, tree) => sum + tree.logReserveRemaining, 0);
  const standingTrees = worldTrees().filter((tree) => tree.health > 0).length;
  const tree = focusedTree();
  const playerActive = Boolean(state.playerActive);
  hudLevel.textContent = String(state.playerLevel);
  hudXp.textContent = String(state.playerXp);
  hudLogs.textContent = String(state.playerLogs || 0);
  hudOnline.textContent = String(nearbyPlayerCount());
  hudPosition.textContent = `(${player.x}, ${player.y})`;

  walletHeading.textContent = playerActive ? 'Wallet / top up' : 'Create player';
  dashboard.classList.toggle('player-active', playerActive);
  bagPanel.hidden = !playerActive;
  statsPanel.hidden = !playerActive;
  address.textContent = state.address;
  copyAddressButton.disabled = !state.address;
  fundingInstructionElement.textContent = fundingMessage();


  onboardingNote.classList.toggle('error', Boolean(state.activationBlockedReason));
  if (playerActive) {
    onboardingNote.textContent = `The ${state.dustSats}-sat player deposit stays locked. If Arkade charges renewal fees, send a separate asset-free VTXO to this address, then press Refresh.`;
  } else if (state.activationBlockedReason) {
    onboardingNote.textContent = state.activationBlockedReason;
  } else if (state.fundingRequiredSats > 0) {
    onboardingNote.textContent = 'Send from any Arkade wallet, then press Refresh.';
  } else if (state.activationReady) {
    onboardingNote.textContent = 'Deposit detected. Creating a player issues its unique PLAYER_ID.';
  } else {
    onboardingNote.textContent = 'Wallet ready.';
  }

  walletSats.textContent = `${state.walletSats} sats`;
  playerState.textContent = playerActive ? 'Active' : 'Inactive';
  const sessionLeft = state.playerStateExpiresInSeconds;
  playerSession.classList.remove('error');
  let sessionText = '-';
  if (playerActive && sessionLeft == null) {
    sessionText = 'Unknown expiry';
  } else if (playerActive && sessionLeft <= 300) {
    playerSession.classList.add('error');
    sessionText = `${sessionLeft}s left`;
  } else if (playerActive && sessionLeft <= 600) {
    playerSession.classList.add('error');
    sessionText = `${Math.ceil(sessionLeft / 60)} min left`;
  } else if (playerActive) {
    sessionText = `${Math.floor(sessionLeft / 60)} min left`;
  }
  playerSession.textContent = sessionText;

  levelNumber.textContent = String(state.playerLevel);
  xpNumber.textContent = `${state.playerXp} Woodcutting XP`;
  xpNext.textContent = state.playerNextLevelXp == null
    ? 'Maximum level'
    : `${Math.max(0, state.playerNextLevelXp - state.playerXp)} Woodcutting XP to level ${state.playerLevel + 1}`;
  xpBacking.textContent = `${state.seasonXpRemaining} season Woodcutting XP remaining`;
  logChance.textContent = `${state.logDropBasisPoints / 100}%`;
  forestHealth.textContent = `${totalLogs} LOG reserve / ${standingTrees} standing`;
  const logCount = state.playerLogs || 0;
  playerLogs.textContent = String(logCount);
  logSlot.classList.toggle('empty', logCount === 0);
  logSlot.setAttribute('aria-label', `${logCount} LOG in inventory`);
  const stoneCount = state.playerStone || 0;
  playerStone.textContent = String(stoneCount);
  stoneSlot.classList.toggle('empty', stoneCount === 0);
  stoneSlot.setAttribute('aria-label', `${stoneCount} STONE in inventory`);
  const ironOreCount = state.playerIronOre || 0;
  playerIronOre.textContent = String(ironOreCount);
  ironOreSlot.classList.toggle('empty', ironOreCount === 0);
  ironOreSlot.setAttribute('aria-label', `${ironOreCount} IRON ORE in inventory`);
  const currentAxeName = axeName(state.playerAxe);
  playerAxe.textContent = state.playerAxe === 'none'
    ? 'None'
    : currentAxeName.replace(' Axe', '');
  axeSlot.classList.toggle('empty', state.playerAxe === 'none');
  axeSlot.setAttribute('aria-label', `${currentAxeName} equipped`);

  const stumps = worldTrees().filter((candidate) => candidate.health === 0 && !candidate.depleted);
  const depleted = worldTrees().filter((candidate) => candidate.depleted);
  treeState.textContent = stumps.length || depleted.length
    ? `${stumps.length} stump${depleted.length ? ` / ${depleted.length} depleted` : ''}`
    : '0';
  emulator.textContent = `${state.emulatorVersion} / ${state.emulatorSigner.slice(0, 12)}...`;

  activateButton.hidden = playerActive;
  activateButton.disabled = walking || busy || playerActive || !state.activationReady;
  activateButton.textContent = state.pendingActivationTxid
    ? (busy ? 'Recovering player...' : 'Recover player')
    : (busy && !playerActive ? 'Creating player...' : 'Create player');
  const mainnet = worldManifest?.network === 'bitcoin';
  const pendingTransaction = hasPendingWalletTransaction();
  resetProfileButton.hidden = mainnet || playerActive || !localStorage.getItem(profileStorageKey);
  resetProfileButton.disabled = walking || busy || polling || playerActive || pendingTransaction;
  resetButton.hidden = mainnet;
  resetButton.disabled = walking || busy || polling || pendingTransaction;
  downloadBackupButton.disabled = walking || busy || !app;
  restoreBackupButton.disabled = walking || busy || polling || !app
    || pendingTransaction;
  restoreBackupButton.title = state.pendingActivationTxid
    ? 'Wait for the pending activation to reconcile before switching keys.'
    : pendingTransaction
      ? 'Wait for the saved pending transaction to reconcile before switching keys.'
      : '';
  const nextRecipe = state.nextAxeRecipe;
  axeRecipe.textContent = axeRecipeSummary(nextRecipe);
  craftAxeButton.textContent = nextRecipe
    ? `Craft ${axeName(nextRecipe.axe)}`
    : 'Highest axe crafted';
  craftAxeButton.disabled = walking
    || busy
    || !playerActive
    || !state.craftAxeReady
    || Boolean(state.pendingChopTxid);
  craftAxeButton.title = !playerActive
    ? 'Create a player before crafting.'
    : axeRecipeSummary(nextRecipe);
  const rolloverDue = state.playerStateExpiresInSeconds != null
    && state.playerRolloverMarginSeconds != null
    && state.playerStateExpiresInSeconds < state.playerRolloverMarginSeconds;
  renewButton.disabled = walking
    || busy
    || !playerActive
    || !rolloverDue
    || Boolean(state.pendingChopTxid);

  details.hidden = !tree;
  if (tree) {
    element('tree-id').textContent = `#${tree.treeId} at (${tree.x}, ${tree.y})`;
    element('tree-health').textContent = tree.health === 0
      ? (tree.depleted ? 'exhausted stump' : 'funded stump; click to regrow')
      : `${tree.health} active`;
    element('tree-reserve').textContent = `${tree.logReserveRemaining} LOG`;
    element('tree-xp').textContent = `${tree.xpRemaining} Woodcutting XP`;
    element('tree-stone').textContent = `${tree.stoneRemaining} STONE`;
    element('tree-iron-ore').textContent = `${tree.ironOreRemaining} IRON ORE`;
    element('player-asset').textContent = state.playerAsset || 'not issued';
    element('tree-asset').textContent = state.treeAsset;
    element('log-asset').textContent = state.logAsset;
    element('xp-asset').textContent = state.xpAsset;
    element('stone-asset').textContent = state.stoneAsset;
    element('iron-ore-asset').textContent = state.ironOreAsset;
    element('tree-value').textContent = `${tree.valueSats} sats fixed`;
    element('tree-outpoint').textContent = tree.treeOutpoint;
    element('last-chop').textContent = tree.lastAttemptTxid || 'none';
  }
  globalThis.__WOODLAND_E2E_STATE = state;
  globalThis.__WOODLAND_E2E_SERVER_REGISTERED = serverRegistered;
}

async function run(label, action, completion = () => 'Success') {
  if (busy) return;
  globalThis.__WOODLAND_E2E_ERROR = null;
  setBusy(true, label);
  appendLog(label);
  try {
    adoptState(await action());
    status.classList.remove('error');
    const message = completion(state);
    status.textContent = message;
    appendLog(message);
  } catch (error) {
    status.classList.add('error');
    status.textContent = String(error);
    appendLog(`Error: ${error}`);
    globalThis.__WOODLAND_E2E_ERROR = String(error);
  } finally {
    persistProfile();
    persistPosition();
    setBusy(false);
    void syncServerRegistration().catch((error) => {
      leaderboardStatus.textContent = `Server registration failed: ${error}`;
    });
  }
}

function persistProfile() {
  requireWalletWriter();
  if (app) localStorage.setItem(profileStorageKey, app.exportProfile());
}

function persistPosition() {
  if (!state?.playerActive) return;
  requireWalletWriter();
  localStorage.setItem(POSITION, JSON.stringify({
    genesisTxid: state.genesisTxid,
    x: player.x,
    y: player.y,
  }));
}

function restorePosition() {
  if (!state?.playerActive) {
    localStorage.removeItem(POSITION);
    return;
  }
  try {
    const saved = JSON.parse(localStorage.getItem(POSITION) || 'null');
    const valid = saved?.genesisTxid === state.genesisTxid
      && Number.isInteger(saved.x)
      && Number.isInteger(saved.y)
      && saved.x >= 0
      && saved.y >= 0
      && saved.x < state.mapWidth
      && saved.y < state.mapHeight
      && !worldTrees().some((tree) => tree.x === saved.x && tree.y === saved.y);
    if (!valid) {
      localStorage.removeItem(POSITION);
      return;
    }
    player.x = saved.x;
    player.y = saved.y;
  } catch {
    localStorage.removeItem(POSITION);
  }
}

function setBackupStatus(message, isError = false) {
  backupStatus.textContent = message;
  backupStatus.classList.toggle('error', isError);
}

function createPlayerBackup() {
  if (!app || !state || !worldManifest) throw new Error('Player wallet is not ready');
  const profile = JSON.parse(app.exportProfile());
  return {
    format: PLAYER_BACKUP_FORMAT,
    version: PLAYER_BACKUP_VERSION,
    createdAt: new Date().toISOString(),
    gameId: worldManifest.gameId,
    protocolVersion: worldManifest.protocolVersion,
    network: worldManifest.network,
    genesisTxid: profile.genesisTxid,
    walletAddress: app.address(),
    secretKey: app.exportKey(),
    playerAsset: profile.playerAsset || null,
    pendingActivation: profile.pendingActivation || undefined,
    position: state.playerActive ? { x: player.x, y: player.y } : null,
  };
}

function downloadPlayerBackup() {
  const backup = createPlayerBackup();
  const body = `${JSON.stringify(backup, null, 2)}\n`;
  const url = URL.createObjectURL(new Blob([body], { type: 'application/json' }));
  const link = document.createElement('a');
  const label = (backup.playerAsset || backup.genesisTxid).slice(0, 12);
  link.href = url;
  link.download = `woodland-player-backup-${label}.json`;
  document.body.append(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 0);
  setBackupStatus('Backup downloaded. Store it offline and keep it secret.');
  appendLog('Player key and PLAYER_ID backup downloaded.');
}

function parsePlayerBackup(text) {
  let backup;
  try {
    backup = JSON.parse(text);
  } catch {
    throw new Error('Backup is not valid JSON');
  }
  if (!backup || typeof backup !== 'object' || Array.isArray(backup)) {
    throw new Error('Backup must contain one JSON object');
  }
  if (backup.format !== PLAYER_BACKUP_FORMAT || backup.version !== PLAYER_BACKUP_VERSION) {
    throw new Error('Unsupported woodland.sh backup format');
  }
  if (
    backup.gameId !== worldManifest.gameId
    || backup.protocolVersion !== worldManifest.protocolVersion
    || backup.network !== worldManifest.network
    || backup.genesisTxid !== worldManifest.genesisTxid
  ) {
    throw new Error('Backup belongs to a different woodland.sh world');
  }
  if (typeof backup.walletAddress !== 'string' || !backup.walletAddress || backup.walletAddress.length > 200) {
    throw new Error('Backup wallet address is invalid');
  }
  if (typeof backup.secretKey !== 'string' || !/^[0-9a-f]{64}$/u.test(backup.secretKey)) {
    throw new Error('Backup private key is invalid');
  }
  if (
    backup.playerAsset !== null
    && (typeof backup.playerAsset !== 'string' || !/^[0-9a-f]{68}$/u.test(backup.playerAsset))
  ) {
    throw new Error('Backup PLAYER_ID is invalid');
  }
  let position = null;
  if (backup.position !== null) {
    if (
      !backup.position
      || typeof backup.position !== 'object'
      || !Number.isInteger(backup.position.x)
      || !Number.isInteger(backup.position.y)
      || backup.position.x < 0
      || backup.position.y < 0
      || backup.position.x >= worldManifest.mapWidth
      || backup.position.y >= worldManifest.mapHeight
    ) {
      throw new Error('Backup map position is invalid');
    }
    position = { x: backup.position.x, y: backup.position.y };
  }
  return { ...backup, position };
}

async function restorePlayerBackup(file) {
  if (busy || polling || !file || hasPendingWalletTransaction()) return;
  requireWalletWriter();
  let reloading = false;
  setBusy(true, 'Validating player backup...');
  setBackupStatus('Checking the key, PLAYER_ID, and current world...');
  try {
    if (file.size > MAX_PLAYER_BACKUP_BYTES) throw new Error('Backup file is too large');
    const backup = parsePlayerBackup(await file.text());
    const profile = JSON.stringify({
      genesisTxid: backup.genesisTxid,
      playerAsset: backup.playerAsset,
      pendingActivation: backup.pendingActivation,
    });
    const candidate = await WoodlandApp.init(
      worldManifest.arkadeServiceUrl,
      worldManifest.emulatorUrl,
      JSON.stringify(worldManifest),
      backup.secretKey,
      profile,
    );
    if (candidate.address() !== backup.walletAddress) {
      throw new Error('Backup private key does not match its wallet address');
    }

    let playerCheck = 'This backup contains a wallet key with no activated PLAYER_ID.';
    if (backup.pendingActivation) {
      playerCheck = 'This backup includes a submitted activation. It will be recovered after restoring; do not deposit again.';
    } else if (backup.playerAsset) {
      try {
        const candidateState = await candidate.refreshPlayer();
        playerCheck = candidateState.playerActive
          ? `Live player found for ${backup.playerAsset.slice(0, 12)}...`
          : 'The key is valid, but this PLAYER_ID is not currently live at the canonical player contract.';
      } catch (error) {
        playerCheck = `The key is valid, but live player lookup could not be completed: ${String(error).slice(0, 180)}`;
      }
    }
    if (!window.confirm(
      `${playerCheck}\n\nRestoring replaces this browser’s current private key and clears its pending swing journal. Back up the current player first. Continue?`,
    )) {
      setBackupStatus('Restore cancelled. The current key was not changed.');
      return;
    }

    const journal = {
      version: 1,
      genesisTxid: backup.genesisTxid,
      secretKey: backup.secretKey,
      profile,
      pendingKey: pendingStorageKey,
      position: backup.position ? JSON.stringify({
        genesisTxid: backup.genesisTxid, x: backup.position.x, y: backup.position.y,
      }) : null,
    };
    // If staging fails, the current wallet is untouched. Once staged, stop
    // using its in-memory key: reload/Refresh must finish this exact restore.
    localStorage.setItem(RESTORE_JOURNAL, JSON.stringify(journal));
    app = undefined;
    state = undefined;
    serverRegistered = false;
    recoverWalletRestore();
    setBackupStatus('Backup restored. Reloading the player...');
    appendLog('Player backup restored; reloading.');
    reloading = true;
    location.reload();
  } catch (error) {
    reloading = false;
    const unfinished = Boolean(localStorage.getItem(RESTORE_JOURNAL));
    const message = unfinished
      ? `Backup restore is saved but unfinished: ${error}. Free browser storage, then press Refresh to recover it.`
      : `Backup restore failed: ${error}`;
    setBackupStatus(message, true);
    status.classList.add('error');
    status.textContent = message;
    appendLog(message);
  } finally {
    restoreBackupFile.value = '';
    if (!reloading) setBusy(false);
  }
}

async function renewPlayer() {
  try {
    const renewed = await app.renewPlayer();
    lastRenewalError = null;
    status.classList.remove('error');
    status.textContent = 'Player session renewed.';
    return renewed;
  } catch (error) {
    const message = `Player renewal failed: ${error}`;
    if (lastRenewalError !== message) appendLog(message);
    lastRenewalError = message;
    status.classList.add('error');
    status.textContent = message;
    render();
    throw error;
  }
}

async function refreshWorld() {
  const nextState = await app.refreshWorld();
  nextWorldRefreshAt = Date.now() + 60_000;
  return nextState;
}


async function activatePlayer() {
  return app.activate();
}

refreshButton.addEventListener('click', () => {
  if (!app) {
    boot().catch(reportBootError);
    return;
  }
  const resume = Boolean(state?.pendingChopTxid);
  run(
    resume ? 'Resuming the exact pending swing...' : 'Refreshing indexed world and wallet state...',
    () => withApp(() => (resume ? app.resumePendingChop() : app.refresh())),
    () => state.pendingActivationTxid
      ? state.activationBlockedReason
      : (resume ? 'Pending swing reconciled.' : 'Refresh complete.'),
  );
});

activateButton.addEventListener('click', () => (
  run(
    'Issuing PLAYER_ID into owner-authorized player state...',
    () => withApp(activatePlayer),
    () => state.pendingActivationTxid ? state.activationBlockedReason : 'Player activated.',
  )
));


async function waitForChopCadence(startedAt) {
  let remaining = CHOP_CADENCE_MS - (performance.now() - startedAt);
  while (remaining > 0 && !stopChopping) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(50, remaining)));
    remaining = CHOP_CADENCE_MS - (performance.now() - startedAt);
  }
}

async function chopUntilLog(treeId) {
  let swings = 0;
  let success = false;
  let material = 'none';
  const runStartedAt = performance.now();
  chopping = true;
  stopChopping = false;
  render();
  try {
    while (!stopChopping) {
      const tree = worldTrees().find((candidate) => candidate.treeId === treeId);
      if (!tree || tree.health === 0) break;
      flashTree(treeId, 'chop', CHOP_FLASH_MS);
      const submittedAt = performance.now();
      status.textContent = `Submitting swing ${swings + 1} at tree #${treeId}...`;
      const feedbackTimer = setInterval(() => {
        flashTree(treeId, 'chop', CHOP_FLASH_MS);
        const seconds = Math.max(1, Math.round((performance.now() - submittedAt) / 1000));
        status.textContent = `Swing ${swings + 1} pending (${seconds}s)...`;
      }, CHOP_FEEDBACK_MS);
      let nextState;
      try {
        nextState = await withApp(() => app.chop(treeId));
      } finally {
        clearInterval(feedbackTimer);
      }
      adoptState(nextState);
      swings += 1;
      success = state.lastAttempt?.success === true;
      material = state.lastAttempt?.material || 'none';
      if (success) flashTree(treeId, 'log', LOG_FLASH_MS, false);
      render();
      if (!success) await waitForChopCadence(submittedAt);
      if (success) break;
    }
    return state;
  } finally {
    lastChopRun = {
      treeId,
      swings,
      success,
      material,
      cancelled: stopChopping && !success,
      durationMs: Math.round(performance.now() - runStartedAt),
    };
    globalThis.__WOODLAND_E2E_LAST_CHOP_RUN = lastChopRun;
    chopping = false;
    stopChopping = false;
    lockedTreeId = null;
    renderMap();
}
}

function attemptChop(selectedTreeId = null) {
  if (chopping) {
    stopChopping = true;
    status.textContent = 'Stopping after the current swing...';
    renderMap();
    return;
  }
  if (walking) cancelWalking();
  const tree = selectedTreeId == null
    ? adjacentTree()
    : worldTrees().find((candidate) => (
      candidate.treeId === selectedTreeId
      && Math.abs(player.x - candidate.x) + Math.abs(player.y - candidate.y) === 1
    ));
  if (busy || !state?.fundingReady || !tree || tree.health === 0) {
    lockedTreeId = null;
    renderMap();
    return;
  }
  focusedTreeId = tree.treeId;
  lockedTreeId = tree.treeId;
  run(
    `Chopping tree #${tree.treeId} until a LOG drops...`,
    () => chopUntilLog(tree.treeId),
    () => {
      if (lastChopRun?.success) {
        const suffix = lastChopRun.swings === 1 ? 'swing' : 'swings';
        const materialMessage = lastChopRun.material === 'stone'
          ? ' You also find STONE.'
          : lastChopRun.material === 'ironOre'
            ? ' You also find IRON ORE.'
            : '';
        return `You get a LOG and ${state.woodcuttingXpPerLog} Woodcutting XP after ${lastChopRun.swings} ${suffix}.${materialMessage}`;
      }
      if (lastChopRun?.cancelled) {
        return `Stopped after ${lastChopRun.swings} accepted swing${lastChopRun.swings === 1 ? '' : 's'}.`;
      }
      return 'The tree is unavailable.';
    },
  );
}

copyAddressButton.addEventListener('click', () => { void copyAddress(); });


delegateRenewalButton.addEventListener('click', () => { void updateDelegation(); });
chatInput.addEventListener('input', () => {
  sendChatButton.disabled = chatInput.disabled || !chatInput.value.trim() || socialPosting;
});
chatForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const message = chatInput.value.trim();
  if (message) void submitChat(message);
});


craftAxeButton.addEventListener('click', () => {
  const recipe = state?.nextAxeRecipe;
  if (!recipe) return;
  const expectedOutpoint = state.playerStateOutpoint;
  run(
    `Crafting ${axeName(recipe.axe)} under the player covenant...`,
    () => withApp(() => app.craftAxe(expectedOutpoint)),
    () => `${axeName(state.playerAxe)} crafted and equipped.`,
  );
});

renewButton.addEventListener('click', () => {
  run(
    'Renewing player state through a fresh Ark batch...',
    () => withApp(renewPlayer),
    () => 'Player session renewed.',
  );
});
map.addEventListener('click', (event) => { void handleMapClick(event); });


downloadBackupButton.addEventListener('click', () => {
  if (busy) return;
  try {
    downloadPlayerBackup();
  } catch (error) {
    setBackupStatus(`Backup failed: ${error}`, true);
  }
});

restoreBackupButton.addEventListener('click', () => {
  if (!busy) restoreBackupFile.click();
});

restoreBackupFile.addEventListener('change', () => {
  const [file] = restoreBackupFile.files;
  if (file) void restorePlayerBackup(file);
});

resetButton.addEventListener('click', () => {
  if (!walletWriterOwned || !worldManifest || worldManifest.network === 'bitcoin'
    || busy || polling || hasPendingWalletTransaction()
    || !window.confirm('Forget this local test key and create a new wallet?')) return;
  localStorage.removeItem(KEY);
  localStorage.removeItem(PROFILE);
  localStorage.removeItem(profileStorageKey);
  localStorage.removeItem(pendingStorageKey);
  localStorage.removeItem(POSITION);
  location.reload();
});

resetProfileButton.addEventListener('click', () => {
  if (
    !walletWriterOwned || !worldManifest || worldManifest.network === 'bitcoin'
    || busy || polling
    || state?.playerActive
    || hasPendingWalletTransaction()
    || !window.confirm('Forget the saved world profile but keep this funded wallet key?')
  ) return;
  localStorage.removeItem(profileStorageKey);
  localStorage.removeItem(POSITION);
  location.reload();
});

async function boot() {
  if (busy) return;
  busy = true;
  globalThis.__WOODLAND_E2E_ERROR = null;
  status.classList.remove('error');
  status.textContent = 'Connecting directly to Arkade and the emulator...';
  try {
    await acquireWalletWriter();
    recoverWalletRestore();
    await init();
    const worldResponse = await fetch(WORLD, { cache: 'no-store' });
    if (!worldResponse.ok) {
      throw new Error(`woodland.sh world unavailable (${worldResponse.status})`);
    }
    const world = await worldResponse.text();
    const manifest = JSON.parse(world);
    worldManifest = manifest;
    treeLayout = manifest.trees.map(({ state: tree, deploymentTxid }) => ({
      treeId: tree.treeId,
      x: tree.x,
      y: tree.y,
      health: manifest.activeLogsPerTree,
      logReserveRemaining: manifest.logReservePerTree,
      xpRemaining: manifest.xpPerTree * manifest.woodcuttingXpPerLog,
      stoneRemaining: manifest.stoneReservePerTree,
      ironOreRemaining: manifest.ironOreReservePerTree,
      valueSats: manifest.dustSats,
      treeOutpoint: `${deploymentTxid}:0`,
      deploymentTxid,
      lastAttemptTxid: null,
      nextRollBucket: null,
      nextDrop: false,
      expiresInSeconds: null,
      depleted: false,
    }));
    profileStorageKey = `${PROFILE}:${manifest.genesisTxid}`;
    pendingStorageKey = `woodland.sh:web:v2:pending:${manifest.arkadeServiceUrl.replace(/\/+$/, '')}:${manifest.genesisTxid}`;
    const storedProfile = localStorage.getItem(profileStorageKey);
    app = await WoodlandApp.init(
      manifest.arkadeServiceUrl,
      manifest.emulatorUrl,
      world,
      localStorage.getItem(KEY) || undefined,
      storedProfile || undefined,
    );
    localStorage.setItem(KEY, app.exportKey());
    // Keep the Rust bootstrap viewport around spawn until player state is known.
    adoptState(await refreshWorld());
    persistProfile();
    restorePosition();
    if (state.pendingChopTxid) {
      appendLog(`Resuming pending swing ${state.pendingChopTxid}...`);
      try {
        adoptState(await app.resumePendingChop());
        appendLog('Pending swing reconciled.');
      } catch (error) {
        status.classList.add('error');
        status.textContent = 'Still recovering the submitted swing; it retries automatically and your funds remain safe.';
        const message = `Pending swing remains unresolved: ${error}`;
        appendLog(message);
        globalThis.__WOODLAND_E2E_ERROR = String(error);
      }
    }
    if (!state.pendingChopTxid) {
      status.textContent = state.pendingActivationTxid
        ? 'Recovering the submitted activation. Refresh or retry; do not deposit again.'
        : 'Ready';
    }
    appendLog('Connected to woodland.sh');
    globalThis.__WOODLAND_E2E_READY = true;
    globalThis.__WOODLAND_E2E_SET_TREE_VIEWPORT = (minX, minY, maxX, maxY) => {
      treeViewportOverride = [minX, minY, maxX, maxY].every(Number.isInteger)
        ? { minX, minY, maxX, maxY }
        : null;
    };
    globalThis.__WOODLAND_E2E_SUBMISSION_RECOVERY = async (treeId) => {
      adoptState(await withApp(() => app.testSubmissionRecovery(treeId)));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_APP = app;
    globalThis.__WOODLAND_E2E_SERVER_REGISTRATION = () => (
      withApp(() => app.serverRegistration(SERVER_URL))
    );
    globalThis.__WOODLAND_E2E_SERVER_LOCATION = (x, y) => (
      withApp(() => app.serverLocation(SERVER_URL, x, y, Date.now()))
    );
    globalThis.__WOODLAND_E2E_CLICK_MAP = (x, y) => {
      void handleMapPosition(Number(x), Number(y));
    };
    globalThis.__WOODLAND_E2E_CLICK_TREE = (treeId) => {
      const tree = worldTrees().find((candidate) => candidate.treeId === Number(treeId));
      if (tree) void handleMapPosition(tree.x, tree.y);
    };
    globalThis.__WOODLAND_E2E_REGROW = async (treeId) => {
      adoptState(await withApp(() => app.regrow(Number(treeId))));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_INVALID_XP = async (treeId) => {
      await withApp(() => app.testInvalidXpTransition(treeId));
      return withApp(refreshWorld);
    };
    globalThis.__WOODLAND_E2E_WITHDRAW_LOG = async (amount) => {
      adoptState(await withApp(() => app.withdrawLog(amount)));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_CRAFT_AXE = async (expectedOutpoint = state.playerStateOutpoint) => {
      adoptState(await withApp(() => app.craftAxe(expectedOutpoint)));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_REFRESH = async () => {
      const resumePending = Boolean(state?.pendingChopTxid);
      adoptState(await withApp(() => (
        resumePending ? app.resumePendingChop() : refreshWorld()
      )));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_REFRESH_WORLD_RAW = () => withApp(refreshWorld);
    globalThis.__WOODLAND_E2E_REFRESH_WORLD = async () => {
      adoptState(await withApp(refreshWorld));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_INVALID_ASSET_ORDER = async (treeId) => {
      await withApp(() => app.testInvalidAssetGroupOrder(treeId));
      return withApp(() => app.refresh());
    };
    globalThis.__WOODLAND_E2E_SET_REMOTE_LOCATIONS = (locations) => {
      remoteLocations = Array.isArray(locations) ? locations : [];
      renderMap();
    };
    globalThis.__WOODLAND_E2E_INVALID_XP_GROUP = async (treeId) => {
      await withApp(() => app.testInvalidXpGroup(treeId));
      return withApp(() => app.refresh());
    };
    globalThis.__WOODLAND_E2E_INVALID_LOG_XP_ORDER = async (treeId) => {
      await withApp(() => app.testInvalidLogXpGroupOrder(treeId));
      return withApp(() => app.refresh());
    };
    globalThis.__WOODLAND_E2E_CHOP_MUTATION = async (mutation, treeId) => {
      await withApp(() => app.testChopMutation(treeId, mutation));
      return withApp(() => app.refresh());
    };
    globalThis.__WOODLAND_E2E_CHOP_NATURAL = async (treeId) => {
      try {
        adoptState(await withApp(() => app.chop(Number(treeId))));
        render();
        return { ok: true, state, natural: true };
      } catch (error) {
        adoptState(await withApp(() => app.refresh()));
        render();
        return { ok: false, message: String(error), state, natural: true };
      }
    };
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED = async (...args) => {
      try {
        adoptState(await withApp(() => app.chopExpected(...args)));
        render();
        return { ok: true, state };
      } catch (error) {
        adoptState(await withApp(() => app.refresh()));
        render();
        return { ok: false, message: String(error), state };
      }
    };
    globalThis.__WOODLAND_E2E_RESUME_PENDING = async () => {
      adoptState(await withApp(() => app.resumePendingChop()));
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_RENEW_PLAYER = async () => {
      adoptState(await withApp(renewPlayer));
      render();
      return state;
    };
    void Promise.all([refreshLeaderboard(), refreshPresence(), refreshChat()]);
    if (state.playerActive) {
      void syncServerRegistration(true).catch((error) => {
        leaderboardStatus.textContent = `Server registration failed: ${error}`;
      });
    }
  } finally {
    busy = false;
    renderSocialControls();
    render();
  }
}

function reportBootError(error) {
  busy = false;
  app = undefined;
  const detail = String(error);
  const message = detail.includes('Failed to fetch')
    ? 'Cannot reach local arkd or emulator. Run ./scripts/regtest.sh start-tree, then press Refresh.'
    : detail;
  status.classList.add('error');
  status.textContent = message;
  appendLog(`Boot failed: ${message}`);
  globalThis.__WOODLAND_E2E_ERROR = message;
}

renderMap();
boot().catch(reportBootError);

setInterval(async () => {
  if (!app || busy || polling) return;
  polling = true;
  render();
  try {
    const resumePending = Boolean(state?.pendingChopTxid)
      && Date.now() >= pendingRetryAfter;
    if (resumePending) pendingRetryAfter = Date.now() + 5_000;
    const stumpActive = state?.trees?.some((tree) => (
      tree.health === 0 && !tree.depleted
    ));
    const refreshWorld = !resumePending
      && (stumpActive || Date.now() >= nextWorldRefreshAt);
    adoptState(await withApp(() => {
      if (resumePending) return app.resumePendingChop();
      return refreshWorld ? app.refreshWorld() : app.refresh();
    }));
    if (refreshWorld) nextWorldRefreshAt = Date.now() + 60_000;
    if (resumePending && !state.pendingChopTxid) {
      pendingRetryAfter = 0;
      status.textContent = 'Pending swing recovered.';
    }
    if (
      state.playerActive
      && !state.pendingChopTxid
      && state.playerStateExpiresInSeconds != null
      && state.playerRolloverMarginSeconds != null
      && state.playerStateExpiresInSeconds < state.playerRolloverMarginSeconds
      && !(delegatedRenewal
        && delegationObservedAt != null
        && Date.now() - delegationObservedAt < DELEGATION_FRESHNESS_MS
        && state.playerStateExpiresInSeconds > OWNER_RENEWAL_FALLBACK_SECONDS)
    ) {
      status.textContent = 'Rolling player state into a fresh Arkade batch...';
      adoptState(await withApp(renewPlayer));
    }
    persistProfile();
    render();
    void syncServerRegistration().catch((error) => {
      leaderboardStatus.textContent = `Server registration failed: ${error}`;
    });
  } catch {}
  finally {
    polling = false;
    render();
  }

}, 10_000);
window.addEventListener('resize', renderMap);

setInterval(() => {
  void refreshPresence();
  void publishLocation();
}, 1_000);

setInterval(() => {
  void refreshChat();
}, 2_000);

setInterval(() => {
  void refreshLeaderboard();
}, 15_000);
