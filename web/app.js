import init, { WoodlandApp } from './pkg/woodland.js';

const WORLD = new URL('./world.json', import.meta.url);
const SERVER_URL = document
  .querySelector('meta[name="woodland-server"]')
  ?.content.replace(/\/+$/, '') || '';
const STORAGE_SCOPE = location.origin;
const KEY = `woodland.sh:web:v1:key:${STORAGE_SCOPE}`;
const PROFILE = `woodland.sh:web:v1:profile:${STORAGE_SCOPE}`;
let pendingStorageKey = `woodland.sh:web:v1:pending:${STORAGE_SCOPE}`;
const POSITION = `woodland.sh:web:v1:position:${STORAGE_SCOPE}`;
const SERVER_REGISTRATION = `woodland.sh:web:v1:server:${STORAGE_SCOPE}:${SERVER_URL}`;

const element = (id) => document.getElementById(id);
const address = element('address');
const copyAddressButton = element('copy-address');
const fundingInstructionElement = element('funding-instruction');
const walletSats = element('wallet-sats');
const forestHealth = element('forest-health');
const playerLogs = element('player-logs');
const logSlot = element('log-slot');
const playerState = element('player-state');
const playerSession = element('player-session');
const xpBacking = element('xp-backing');
const logChance = element('log-chance');
const regrowReady = element('regrow-ready');
const emulator = element('emulator');
const status = element('status');
const log = element('log');
const onboarding = element('onboarding');
const onboardingNote = element('onboarding-note');
const dashboard = element('dashboard');
const bagPanel = element('bag-panel');
const statsPanel = element('stats-panel');
const levelNumber = element('level-number');
const xpNumber = element('xp-number');
const xpNext = element('xp-next');
const map = element('map');
const mapViewport = element('map-viewport');
const mapHint = element('map-hint');
const refreshButton = element('refresh');
const renewButton = element('renew-player');
const activateButton = element('activate');
const resetProfileButton = element('reset-profile');
const resetButton = element('reset');
const details = element('details');
const leaderboardPanel = element('leaderboard-panel');
const leaderboardRows = element('leaderboard-rows');
const leaderboardStatus = element('leaderboard-status');
const joinLeaderboardButton = element('join-leaderboard');
const delegateRenewalButton = element('delegate-renewal');
const chatForm = element('chat-form');
const chatInput = element('chat-input');
const sendChatButton = element('send-chat');
const chatMessagesElement = element('chat-messages');
const chatStatus = element('chat-status');

const DEFAULT_MAP_WIDTH = 45;
const DEFAULT_MAP_HEIGHT = 19;
const WALK_STEP_MS = 110;
const CHOP_SWING_MS = 900;
const CHOP_FLASH_MS = 340;
const LOG_FLASH_MS = 800;
const DIRECTIONS = [[0, -1], [-1, 0], [1, 0], [0, 1]];
const TREE_GLYPH = '🌲';
const player = { x: 3, y: 17 };

let app;
let state;
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
let appQueue = Promise.resolve();
let leaderboardPlayers = [];
let leaderboardJoined = false;
let serverRegistrationSyncing = false;
let serverRegistrationOutpoint = null;
let serverRegistrationRetryAfter = 0;
let remoteLocations = [];
let chatMessages = [];
let delegatedRenewal = false;
let delegationAvailable = false;
let socialPosting = false;
let locationPosting = false;
let lastPublishedLocation = null;
let lastRenderedChatId = null;
let lastCameraPosition = null;
let cameraInitialized = false;

function withApp(action) {
  const operation = appQueue.then(action, action);
  appQueue = operation.catch(() => {});
  return operation;
}

function appendLog(message) {
  const time = new Date().toLocaleTimeString();
  log.textContent = `[${time}] ${message}\n${log.textContent}`.slice(0, 8000);
}

function setBusy(value, message = '') {
  busy = value;
  if (message) status.textContent = message;
  render();
}

function restoreServerRegistration() {
  leaderboardJoined = false;
  if (!SERVER_URL || !state?.genesisTxid || !state.playerAsset) return;
  try {
    const registration = JSON.parse(localStorage.getItem(SERVER_REGISTRATION) || 'null');
    leaderboardJoined = registration?.genesisTxid === state.genesisTxid
      && registration?.playerAsset === state.playerAsset;
  } catch {
    localStorage.removeItem(SERVER_REGISTRATION);
  }
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
  if (!leaderboardJoined) {
    chatMessagesElement.textContent = 'Join the server to chat.';
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

async function refreshSocial() {
  if (!SERVER_URL) return;
  try {
    const response = await fetch(`${SERVER_URL}/v1/social`, {
      cache: 'no-store',
      mode: 'cors',
    });
    if (!response.ok) throw new Error(`server returned ${response.status}`);
    const payload = await response.json();
    leaderboardPlayers = Array.isArray(payload.players) ? payload.players : [];
    remoteLocations = Array.isArray(payload.locations) ? payload.locations : [];
    chatMessages = Array.isArray(payload.messages) ? payload.messages : [];
    delegationAvailable = payload.delegationAvailable === true;
    delegatedRenewal = Array.isArray(payload.delegatedPlayerAssets)
      && payload.delegatedPlayerAssets.includes(state?.playerAsset);
    leaderboardStatus.textContent = `${leaderboardPlayers.length} verified · ${remoteLocations.length} online`;
    globalThis.__WOODLAND_E2E_LEADERBOARD = leaderboardPlayers;
    globalThis.__WOODLAND_E2E_SOCIAL = payload;
    render();
  } catch (error) {
    delegationAvailable = false;
    delegatedRenewal = false;
    render();
    leaderboardStatus.textContent = `Server unavailable: ${error}`;
  }
}

async function syncServerRegistration(force = false) {
  if (
    !SERVER_URL
    || !leaderboardJoined
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
    localStorage.setItem(SERVER_REGISTRATION, JSON.stringify({
      genesisTxid: state.genesisTxid,
      playerAsset: state.playerAsset,
    }));
    await refreshSocial();
    await publishLocation(true);
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
  if (!SERVER_URL || !leaderboardJoined || !state?.playerActive || locationPosting) return;
  const location = `${player.x}:${player.y}`;
  if (!force && location === lastPublishedLocation) return;
  locationPosting = true;
  try {
    const locationRequest = await withApp(() => (
      app.serverLocation(SERVER_URL, player.x, player.y, Date.now())
    ));
    await postServerAction('/v1/location', locationRequest);
    lastPublishedLocation = location;
  } catch (error) {
    leaderboardStatus.textContent = `Location update failed: ${error}`;
  } finally {
    locationPosting = false;
  }
}

async function submitChat(message) {
  if (!SERVER_URL || !leaderboardJoined || !state?.playerActive || socialPosting) return;
  socialPosting = true;
  render();
  try {
    const chatRequest = await withApp(() => app.serverChat(SERVER_URL, message, Date.now()));
    await postServerAction('/v1/chat', chatRequest);
    chatInput.value = '';
    chatStatus.textContent = '';
    await refreshSocial();
  } catch (error) {
    chatStatus.textContent = String(error);
  } finally {
    socialPosting = false;
    render();
  }
}

async function updateDelegation() {
  if (
    !SERVER_URL
    || !leaderboardJoined
    || !state?.playerActive
    || !delegationAvailable
    || socialPosting
  ) return;
  socialPosting = true;
  render();
  const enabled = !delegatedRenewal;
  try {
    const delegationRequest = await withApp(() => (
      app.serverDelegation(SERVER_URL, enabled, Date.now())
    ));
    await postServerAction('/v1/delegation', delegationRequest);
    delegatedRenewal = enabled;
    chatStatus.textContent = enabled
      ? 'Server renewal delegation enabled.'
      : 'Server renewal delegation disabled.';
    await refreshSocial();
  } catch (error) {
    chatStatus.textContent = String(error);
  } finally {
    socialPosting = false;
    render();
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
  for (let index = 0; index < queue.length && foundKey == null; index += 1) {
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
  const path = findWalkPath(targets);
  if (path == null) {
    status.textContent = 'That tile is unreachable.';
    return false;
  }
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
  void publishLocation();
  return true;
}

async function handleMapClick(event) {
  if (!(event.target instanceof Element)) return;
  const cell = event.target.closest('.map-cell');
  if (!cell || !state) return;
  if (chopping) {
    attemptChop();
    return;
  }
  if (busy) return;
  const x = Number(cell.dataset.x);
  const y = Number(cell.dataset.y);
  const treeId = cell.dataset.treeId ? Number(cell.dataset.treeId) : null;
  if (treeId != null) {
    const tree = worldTrees().find((candidate) => candidate.treeId === treeId);
    if (!tree) return;
    focusedTreeId = treeId;
    lockedTreeId = tree.health > 0 ? treeId : null;
    if (tree.health === 0) {
      status.textContent = `Tree #${treeId} is a stump.`;
      render();
      return;
    }
    const destinations = DIRECTIONS
      .map(([dx, dy]) => ({ x: tree.x + dx, y: tree.y + dy }))
      .filter((position) => (
        isMapCoordinate(position.x, position.y)
        && !worldTrees().some((candidate) => (
          candidate.x === position.x && candidate.y === position.y
        ))
      ));
    if (await walkTo(destinations, treeId)) {
      if (state.playerActive && state.fundingReady) {
        attemptChop();
      } else {
        lockedTreeId = null;
        status.textContent = 'Create a player to chop this tree.';
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
function followPlayerCamera(force = false) {
  if (!state?.playerActive) return;
  const position = coordinateKey(player.x, player.y);
  if (!force && position === lastCameraPosition) return;
  lastCameraPosition = position;
  requestAnimationFrame(() => {
    const cell = map.querySelector(
      `.map-cell[data-x="${player.x}"][data-y="${player.y}"]`,
    );
    if (!cell) return;
    const reducedMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    const viewportBounds = mapViewport.getBoundingClientRect();
    const cellBounds = cell.getBoundingClientRect();
    const targetLeft = Math.max(
      0,
      mapViewport.scrollLeft
        + cellBounds.left
        + cellBounds.width / 2
        - viewportBounds.left
        - viewportBounds.width / 2,
    );
    const targetTop = Math.max(
      0,
      mapViewport.scrollTop
        + cellBounds.top
        + cellBounds.height / 2
        - viewportBounds.top
        - viewportBounds.height / 2,
    );
    mapViewport.scrollTo({
      left: targetLeft,
      top: targetTop,
      behavior: cameraInitialized && !reducedMotion ? 'smooth' : 'auto',
    });
    cameraInitialized = true;
    globalThis.__WOODLAND_E2E_CAMERA = {
      x: player.x,
      y: player.y,
      targetLeft,
      targetTop,
    };
  });
}

function renderMap() {
  const width = state?.mapWidth || DEFAULT_MAP_WIDTH;
  const height = state?.mapHeight || DEFAULT_MAP_HEIGHT;
  const trees = new Map(worldTrees().map((tree) => [`${tree.x}:${tree.y}`, tree]));
  const playersByPosition = new Map();
  for (const location of remoteLocations) {
    if (location.playerAsset === state?.playerAsset) continue;
    const key = coordinateKey(location.x, location.y);
    const present = playersByPosition.get(key) || [];
    present.push(location.playerAsset);
    playersByPosition.set(key, present);
  }
  const cells = document.createDocumentFragment();
  map.style.setProperty('--map-width', width);
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const cell = document.createElement('span');
      const tree = trees.get(`${x}:${y}`);
      cell.className = 'map-cell';
      cell.dataset.x = String(x);
      cell.dataset.y = String(y);
      cell.setAttribute('aria-hidden', 'true');
      cell.textContent = '.';
      if (tree) {
        cell.dataset.treeId = String(tree.treeId);
        cell.classList.add(tree.health === 0 ? 'stump' : 'tree');
        if (treeEffect?.treeId === tree.treeId) {
          cell.classList.add(treeEffect.type === 'log' ? 'log-flash' : 'chop-flash');
          cell.dataset.effect = treeEffect.type;
        }
        cell.textContent = tree.health === 0 ? '+' : TREE_GLYPH;
        cell.title = tree.health === 0
          ? `Tree #${tree.treeId}: stump`
          : `Tree #${tree.treeId}: ${tree.health} LOG`;
      }
      if (tree?.treeId === lockedTreeId) {
        cell.classList.add('locked');
      }
      if (
        (tree && tree.treeId === walkingTargetTreeId)
        || (!tree && walkingTarget?.x === x && walkingTarget?.y === y)
      ) {
        cell.classList.add('target');
      }
      const remotePlayers = playersByPosition.get(coordinateKey(x, y)) || [];
      if (!tree && remotePlayers.length && (player.x !== x || player.y !== y)) {
        cell.classList.add('remote-player');
        cell.textContent = remotePlayers.length === 1 ? '&' : String(remotePlayers.length);
        cell.title = remotePlayers
          .map((asset) => `${asset.slice(0, 8)}…`)
          .join(', ');
      }
      if (player.x === x && player.y === y) {
        cell.className = 'map-cell player';
        cell.textContent = '@';
        cell.title = `Player at (${x}, ${y})`;
      }
      cells.append(cell);
    }
  }
  map.replaceChildren(cells);

  const adjacent = adjacentTree();
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
  else if (!state.playerActive) mapHint.textContent = 'Fund and activate the player.';
  else if (!state.fundingReady) mapHint.textContent = 'Player state is reconciling.';
  else if (adjacent?.health === 0) {
    mapHint.textContent = adjacent.respawnInSeconds == null
      ? `Tree #${adjacent.treeId} has exhausted its LOG and XP reserve.`
      : `Tree #${adjacent.treeId} returns in ${adjacent.respawnInSeconds}s.`;
  }
  else if (adjacent) mapHint.textContent = `In range of tree #${adjacent.treeId}. Click the tree to chop until LOG.`;
  else mapHint.textContent = 'Click a tile to walk, or click a tree to walk there and chop.';
  followPlayerCamera();

  globalThis.__WOODLAND_E2E_PLAYER = { ...player };
  globalThis.__WOODLAND_E2E_ADJACENT = Boolean(adjacent);
  globalThis.__WOODLAND_E2E_ADJACENT_TREE = adjacent;
  globalThis.__WOODLAND_E2E_LOCKED_TREE = lockedTreeId;
}


function fundingMessage() {
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
  if (!state) return;

  const totalLogs = worldTrees().reduce((sum, tree) => sum + tree.logReserveRemaining, 0);
  const standingTrees = worldTrees().filter((tree) => tree.health > 0).length;
  const tree = focusedTree();
  const playerActive = Boolean(state.playerActive);

  onboarding.hidden = playerActive;
  dashboard.classList.toggle('player-active', playerActive);
  bagPanel.hidden = !playerActive;
  statsPanel.hidden = !playerActive;
  leaderboardPanel.hidden = !SERVER_URL;
  joinLeaderboardButton.hidden = !playerActive || leaderboardJoined;
  joinLeaderboardButton.disabled = busy || serverRegistrationSyncing || socialPosting;
  delegateRenewalButton.hidden = !playerActive || !leaderboardJoined || !delegationAvailable;
  delegateRenewalButton.disabled = busy || socialPosting;
  delegateRenewalButton.textContent = delegatedRenewal
    ? 'Stop delegated renewals'
    : 'Delegate renewals';
  chatInput.disabled = !playerActive || !leaderboardJoined || socialPosting;
  sendChatButton.disabled = chatInput.disabled || !chatInput.value.trim();
  renderLeaderboard();
  renderChat();
  address.textContent = state.address;
  copyAddressButton.disabled = !state.address;
  fundingInstructionElement.textContent = fundingMessage();


  onboardingNote.classList.toggle('error', Boolean(state.activationBlockedReason));
  if (state.activationBlockedReason) {
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
  xpNumber.textContent = `${state.playerXp} XP`;
  xpNext.textContent = state.playerNextLevelXp == null
    ? 'Maximum level'
    : `${Math.max(0, state.playerNextLevelXp - state.playerXp)} XP to level ${state.playerLevel + 1}`;
  xpBacking.textContent = `${state.seasonXpRemaining} season XP remaining`;
  logChance.textContent = `${state.logDropBasisPoints / 100}%`;
  forestHealth.textContent = `${totalLogs} LOG reserve / ${standingTrees} standing`;
  const logCount = state.playerLogs || 0;
  playerLogs.textContent = String(logCount);
  logSlot.classList.toggle('empty', logCount === 0);
  logSlot.setAttribute('aria-label', `${logCount} LOG in inventory`);

  const stumps = worldTrees().filter((candidate) => candidate.health === 0);
  const timedStumps = stumps.filter((candidate) => candidate.respawnInSeconds != null);
  const nextRespawn = timedStumps.length
    ? Math.min(...timedStumps.map((candidate) => candidate.respawnInSeconds))
    : null;
  regrowReady.textContent = stumps.length
    ? `${stumps.length}${nextRespawn == null ? ' / depleted' : ` / next in ${nextRespawn}s`}`
    : '0';
  emulator.textContent = `${state.emulatorVersion} / ${state.emulatorSigner.slice(0, 12)}...`;

  activateButton.disabled = walking || busy || playerActive || !state.activationReady;
  activateButton.textContent = busy && !playerActive ? 'Creating player...' : 'Create player';
  resetProfileButton.hidden = playerActive || !localStorage.getItem(PROFILE);
  resetProfileButton.disabled = walking || busy || playerActive;
  resetButton.disabled = walking || busy;
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
    element('tree-health').textContent = tree.health === 0 ? 'stump' : `${tree.health} active`;
    element('tree-reserve').textContent = `${tree.logReserveRemaining} LOG`;
    element('tree-xp').textContent = `${tree.xpRemaining} XP`;
    element('player-asset').textContent = state.playerAsset || 'not issued';
    element('tree-asset').textContent = state.treeAsset;
    element('log-asset').textContent = state.logAsset;
    element('xp-asset').textContent = state.xpAsset;
    element('tree-value').textContent = `${tree.valueSats} sats fixed`;
    element('tree-outpoint').textContent = tree.treeOutpoint;
    element('last-chop').textContent = tree.lastAttemptTxid || 'none';
  }
  globalThis.__WOODLAND_E2E_STATE = state;
}

async function run(label, action, completion = () => 'Success') {
  if (busy) return;
  globalThis.__WOODLAND_E2E_ERROR = null;
  setBusy(true, label);
  appendLog(label);
  try {
    state = await withApp(action);
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
  if (app) localStorage.setItem(PROFILE, app.exportProfile());
}

function persistPosition() {
  if (!state?.playerActive) return;
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

async function renewPlayer() {
  return app.renewPlayer();
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
    () => (resume ? app.resumePendingChop() : app.refresh()),
    () => (resume ? 'Pending swing reconciled.' : 'Refresh complete.'),
  );
});

activateButton.addEventListener('click', () => (
  run('Issuing PLAYER_ID into owner-authorized player state...', activatePlayer)
));

async function waitForSwingCadence(startedAt) {
  let remaining = CHOP_SWING_MS - (performance.now() - startedAt);
  while (remaining > 0 && !stopChopping) {
    await new Promise((resolve) => setTimeout(resolve, Math.min(50, remaining)));
    remaining = CHOP_SWING_MS - (performance.now() - startedAt);
  }
}

async function chopUntilLog(treeId) {
  let swings = 0;
  let success = false;
  const runStartedAt = performance.now();
  chopping = true;
  stopChopping = false;
  render();
  try {
    while (!stopChopping) {
      const tree = worldTrees().find((candidate) => candidate.treeId === treeId);
      if (!tree || tree.health === 0) break;
      flashTree(treeId, 'chop', CHOP_FLASH_MS);
      const startedAt = performance.now();
      status.textContent = `Swinging at tree #${treeId} (${swings + 1})...`;
      const nextState = await app.chopExpected(
        treeId,
        tree.treeOutpoint,
        state.playerStateOutpoint,
        tree.nextDrop,
      );
      await waitForSwingCadence(startedAt);
      state = nextState;
      swings += 1;
      success = state.lastAttempt?.success === true;
      if (success) flashTree(treeId, 'log', LOG_FLASH_MS, false);
      render();
      if (success) break;
    }
    return state;
  } finally {
    lastChopRun = {
      treeId,
      swings,
      success,
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

function attemptChop() {
  if (chopping) {
    stopChopping = true;
    status.textContent = 'Stopping after the current swing...';
    renderMap();
    return;
  }
  if (walking) cancelWalking();
  const tree = adjacentTree();
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
        return `You get a LOG and 1 XP after ${lastChopRun.swings} ${suffix}.`;
      }
      if (lastChopRun?.cancelled) {
        return `Stopped after ${lastChopRun.swings} accepted swing${lastChopRun.swings === 1 ? '' : 's'}.`;
      }
      return 'The tree is unavailable.';
    },
  );
}

copyAddressButton.addEventListener('click', () => { void copyAddress(); });

joinLeaderboardButton.addEventListener('click', async () => {
  if (!state?.playerActive || !SERVER_URL || serverRegistrationSyncing) return;
  leaderboardJoined = true;
  joinLeaderboardButton.disabled = true;
  leaderboardStatus.textContent = 'Verifying player state...';
  try {
    await syncServerRegistration(true);
    render();
  } catch (error) {
    leaderboardJoined = false;
    localStorage.removeItem(SERVER_REGISTRATION);
    leaderboardStatus.textContent = `Server registration failed: ${error}`;
    render();
  }
});

delegateRenewalButton.addEventListener('click', () => { void updateDelegation(); });
chatInput.addEventListener('input', () => {
  sendChatButton.disabled = chatInput.disabled || !chatInput.value.trim() || socialPosting;
});
chatForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const message = chatInput.value.trim();
  if (message) void submitChat(message);
});


renewButton.addEventListener('click', () => {
  run(
    'Renewing player state through a fresh Ark batch...',
    renewPlayer,
    () => 'Player session renewed.',
  );
});
map.addEventListener('click', (event) => { void handleMapClick(event); });


resetButton.addEventListener('click', () => {
  if (busy || !window.confirm('Forget this local test key and create a new wallet?')) return;
  localStorage.removeItem(KEY);
  localStorage.removeItem(PROFILE);
  localStorage.removeItem(pendingStorageKey);
  localStorage.removeItem(POSITION);
  localStorage.removeItem(SERVER_REGISTRATION);
  location.reload();
});

resetProfileButton.addEventListener('click', () => {
  if (
    busy
    || state?.playerActive
    || !window.confirm('Forget the saved world profile but keep this funded wallet key?')
  ) return;
  localStorage.removeItem(PROFILE);
  localStorage.removeItem(POSITION);
  localStorage.removeItem(SERVER_REGISTRATION);
  location.reload();
});

async function boot() {
  if (busy) return;
  busy = true;
  globalThis.__WOODLAND_E2E_ERROR = null;
  status.classList.remove('error');
  status.textContent = 'Connecting directly to Arkade and the emulator...';
  try {
    await init();
    const worldResponse = await fetch(WORLD, { cache: 'no-store' });
    if (!worldResponse.ok) {
      throw new Error(`woodland.sh world unavailable (${worldResponse.status})`);
    }
    const world = await worldResponse.text();
    const manifest = JSON.parse(world);
    pendingStorageKey = `woodland.sh:web:v1:pending:${manifest.arkadeServiceUrl.replace(/\/+$/, '')}`;
    app = await WoodlandApp.init(
      manifest.arkadeServiceUrl,
      manifest.emulatorUrl,
      world,
      localStorage.getItem(KEY) || undefined,
      localStorage.getItem(PROFILE) || undefined,
    );
    localStorage.setItem(KEY, app.exportKey());
    state = await app.refresh();
    persistProfile();
    restorePosition();
    if (state.pendingChopTxid) {
      appendLog(`Resuming pending swing ${state.pendingChopTxid}...`);
      try {
        state = await app.resumePendingChop();
        appendLog('Pending swing reconciled.');
      } catch (error) {
        const message = `Pending swing remains unresolved: ${error}`;
        status.classList.add('error');
        status.textContent = message;
        appendLog(message);
        globalThis.__WOODLAND_E2E_ERROR = String(error);
      }
    }
    if (!state.pendingChopTxid) status.textContent = 'Ready';
    appendLog('Connected to woodland.sh');
    restoreServerRegistration();
    await refreshSocial();
    if (leaderboardJoined) {
      await syncServerRegistration(true).catch((error) => {
        leaderboardStatus.textContent = `Server registration failed: ${error}`;
      });
    }
    globalThis.__WOODLAND_E2E_APP = app;
    globalThis.__WOODLAND_E2E_SERVER_REGISTRATION = () => (
      withApp(() => app.serverRegistration(SERVER_URL))
    );
    globalThis.__WOODLAND_E2E_SERVER_LOCATION = (x, y) => (
      withApp(() => app.serverLocation(SERVER_URL, x, y, Date.now()))
    );
    globalThis.__WOODLAND_E2E_UNAUTHORIZED_REISSUANCE = () => (
      withApp(() => app.testUnauthorizedReissuance())
    );
    globalThis.__WOODLAND_E2E_INVALID_XP = async (treeId) => {
      await withApp(() => app.testInvalidXpTransition(treeId));
      return withApp(() => app.refresh());
    };
    globalThis.__WOODLAND_E2E_INVALID_ASSET_ORDER = async (treeId) => {
      await withApp(() => app.testInvalidAssetGroupOrder(treeId));
      return withApp(() => app.refresh());
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
    globalThis.__WOODLAND_E2E_CHOP_EXPECTED = async (...args) => {
      try {
        state = await withApp(() => app.chopExpected(...args));
        render();
        return { ok: true, state };
      } catch (error) {
        return { ok: false, message: String(error) };
      }
    };
    globalThis.__WOODLAND_E2E_RESUME_PENDING = async () => {
      state = await withApp(() => app.resumePendingChop());
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_RENEW_PLAYER = async () => {
      state = await withApp(renewPlayer);
      render();
      return state;
    };
    globalThis.__WOODLAND_E2E_READY = true;
  } finally {
    busy = false;
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
  try {
    state = await withApp(() => app.refresh());
    if (
      state.playerActive
      && !state.pendingChopTxid
      && state.playerStateExpiresInSeconds != null
      && state.playerRolloverMarginSeconds != null
      && state.playerStateExpiresInSeconds < state.playerRolloverMarginSeconds
      && !delegatedRenewal
    ) {
      status.textContent = 'Rolling player state into a fresh Arkade batch...';
      state = await renewPlayer();
    }
    persistProfile();
    render();
    void syncServerRegistration().catch((error) => {
      leaderboardStatus.textContent = `Server registration failed: ${error}`;
    });
  } catch {}
  finally { polling = false; }

}, 1000);
window.addEventListener('resize', () => {
  followPlayerCamera(true);
});

setInterval(() => {
  void refreshSocial();
  void publishLocation();
}, 2_000);
