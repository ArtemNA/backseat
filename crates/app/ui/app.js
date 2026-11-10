const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const el = (tag, attrs = {}, ...kids) => {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') e.className = v;
    else if (k === 'text') e.textContent = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) e.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids) if (k != null) e.append(k);
  return e;
};

const PRESETS = [2000, 4000, 8000, 12000, 15000, 20000, 35000, 50000];
// The streamer's own upload limit in Mbit/s, or 0 when unknown.
function uploadLimit() {
  try { return Math.max(0, +localStorage.getItem('uploadLimit') || 0); } catch { return 0; }
}
function setUploadLimit(v) {
  try { if (v > 0) localStorage.setItem('uploadLimit', String(v)); else localStorage.removeItem('uploadLimit'); } catch {}
}
const fmt = (v) => v.toLocaleString(lang, { maximumFractionDigits: 1 });
const VIEW_MODES = ['1440p60', '1080p60', '1080p30', '720p60', '720p30'];
const state = { snap: null, current: null, joinOpen: false, joinMode: 'create', videosHidden: false, choice: {}, cardsKey: '', liveKey: '', gl: { tab: 'screen', pick: null, res: '1080', fps: '60', kbit: 20000, codec: 'h265', sources: null, thumbs: {} } };

// Pictures for the visible tab of the source picker, every 2 s while it is open.
async function refreshThumbs() {
  const g = state.gl;
  if (!$('golive').open || !g.sources) return;
  const tab = g.tab;
  const ids = tab === 'screen' ? g.sources.monitors.map((m) => m.handle) : g.sources.windows.map((w) => w.hwnd);
  const shots = await invoke('thumbnails', { kind: tab === 'screen' ? 'monitor' : 'window', ids });
  for (const s of shots) g.thumbs[`${tab}:${s.id}`] = s;
  // Swap pictures in place, so the grid does not jump.
  for (const s of shots) {
    const card = document.querySelector(`#gl-sources [data-src="${s.id}"] .shot`);
    if (!card || !s.image) continue;
    const img = card.querySelector('img');
    if (img) img.src = s.image; else card.replaceChildren(el('img', { src: s.image, alt: '' }));
  }
}
setInterval(refreshThumbs, 2000);

// ---------- language ----------
function applyStrings() {
  document.querySelectorAll('[data-t]').forEach((n) => { n.textContent = t(n.dataset.t); });
  $('leave').setAttribute('aria-label', t('room.leave'));
  $('invite').setAttribute('aria-label', t('room.invite'));
  $('invite').title = t('room.invite');
  renderJoinForm();
  state.cardsKey = '';
  state.liveKey = '';
  $('rail').dataset.key = '';
  if (state.snap) render(state.snap);
}
function fillLanguages() {
  const sel = $('f-lang');
  sel.replaceChildren(...LANGS.map((l) => el('option', { value: l.id, text: l.name, selected: l.id === lang })));
  sel.addEventListener('change', () => { setLanguage(sel.value); applyStrings(); });
}

// ---------- helpers ----------
const COLORS = ['#c2622d', '#3a8a5c', '#5d5fa8', '#7a4e9a', '#2f7f95', '#9a4e6b', '#7a6a2e'];
function avatar(name) {
  let h = 0;
  for (const c of name) h = (h * 31 + c.codePointAt(0)) >>> 0;
  return el('span', { class: 'avatar', style: `background:${COLORS[h % COLORS.length]}`, text: (name[0] || '?').toUpperCase() });
}
const mbit = (kbit) => (kbit / 1000).toLocaleString(lang);
function clock(secs) {
  const h = Math.floor(secs / 3600), m = Math.floor((secs % 3600) / 60), s = secs % 60;
  return [h, m, s].map((x) => String(x).padStart(2, '0')).join(':');
}
// "720p30 2000" -> "720p30 · 2 Mbit/s"
function requestLabel(q) {
  if (!q) return t('card.source_quality');
  const [mode, kbit] = q.split(' ');
  return kbit ? `${mode} · ${mbit(+kbit)} ${t('unit.mbit')}` : mode;
}
const ms = (v) => (v >= 1 ? `${Math.round(v)} ${t('unit.ms')}` : v > 0 ? `<1 ${t('unit.ms')}` : '—');
// "1280x720@30 4000 kbit/s" -> "720p30 · 4"
function shortQuality(q) {
  const m = /(\d+)x(\d+)@(\d+) (\d+) kbit/.exec(q || '');
  return m ? `${m[2]}p${m[3]} · ${mbit(+m[4])}` : q || '—';
}
async function call(cmd, args, errBox) {
  try {
    return await invoke(cmd, args);
  } catch (e) {
    if (errBox) { errBox.textContent = String(e); errBox.classList.remove('hidden'); }
    throw e;
  }
}

// ---------- saved rooms ----------
// Rooms the user has been in, shown in the left rail like Discord servers. Only the open
// one is joined; the others are shortcuts.
function savedRooms() {
  try { return JSON.parse(localStorage.getItem('rooms') || '[]'); } catch { return []; }
}
function saveRooms(list) {
  try { localStorage.setItem('rooms', JSON.stringify(list)); } catch {}
}
const sameRoom = (a, b) => a && b && a.room === b.room && a.server === b.server;
// Rooms made here are named by a random invite code (Crockford base32, 12 symbols, about
// 60 bits): nobody finds them by guessing. Older rooms are plain names anyone can type.
const CODE_ABC = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
const CODE_RE = /^[0-9A-HJKMNP-TV-Z]{12}$/;
function newCode() {
  const b = new Uint8Array(12);
  crypto.getRandomValues(b);
  return [...b].map((x) => CODE_ABC[x & 31]).join('');
}
// A code or a link carrying one, as typed or pasted; null if it is neither.
function parseCode(text) {
  let v = text.trim();
  const m = v.match(/(?:\/j\/|join\/)([0-9A-Za-z-]+)/);
  if (m) v = m[1];
  v = v.toUpperCase().replace(/[\s-]/g, '').replace(/O/g, '0').replace(/[IL]/g, '1').replace(/U/g, 'V');
  return CODE_RE.test(v) ? v : null;
}
const isCode = (room) => CODE_RE.test(room);
const fmtCode = (c) => c.match(/.{1,4}/g).join('-');
const roomLabel = (r) => (r && (r.title || r.room)) || '';
function showToast(text) {
  const tb = $('toast');
  tb.textContent = text;
  tb.classList.remove('hidden');
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => tb.classList.add('hidden'), 6000);
}
// The https page for a room's invite, on the room's own server (ws -> http, wss -> https).
function inviteLink(r) {
  try {
    const u = new URL(r.server.replace(/^ws/, 'http'));
    return `${u.protocol}//${u.host}/j/${fmtCode(r.room)}`;
  } catch { return ''; }
}
// An invite link from a click (backseat://join/CODE): go there.
async function handleLink(link) {
  const code = parseCode(link);
  if (!code) return;
  // The same link twice (the page opened the app and the button was pressed too): once.
  if ((state.current && state.current.room === code) || state.linkBusy === code) return;
  state.linkBusy = code;
  setTimeout(() => { if (state.linkBusy === code) state.linkBusy = null; }, 10000);
  const known = savedRooms().find((x) => x.room === code);
  if (known) { if (!sameRoom(known, state.current)) await switchRoom(known); return; }
  const fixed = await invoke('built_in_server');
  const server = fixed || settings().server || (state.current && state.current.server) || $('f-server').value.trim();
  let name = (state.current && state.current.name) || '';
  try { name = name || JSON.parse(localStorage.getItem('join') || '{}').name || ''; } catch {}
  if (name) { await switchRoom({ server, room: code, name }); return; }
  // First time: ask for a name, the code is filled in.
  state.joinMode = 'code';
  $('f-code').value = fmtCode(code);
  state.joinOpen = true;
  renderJoinForm();
  tick();
}
async function copyInvite(r) {
  const text = isCode(r.room) ? t('invite.text', { title: roomLabel(r), code: fmtCode(r.room), link: inviteLink(r) }) : r.room;
  try { await navigator.clipboard.writeText(text); } catch {
    const ta = el('textarea', { style: 'position:fixed;left:-9999px' });
    ta.value = text; document.body.append(ta); ta.select(); document.execCommand('copy'); ta.remove();
  }
  showToast(isCode(r.room) ? t('room.copied', { code: fmtCode(r.room) }) : t('room.copied_plain', { code: r.room }));
}
function rememberRoom(r) {
  const list = savedRooms();
  const i = list.findIndex((x) => sameRoom(x, r));
  if (i >= 0) list[i] = r; else list.push(r);
  saveRooms(list);
}
function initials(room) {
  const words = room.split(/[\s\-_.]+/).filter(Boolean);
  const two = words.length > 1 ? words[0][0] + words[1][0] : room.slice(0, 2);
  return two.toUpperCase();
}
async function joinRoom(r) {
  await call('connect', { server: r.server, room: r.room, name: r.name, title: r.title || null }, $('join-error'));
  state.current = r;
  rememberRoom(r);
  try { localStorage.setItem('lastRoom', JSON.stringify(r)); } catch {}
  try { localStorage.setItem('join', JSON.stringify(r)); } catch {}
  state.joinOpen = false;
  state.cardsKey = '';
  tick();
}
async function switchRoom(r) {
  $('join-error').classList.add('hidden');
  try {
    await joinRoom(r);
  } catch {
    // Show the join screen with this room filled in and the error, unless a direct
    // connection is being set up meanwhile (the start-up join can fail seconds later).
    if (state.joinMode !== 'direct') {
      $('f-name').value = r.name;
      if (isCode(r.room)) { state.joinMode = 'code'; $('f-code').value = fmtCode(r.room); }
    }
    state.joinOpen = true;
    tick();
  }
}
function renderRail(s) {
  const list = savedRooms();
  const cur = state.current;
  if (cur && !list.some((x) => sameRoom(x, cur))) list.push(cur);
  const key = JSON.stringify(list) + '|' + JSON.stringify(cur) + '|' + lang + '|' + (state.update ? state.update.latest : '');
  if ($('rail').dataset.key === key) return;
  $('rail').dataset.key = key;
  $('rail').replaceChildren(
    ...list.map((r) => el('button', {
      class: 'r' + (sameRoom(r, cur) ? ' on' : ''), type: 'button', title: roomLabel(r), 'data-room': r.room, text: initials(roomLabel(r)),
      onclick: () => { if (!sameRoom(r, state.current)) switchRoom(r); },
      oncontextmenu: (e) => { e.preventDefault(); openRoomMenu(e, r); },
    })),
    el('div', { class: 'sep' }),
    el('button', { class: 'r add', type: 'button', title: t('rooms.add'), text: '+', onclick: () => { state.joinOpen = true; tick(); } }),
    ...(state.update ? [el('button', { class: 'r update', type: 'button', title: t('set.available', { v: state.update.latest }), text: '↓', onclick: () => openSettings('updates') })] : []));
}
function closeRoomMenu() { $('room-menu').classList.add('hidden'); }
function openRoomMenu(e, r) {
  const m = $('room-menu');
  const item = (text, fn, cls = '') => el('button', { type: 'button', class: cls, text, onclick: () => { closeRoomMenu(); fn(); } });
  m.replaceChildren(
    sameRoom(r, state.current) ? null : item(t('rooms.open'), () => switchRoom(r)),
    item(t('rooms.copy_invite'), () => copyInvite(r)),
    item(t('rooms.rename'), () => openRename(r)),
    item(t('rooms.remove'), () => { saveRooms(savedRooms().filter((x) => !sameRoom(x, r))); $('rail').dataset.key = ''; tick(); }, 'danger'));
  m.style.left = `${e.clientX}px`;
  m.style.top = `${e.clientY}px`;
  m.classList.remove('hidden');
}
document.addEventListener('click', (e) => { if (!$('room-menu').contains(e.target)) closeRoomMenu(); });
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closeRoomMenu(); });
let renaming = null;
function openRename(r) {
  renaming = r;
  $('rename-title').textContent = t('rename.title', { room: roomLabel(r) });
  $('rename-name').value = r.name;
  $('rename').showModal();
}
$('rename').addEventListener('close', async () => {
  const r = renaming;
  renaming = null;
  if (!r || $('rename').returnValue !== 'save') return;
  const name = $('rename-name').value.trim();
  if (!name || name === r.name) return;
  const next = { ...r, name };
  saveRooms(savedRooms().map((x) => (sameRoom(x, r) ? next : x)));
  $('rail').dataset.key = '';
  // In the open room the new name takes effect by joining again.
  if (sameRoom(r, state.current)) await switchRoom(next); else tick();
});

// ---------- join ----------
function loadForm() {
  try {
    const saved = JSON.parse(localStorage.getItem('join') || '{}');
    for (const k of ['server', 'name']) if (saved[k]) $('f-' + k).value = saved[k];
  } catch {}
  // The default server from the settings comes first.
  if (settings().server) $('f-server').value = settings().server;
  // A build made for one server has no server field: the address is fixed. It also becomes
  // the default server, so a later build without one (an update) keeps using it.
  invoke('built_in_server').then((url) => {
    if (!url) return;
    $('f-server').value = url;
    $('f-server').closest('label').classList.add('hidden');
    if (!settings().server) saveSettings({ ...settings(), server: url });
  });
}

// ---------- settings ----------
function settings() {
  const base = { tray: true, autoUpdate: true, server: '' };
  try { return { ...base, ...JSON.parse(localStorage.getItem('settings') || '{}') }; } catch { return base; }
}
function saveSettings(s) {
  try { localStorage.setItem('settings', JSON.stringify(s)); } catch {}
}
function setTab(tab) {
  for (const b of document.querySelectorAll('.set-tabs button')) b.setAttribute('aria-pressed', String(b.dataset.tab === tab));
  for (const p of ['general', 'stream', 'watch', 'updates']) $('set-' + p).classList.toggle('hidden', tab !== p);
}
// Defaults for going live and the most to receive when watching ('' = auto / as the streamer).
function streamPrefs() { return { res: '', fps: '', kbit: '', codec: '', ...(settings().stream || {}) }; }
function watchPrefs() { return { res: '', fps: '', kbit: '', ...(settings().watch || {}) }; }
// What a new watch asks for: the preferred maximum, which the streamer clamps to what it
// sends (each part separately). Null: the streamer's own quality.
function watchRequest() {
  const w = watchPrefs();
  if (!w.res && !w.fps && !w.kbit) return null;
  return `${w.res || '1440'}p${w.fps || '60'}${w.kbit ? ' ' + w.kbit : ''}`;
}
function fillPrefSelect(id, items, value, onChange) {
  const sel = $(id);
  sel.replaceChildren(...items.map(([v, label]) => el('option', { value: v, text: label, selected: String(v) === String(value) })));
  sel.onchange = () => onChange(sel.value);
}
function fillPrefs() {
  const s = streamPrefs(), w = watchPrefs();
  const saveS = (k) => (v) => saveSettings({ ...settings(), stream: { ...streamPrefs(), [k]: v } });
  const saveW = (k) => (v) => saveSettings({ ...settings(), watch: { ...watchPrefs(), [k]: v } });
  const kbits = PRESETS.map((k) => [String(k), `${mbit(k)} ${t('unit.mbit')}`]);
  fillPrefSelect('set-s-res', [['', t('set.auto_res')], ['720', '720p'], ['1080', '1080p'], ['1440', '1440p']], s.res, saveS('res'));
  fillPrefSelect('set-s-fps', [['', t('set.auto')], ['30', '30'], ['60', '60']], s.fps, saveS('fps'));
  fillPrefSelect('set-s-kbit', [['', t('set.auto')], ...kbits], s.kbit, saveS('kbit'));
  fillPrefSelect('set-s-codec', [['', t('set.auto')], ['av1', 'AV1'], ['h265', 'H.265'], ['h264', 'H.264']], s.codec, saveS('codec'));
  fillPrefSelect('set-s-audio', [['', t('set.audio_auto')], ['system', t('golive.audio_system')], ['none', t('golive.audio_none')]], s.audio || '', saveS('audio'));
  fillPrefSelect('set-w-res', [['', t('set.as_streamer')], ['1440', '1440p'], ['1080', '1080p'], ['720', '720p']], w.res, saveW('res'));
  fillPrefSelect('set-w-fps', [['', t('set.as_streamer')], ['60', '60'], ['30', '30']], w.fps, saveW('fps'));
  fillPrefSelect('set-w-kbit', [['', t('set.as_streamer')], ...kbits], w.kbit, saveW('kbit'));
}
async function openSettings(tab) {
  const s = settings();
  const fixed = await invoke('built_in_server');
  $('set-server').value = s.server || $('f-server').value.trim();
  $('set-server').disabled = !!fixed;
  $('set-server-hint').textContent = fixed ? t('set.server_fixed', { url: fixed }) : t('set.server_hint');
  $('set-lang').replaceChildren(...LANGS.map((l) => el('option', { value: l.id, text: l.name, selected: l.id === lang })));
  $('set-tray').checked = s.tray;
  $('set-auto').checked = s.autoUpdate;
  $('set-version').textContent = t('set.version', { v: await invoke('app_version') });
  fillPrefs();
  showUpdate();
  setTab(tab || 'general');
  if (!$('settings').open) $('settings').showModal();
}
function showUpdate() {
  $('set-install').classList.toggle('hidden', !state.update);
  if (state.update) $('set-status').textContent = t('set.available', { v: state.update.latest });
}
for (const b of document.querySelectorAll('.set-tabs button')) b.addEventListener('click', () => setTab(b.dataset.tab));
$('settings-btn').addEventListener('click', () => openSettings());
$('join-settings').addEventListener('click', () => openSettings());
$('set-server').addEventListener('change', () => {
  const v = $('set-server').value.trim();
  saveSettings({ ...settings(), server: v });
  if (v) $('f-server').value = v;
});
$('set-lang').addEventListener('change', () => { setLanguage($('set-lang').value); $('main-header').dataset.key = ''; applyStrings(); $('f-lang').value = lang; });
$('set-tray').addEventListener('change', () => { saveSettings({ ...settings(), tray: $('set-tray').checked }); invoke('set_close_to_tray', { on: $('set-tray').checked }); });
$('set-auto').addEventListener('change', () => saveSettings({ ...settings(), autoUpdate: $('set-auto').checked }));
// Looks for a newer release; with `install` and one found, installs it (the app restarts).
async function checkUpdate(manual, install) {
  if (manual) $('set-status').textContent = t('set.checking');
  try {
    const r = await invoke('check_update');
    state.update = r.newer ? r : null;
    $('rail').dataset.key = '';
    if (manual) $('set-status').textContent = r.newer ? t('set.available', { v: r.latest }) : t('set.uptodate');
    showUpdate();
    if (r.newer && install) await installUpdate();
  } catch (e) {
    if (manual) $('set-status').textContent = t('set.failed', { e: String(e) });
  }
}
async function installUpdate() {
  $('set-status').textContent = t('set.installing');
  showToast(t('set.installing'));
  try { await invoke('install_update'); } catch (e) { $('set-status').textContent = t('set.failed', { e: String(e) }); showToast(t('set.failed', { e: String(e) })); }
}
$('set-logs').addEventListener('click', () => invoke('open_logs').catch((e) => { $('set-status').textContent = String(e); }));
$('set-check').addEventListener('click', () => checkUpdate(true, false));
$('set-install').addEventListener('click', () => installUpdate());
// The form makes a new room or joins one by its invite.
function renderJoinForm() {
  const mode = state.joinMode;
  seg($('join-mode'), [['create', t('join.create_mode')], ['code', t('join.code_mode')], ['direct', t('join.direct_mode')]], mode, (v) => { state.joinMode = v; renderJoinForm(); });
  $('row-title').classList.toggle('hidden', mode !== 'create');
  $('row-code').classList.toggle('hidden', mode !== 'code');
  $('row-direct').classList.toggle('hidden', mode !== 'direct');
  // No server in a direct connection.
  invoke('built_in_server').then((fixed) => { if (!fixed) $('f-server').closest('label').classList.toggle('hidden', mode === 'direct'); });
  state.directRole = state.directRole || 'invite';
  seg($('direct-role'), [['invite', t('direct.role_invite')], ['answer', t('direct.role_answer')]], state.directRole, (v) => { state.directRole = v; state.directStep = {}; renderJoinForm(); });
  $('join-submit').classList.toggle('hidden', mode === 'direct');
  $('join-submit').textContent = mode === 'create' ? t('join.create') : t('join.enter');
  renderDirectSteps();
}
// The two texts of a direct connection, in the join form. With "short code" on, each text
// also goes to the short-code service for 5 minutes; the full text stays as the fallback.
const shortCodesOn = () => settings().shortCode !== false;
const looksLikeCode = (v) => /^[0-9A-Za-z]{8}$/.test(v.replace(/[\s-]/g, ''));
const showCode = (c) => `${c.slice(0, 4)}-${c.slice(4)}`;
const codeClock = (ms) => { const s = Math.max(0, Math.ceil(ms / 1000)); return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`; };
// A short code for a text, or nothing when the service is off, slow or down.
async function shortCodeFor(text) {
  if (!shortCodesOn()) return null;
  try {
    const r = await invoke('shortcode_put', { text });
    return { code: r.code, until: Date.now() + r.expires_in * 1000 };
  } catch { return { failed: true }; }
}
// What the person pasted: a full text as is, or the text behind a short code.
async function pastedText(value) {
  const v = value.trim();
  if (!looksLikeCode(v)) return v;
  if (!shortCodesOn()) throw new Error(t('direct.codes_off'));
  try { return await invoke('shortcode_get', { code: v }); }
  catch (e) { throw new Error(String(e) === 'not_found' ? t('direct.code_not_found') : t('direct.code_failed', { e: String(e) })); }
}
setInterval(() => {
  for (const e of document.querySelectorAll('#direct-steps [data-until]')) {
    const left = Number(e.dataset.until) - Date.now();
    if (left > 0) { e.textContent = t('direct.code_left', { time: codeClock(left) }); continue; }
    const d = state.directStep || {};
    for (const k of ['inviteCode', 'answerCode']) if (d[k] && d[k].until <= Date.now()) d[k] = { expired: true };
    renderDirectSteps();
    break;
  }
}, 1000);
function renderDirectSteps() {
  const box = $('direct-steps');
  if (state.joinMode !== 'direct') { box.replaceChildren(); return; }
  const d = state.directStep || (state.directStep = {});
  const name = () => $('f-name').value.trim();
  const copy = (text, label = 'direct.copy') => el('button', { class: 'btn', type: 'button', text: t(label), onclick: async (e) => { try { await navigator.clipboard.writeText(text); e.currentTarget.textContent = t('direct.copied'); } catch {} } });
  // A textarea shows its .value, not a value attribute.
  const area = (value, readOnly) => Object.assign(el('textarea', { spellcheck: 'false', readonly: !!readOnly }), { value: value || '' });
  const run = async (btn, f) => {
    if (!name()) { joinError(t('direct.need_name')); return; }
    $('join-error').classList.add('hidden');
    btn.disabled = true; btn.textContent = t('direct.making');
    try { await f(); state.joinOpen = true; } catch (e) { joinError(e instanceof Error ? e.message : String(e)); }
    renderDirectSteps(); tick();
  };
  // The text to send: the short code first when there is one, the full text below it.
  const outgoing = (title, text, code) => {
    const out = [el('b', { text: title })];
    if (code && code.code) {
      out.push(el('div', { class: 'row' }, el('span', { class: 'direct-code', text: showCode(code.code) }), copy(showCode(code.code), 'direct.copy_code'), el('span', { class: 'small', 'data-until': String(code.until), text: t('direct.code_left', { time: codeClock(code.until - Date.now()) }) })),
        el('span', { class: 'small', text: t('direct.or_text') }));
    } else if (code) {
      out.push(el('span', { class: 'small', text: t(code.expired ? 'direct.code_expired' : 'direct.no_code') }));
    }
    out.push(area(text, true), el('div', { class: 'row' }, copy(text)));
    return out;
  };
  const short = el('label', { class: 'opt' }, el('input', { type: 'checkbox', id: 'direct-short', checked: shortCodesOn(), onchange: (e) => saveSettings({ ...settings(), shortCode: e.currentTarget.checked }) }), el('span', { text: ' ' + t('direct.short') }));
  const parts = [];
  if (state.directRole === 'invite') {
    if (!d.invite) {
      parts.push(short, el('button', { class: 'btn primary', type: 'button', text: t('direct.make_invite'), onclick: (e) => run(e.currentTarget, async () => { const text = await invoke('direct_invite', { name: name() }); d.inviteCode = await shortCodeFor(text); d.invite = text; }) }));
    } else {
      const ans = area(d.answerIn || '');
      ans.oninput = () => { d.answerIn = ans.value; };
      parts.push(...outgoing(t('direct.send_invite'), d.invite, d.inviteCode),
        el('b', { text: t('direct.paste_answer') }), ans,
        el('button', { class: 'btn primary', type: 'button', text: d.accepted ? t('direct.connecting') : t('direct.connect'), disabled: !!d.accepted, onclick: (e) => run(e.currentTarget, async () => { await invoke('direct_accept', { text: await pastedText(ans.value) }); d.accepted = true; state.directOpen = true; }) }));
    }
  } else if (!d.answer) {
    const inv = area(d.inviteIn || '');
    inv.oninput = () => { d.inviteIn = inv.value; };
    parts.push(short, el('b', { text: t('direct.paste_invite') }), inv,
      el('button', { class: 'btn primary', type: 'button', text: t('direct.make_answer'), onclick: (e) => run(e.currentTarget, async () => { const text = await invoke('direct_answer', { text: await pastedText(inv.value), name: name() }); state.directOpen = true; d.answerCode = await shortCodeFor(text); d.answer = text; }) }));
  } else {
    parts.push(...outgoing(t('direct.send_answer'), d.answer, d.answerCode), el('span', { class: 'small', text: t('direct.wait') }));
  }
  box.replaceChildren(...parts);
}
function joinError(text) {
  $('join-error').textContent = text;
  $('join-error').classList.remove('hidden');
}
$('join-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('join-error').classList.add('hidden');
  const base = { server: $('f-server').value.trim(), name: $('f-name').value.trim() };
  if (state.joinMode === 'direct') return;
  let r;
  if (state.joinMode === 'create') {
    const title = $('f-title').value.trim();
    if (!title) return joinError(t('join.need_title'));
    r = { ...base, room: newCode(), title };
  } else {
    const code = parseCode($('f-code').value);
    if (!code) return joinError(t('join.bad_code'));
    r = { ...base, room: code };
  }
  try { await joinRoom(r); $('f-title').value = ''; $('f-code').value = ''; } catch {}
});
$('invite').addEventListener('click', () => { if (state.current) copyInvite(state.current); });
$('join-back').addEventListener('click', () => { state.joinOpen = false; tick(); });
$('leave').addEventListener('click', async () => { await invoke('leave'); state.current = null; $('rail').dataset.key = ''; tick(); });

// ---------- render ----------
function render(s) {
  // Out of every room but with saved ones: the window stays, with the rooms on the left.
  // The join form alone only when there is nothing to go back to.
  const hasRooms = savedRooms().length > 0;
  // Boolean on purpose: toggle(name, undefined) flips the class instead of clearing it.
  const joinShown = !!state.joinOpen || (!s.connected && !hasRooms);
  $('join').classList.toggle('hidden', !joinShown);
  $('join-back').classList.toggle('hidden', !(s.connected || hasRooms));
  $('shell').classList.toggle('hidden', joinShown);
  $('invite').classList.toggle('hidden', !s.connected);
  $('leave').classList.toggle('hidden', !s.connected);
  if (!s.connected) { state.direct = null; renderLobby(s); syncVideos(); return; }
  const me = s.peers.find((p) => p.me) || { name: '?' };
  // A direct connection (beta): a room of two with no server; nothing to save or invite to.
  if (s.direct) {
    if (s.direct === 'connected' && state.directOpen) { state.directOpen = false; state.joinOpen = false; }
    state.current = null;
    $('room-name').replaceChildren(document.createTextNode(t('direct.title')), el('span', { class: 'beta', text: 'BETA' }));
    $('room-name').title = '';
    $('invite').classList.add('hidden');
    $('me-avatar').replaceWith(Object.assign(avatar(me.name), { id: 'me-avatar' }));
    $('me-state').textContent = s.live ? t('room.live_for', { t: clock(s.live.since_secs) }) : t('room.not_live');
    $('me-state').style.color = s.live ? '#ff9aa0' : '';
    $('go-live').classList.toggle('hidden', !!s.live);
    renderRail(s); renderPeople(s); renderHeader(s); renderContent(s);
    return;
  }
  // Joined some other way (tests): remember what we are in.
  if (!state.current || state.current.room !== s.room || (s.title && state.current.title !== s.title)) {
    // The server's title wins: a room joined by code learns its name here.
    state.current = { ...(state.current && state.current.room === s.room ? state.current : { server: $('f-server').value.trim(), name: me.name }), room: s.room, title: s.title || (state.current && state.current.title) || null };
    rememberRoom(state.current);
    try { localStorage.setItem('lastRoom', JSON.stringify(state.current)); } catch {}
  }
  $('room-name').textContent = s.title || s.room;
  $('room-name').title = isCode(s.room) ? fmtCode(s.room) : s.room;
  $('me-avatar').replaceWith(Object.assign(avatar(me.name), { id: 'me-avatar' }));
  $('me-state').textContent = s.live ? t('room.live_for', { t: clock(s.live.since_secs) }) : t('room.not_live');
  $('me-state').style.color = s.live ? '#ff9aa0' : '';
  $('go-live').classList.toggle('hidden', !!s.live);

  renderRail(s);
  renderPeople(s);
  renderHeader(s);
  renderContent(s);
}

// Not in a room: the rail to pick one, and a hint where the room would be.
function renderLobby(s) {
  state.current = null;
  if (state.cardsKey === 'lobby') { renderRail(s); return; }
  state.cardsKey = 'lobby';
  $('room-name').textContent = t('lobby.title');
  $('room-name').title = '';
  $('people').replaceChildren();
  let name = '';
  try { name = JSON.parse(localStorage.getItem('join') || '{}').name || ''; } catch {}
  $('me-avatar').replaceWith(Object.assign(avatar(name || '?'), { id: 'me-avatar' }));
  $('me-state').textContent = '';
  $('go-live').classList.add('hidden');
  $('main-header').dataset.key = 'lobby';
  $('main-header').replaceChildren(el('span', { text: t('room.streams') }), langPicker());
  $('stage').className = 'stage hidden';
  $('stage').replaceChildren();
  $('content').replaceChildren(el('div', { class: 'empty', text: t('lobby.pick') }));
  renderRail(s);
}

function renderPeople(s) {
  const live = s.peers.filter((p) => p.streaming);
  const rest = s.peers.filter((p) => !p.streaming);
  const row = (p) => el('div', { class: 'person' + (p.streaming ? ' live' : '') },
    avatar(p.name),
    el('span', { class: 'who' }, el('span', { text: p.me ? `${p.name} (${t('room.you')})` : p.name })),
    p.streaming ? el('span', { class: 'pill-live', text: 'LIVE' }) : null);
  $('people').replaceChildren(
    el('div', { class: 'section', text: t('room.live', { n: live.length }) }), ...live.map(row),
    el('div', { class: 'section', text: t('room.here', { n: rest.length }) }), ...rest.map(row));
}

// Language, also from inside a room.
function langPicker() {
  const sel = el('select', { class: 'lang', 'aria-label': t('connect.language'), onchange: (e) => { setLanguage(e.target.value); $('main-header').dataset.key = ''; applyStrings(); } },
    ...LANGS.map((l) => el('option', { value: l.id, text: l.name, selected: l.id === lang })));
  return sel;
}

function renderHeader(s) {
  const h = $('main-header');
  if (s.live) {
    const key = 'live';
    if (h.dataset.key !== key) {
      h.dataset.key = key;
      h.replaceChildren(
        el('span', { class: 'pill-live', text: 'LIVE' }),
        el('span', { text: t('live.title') }),
        el('span', { class: 'small', id: 'live-sub' }),
        langPicker(),
        el('button', { class: 'btn', type: 'button', text: t('live.change'), onclick: () => openGoLive(true) }),
        el('button', { class: 'btn danger', type: 'button', text: t('live.end'), onclick: async () => { await invoke('end_live'); tick(); } }));
    }
    $('live-sub').textContent = `${shortQuality(s.live.ceiling)} ${t('unit.mbit')} · ${s.live.encoder}`;
  } else if (h.dataset.key !== 'streams') {
    h.dataset.key = 'streams';
    h.replaceChildren(el('span', { text: t('room.streams') }), langPicker());
  }
}

function renderContent(s) {
  const watched = s.peers.filter((p) => p.streaming && !p.me && p.watching);
  const others = s.peers.filter((p) => p.streaming && !p.me && !p.watching);
  const key = watched.map((p) => p.id).join(',') + '|' + others.map((p) => p.id).join(',') + '|' + (s.live ? 'L' : '');
  const stage = $('stage');
  const box = $('content');
  if (key !== state.cardsKey) {
    state.cardsKey = key;
    state.liveKey = '';
    stage.className = 'stage ' + (watched.length === 0 ? 'hidden' : watched.length === 1 ? 'n1' : watched.length === 2 ? 'n2' : 'n4');
    stage.replaceChildren(...watched.map(tile));
    const parts = [el('div', { id: 'hint' })];
    if (watched.length) parts.push(el('div', { class: 'small', text: t('tile.keys') }));
    if (others.length) parts.push(el('section', {},
      el('div', { class: 'section', style: 'padding-left:0', text: t('room.also_live') }),
      el('div', { class: 'chips' }, ...others.map(chip))));
    if (s.live) parts.push(el('div', { id: 'live-panel', style: 'display:flex;flex-direction:column;gap:16px' }));
    if (!watched.length && !others.length && !s.live) parts.push(el('div', { class: 'empty', text: t('room.none') }));
    box.replaceChildren(...parts);
  }
  // Numbers, in place.
  let hint = null;
  for (const w of s.watches) {
    const c = document.querySelector(`[data-tile="${w.streamer}"]`);
    if (!c) continue;
    const throttled = w.received_fps > 20 && w.shown_fps < w.received_fps * 0.8;
    const st = c.querySelector('[data-k=stat]');
    st.textContent = `${Math.round(w.shown_fps)} fps · ${ms(w.latency_ms)}${w.quality ? ' · ' + requestLabel(w.quality) : ''}`;
    st.classList.toggle('warn', throttled);
    c.querySelector('[data-k=ov]').setAttribute('aria-pressed', String(w.overlay));
    setMuteButton(c.querySelector('[data-k=mute]'), w.muted);
    const pop = c.querySelector('[data-k=pop]');
    pop.title = w.embedded ? t('tile.popout') : t('tile.popin');
    pop.setAttribute('aria-label', pop.title);
    pop.setAttribute('aria-pressed', String(!w.embedded));
    pop.dataset.action = w.embedded ? 'pop_out' : 'pop_in';
    const p = s.peers.find((x) => x.id === w.streamer);
    c.querySelector('.video').textContent = w.embedded ? '' : t('tile.popped', { name: p ? p.name : '?' });
    // Show what is being watched in the pickers until the viewer picks something else.
    const ch = state.choice[w.streamer];
    if (ch && !ch.touched && w.quality) {
      const [m, k] = w.quality.split(' ');
      if (ch.mode !== m || ch.kbit !== (k || '')) {
        ch.mode = m; ch.kbit = k || '';
        const sels = c.querySelectorAll('select');
        if (sels[0]) sels[0].value = ch.mode;
        if (sels[1]) sels[1].value = ch.kbit;
      }
    }
    if (throttled && !hint) hint = t('hint.background', { name: p ? p.name : '?', shown: Math.round(w.shown_fps), rx: Math.round(w.received_fps) });
  }
  const hb = $('hint');
  if (hb) {
    if (hint) hb.replaceChildren(el('div', { class: 'banner' }, el('span', { style: 'flex:1 1 320px', text: hint })));
    else hb.replaceChildren();
  }
  if (s.live) renderLive(s.live);
  syncVideos();
}

function qualitySelects(id) {
  const ch = state.choice[id] || (state.choice[id] = { mode: '', kbit: '' });
  const mode = el('select', { 'aria-label': t('card.quality'), onchange: (e) => { ch.mode = e.target.value; ch.touched = true; } },
    el('option', { value: '', text: t('card.source_quality'), selected: ch.mode === '' }),
    ...VIEW_MODES.map((m) => el('option', { value: m, text: m, selected: ch.mode === m })));
  const kbit = el('select', { 'aria-label': t('card.bitrate'), onchange: (e) => { ch.kbit = e.target.value; ch.touched = true; } },
    el('option', { value: '', text: '—', selected: ch.kbit === '' }),
    ...PRESETS.map((k) => el('option', { value: String(k), text: `${mbit(k)} ${t('unit.mbit')}`, selected: ch.kbit === String(k) })));
  return [mode, kbit];
}
function requestOf(id) {
  const ch = state.choice[id] || {};
  if (!ch.mode && !ch.kbit) return null;
  return `${ch.mode || '1440p60'}${ch.kbit ? ' ' + ch.kbit : ''}`;
}

// A watched stream: the native video window is placed over .video by syncVideos.
function tile(p) {
  const act = (action) => invoke('video_window_do', { streamer: p.id, action });
  return el('div', { class: 'vtile', 'data-tile': p.id },
    el('div', { class: 'video', 'data-video': p.id }),
    el('div', { class: 'vbar' },
      el('span', { class: 'name', text: p.name }),
      el('span', { class: 'vstat', 'data-k': 'stat', text: '—' }),
      ...qualitySelects(p.id),
      el('button', { class: 'btn', type: 'button', text: t('card.apply'), onclick: async () => { const q = requestOf(p.id); if (q) await invoke('set_quality', { streamer: p.id, quality: q }); } }),
      el('button', { class: 'btn icon', type: 'button', 'data-k': 'mute', 'aria-pressed': 'false', title: t('tile.mute'), 'aria-label': t('tile.mute'), text: '🔊', onclick: async (e) => { const on = e.currentTarget.getAttribute('aria-pressed') !== 'true'; await invoke('set_muted', { streamer: p.id, muted: on }); setMuteButton(e.currentTarget, on); } }),
      el('button', { class: 'btn', type: 'button', 'data-k': 'ov', 'aria-pressed': 'false', title: 'F3', text: t('tile.stats'), onclick: (e) => act(e.currentTarget.getAttribute('aria-pressed') === 'true' ? 'no_overlay' : 'overlay') }),
      el('button', { class: 'btn icon', type: 'button', 'data-k': 'pop', 'data-action': 'pop_out', text: '⧉', onclick: (e) => { act(e.currentTarget.dataset.action); setTimeout(tick, 150); } }),
      el('button', { class: 'btn icon', type: 'button', title: t('tile.fullscreen'), 'aria-label': t('tile.fullscreen'), text: '⛶', onclick: () => act('toggle_fullscreen') }),
      el('button', { class: 'btn danger', type: 'button', text: t('card.stop'), onclick: async () => { await invoke('unwatch', { streamer: p.id }); tick(); } })));
}
function setMuteButton(b, muted) {
  if (!b) return;
  b.setAttribute('aria-pressed', String(muted));
  b.textContent = muted ? '🔇' : '🔊';
  b.title = muted ? t('tile.unmute') : t('tile.mute');
  b.setAttribute('aria-label', b.title);
}
// Someone live we are not watching.
function chip(p) {
  return el('div', { class: 'chip' }, avatar(p.name), el('b', { text: p.name }),
    el('button', { class: 'btn primary', type: 'button', text: t('card.watch'), onclick: async () => { await invoke('watch', { streamer: p.id, quality: watchRequest() }); tick(); } }));
}

// Keeps each native video window over its .video box, and hides them all while
// something from the page must be on top (a dialog, the join screen).
const placed = {};
function syncVideos() {
  const s = state.snap;
  const covered = [...document.querySelectorAll('dialog')].some((d) => d.open) || !$('join').classList.contains('hidden');
  if (covered !== state.videosHidden) {
    state.videosHidden = covered;
    invoke('videos_hidden', { hidden: covered });
  }
  for (const w of (s && s.watches) || []) {
    const box = document.querySelector(`[data-video="${w.streamer}"]`);
    let rect = null;
    if (box && w.embedded) {
      const r = box.getBoundingClientRect();
      if (r.width > 4 && r.height > 4) rect = [r.left, r.top, r.width, r.height];
    }
    const k = JSON.stringify(rect);
    if (placed[w.streamer] !== k) {
      placed[w.streamer] = k;
      invoke('video_place', { streamer: w.streamer, rect });
    }
  }
}
new ResizeObserver(() => syncVideos()).observe($('stage'));
window.addEventListener('resize', () => syncVideos());
new MutationObserver(() => syncVideos()).observe(document.body, { subtree: true, attributes: true, attributeFilter: ['open', 'class'] });

function renderLive(l) {
  const panel = $('live-panel');
  if (!panel) return;
  const active = l.layers.filter((x) => !x.parked);
  const fps = Math.round(active.reduce((a, x) => Math.max(a, x.fps), 0));
  const limit = uploadLimit();
  const cap = limit || Math.max(l.ceiling_mbit * Math.max(l.viewers.length, 1), 1);
  const wontFit = limit && l.upload_mbit + l.ceiling_mbit > limit;
  const rows = l.viewers.map((v) => {
    const q = v.problems.find((p) => p.startsWith('queue:'));
    const loss = v.problems.find((p) => p.startsWith('loss:'));
    let stateCell;
    if (q) stateCell = el('span', { class: 'state-warn' }, el('span', { class: 'dot', style: 'background:var(--warn)' }), t('live.queue', { s: (q.split(':')[1] / 1000).toFixed(1) }), el('div', { class: 'small', text: t('live.queue_note') }));
    else if (loss) stateCell = el('span', { class: 'state-warn' }, el('span', { class: 'dot', style: 'background:var(--warn)' }), t('live.loss', { pct: loss.split(':')[1] }));
    else stateCell = el('span', { class: 'state-ok' }, el('span', { class: 'dot', style: 'background:var(--ok)' }), t('live.ok'));
    return el('div', { class: 'tr' },
      el('span', { style: 'display:flex;align-items:center;gap:10px' }, avatar(v.name), v.name),
      el('span', { text: shortQuality(v.quality) }),
      el('span', { text: ms(v.rtt_ms) }),
      stateCell);
  });
  panel.replaceChildren(...[
    el('div', { class: 'tiles' },
      el('div', { class: 'tile' }, el('span', { class: 'small', text: t('live.viewers') }), el('span', { class: 'num', text: String(l.viewers.length) })),
      el('div', { class: 'tile' }, el('span', { class: 'small', text: t('live.encoding') }), el('span', { class: 'num', text: `${fps} fps` }),
        el('span', { class: 'small', text: t('live.layers', { n: active.length, list: active.map((x) => shortQuality(x.quality)).join(', ') || '—' }) })),
      el('div', { class: 'tile' }, el('span', { class: 'small', text: t('live.upload') }),
        el('span', { class: 'num', text: limit ? t('up.of', { used: fmt(l.upload_mbit), limit: fmt(limit) }) : `${fmt(l.upload_mbit)} ${t('unit.mbit')}` }),
        el('div', { class: 'meter' }, el('div', { style: `width:${Math.min(100, (l.upload_mbit / cap) * 100)}%;${wontFit ? 'background:var(--warn)' : ''}` })))),
    wontFit ? el('div', { class: 'banner', text: t('up.next_wont_fit', { used: fmt(l.upload_mbit), add: fmt(l.ceiling_mbit), limit: fmt(limit) }) }) : null,
    l.viewers.length
      ? el('section', { class: 'table', 'aria-label': t('live.viewers') },
          el('div', { class: 'tr head' }, el('span', { text: t('live.th_viewer') }), el('span', { text: t('live.th_quality') }), el('span', { text: t('live.th_rtt') }), el('span', { text: t('live.th_state') })),
          ...rows)
      : el('div', { class: 'empty', text: t('live.no_viewers') }),
    el('p', { class: 'small', style: 'margin:0', text: t('live.isolation') })].filter(Boolean));
}

// ---------- go live ----------
function seg(box, items, current, onPick) {
  box.replaceChildren(...items.map(([v, label]) => el('button', { type: 'button', 'aria-pressed': String(v === current), text: label, onclick: () => onPick(v) })));
}
function renderGoLive() {
  const g = state.gl, s = state.snap;
  $('gl-sub').textContent = t('golive.sub', { room: s.room, n: Math.max(s.peers.length - 1, 0) });
  $('gl-tab-screen').setAttribute('aria-pressed', String(g.tab === 'screen'));
  $('gl-tab-window').setAttribute('aria-pressed', String(g.tab === 'window'));
  const list = g.tab === 'screen'
    ? (g.sources?.monitors || []).map((m) => ({ id: m.handle, title: `${m.name.replace('\\\\.\\', '')}${m.primary ? ' · ' + t('golive.primary') : ''}`, sub: `${m.width}×${m.height}`, h: m.height }))
    : (g.sources?.windows || []).map((w) => ({ id: w.hwnd, title: w.title, sub: w.exe, minimized: w.minimized, pid: w.pid, h: w.height }));
  if (!list.some((x) => g.pick && x.id === g.pick.id)) g.pick = g.tab === 'screen' ? list[0] || null : null;
  // The resolution follows the source until the user picks one: 1440p for a source 1440
  // tall or more, 1080p from 1080, else 720p.
  if (!g.resPicked && g.pick && g.pick.h > 0) g.res = g.pick.h >= 1440 ? '1440' : g.pick.h >= 1080 ? '1080' : '720';
  const shot = (id) => g.thumbs[`${g.tab}:${id}`] || {};
  $('gl-sources').replaceChildren(...list.map((x) => el('button', { type: 'button', class: 'source', 'aria-pressed': String(g.pick?.id === x.id), 'data-src': x.id, onclick: () => { g.pick = x; renderGoLive(); } },
    el('span', { class: 'shot' }, shot(x.id).image ? el('img', { src: shot(x.id).image, alt: '' }) : el('span', { class: 'small', text: x.minimized ? t('golive.minimized') : '' })),
    el('span', { class: 'src-title' }, shot(x.id).icon ? el('img', { class: 'src-icon', src: shot(x.id).icon, alt: '' }) : null, el('span', { text: x.title })),
    el('span', { class: 'small', text: x.sub }))));
  $('gl-audio-app').classList.toggle('hidden', g.tab !== 'window');
  // Sound follows the source until picked by hand: one program's sound for a window, the
  // system's for a screen ("auto"), or what the settings say.
  if (!g.audioPicked && !g.editing) {
    const p = streamPrefs().audio || '';
    const want = p === 'none' ? 'none' : p === 'system' ? 'system' : g.tab === 'window' ? 'app' : 'system';
    const r = document.querySelector(`input[name=audio][value=${want}]`);
    if (r) r.checked = true;
  }
  if (g.tab !== 'window' && document.querySelector('input[name=audio]:checked')?.value === 'app') document.querySelector('input[name=audio][value=system]').checked = true;
  seg($('gl-res'), [['720', '720p'], ['1080', '1080p'], ['1440', '1440p']], g.res, (v) => { g.res = v; g.resPicked = true; renderGoLive(); });
  seg($('gl-fps'), [['30', '30'], ['60', '60']], g.fps, (v) => { g.fps = v; renderGoLive(); });
  seg($('gl-kbit'), PRESETS.map((k) => [k, mbit(k)]), g.kbit, (v) => { g.kbit = v; renderGoLive(); });
  // Only codecs this machine encodes in hardware; AV1 needs a recent GPU.
  // AV1 first and chosen when the GPU encodes it, with its own default bitrate.
  // While live the codec stays (changing it would need every viewer to reconnect).
  const codecs = [['av1', 'AV1'], ['h265', 'H.265'], ['h264', 'H.264']].filter(([c]) => (g.codecs || ['h265', 'h264']).includes(c) && (!g.editing || c === g.codec));
  if (!g.codecPicked && codecs.length && g.codec !== codecs[0][0]) {
    if (codecs[0][0] === 'av1' && g.kbit === 20000) g.kbit = 15000;
    g.codec = codecs[0][0];
  }
  if (!codecs.some(([c]) => c === g.codec) && codecs.length) g.codec = codecs[0][0];
  seg($('gl-codec'), codecs, g.codec, (v) => {
    // AV1 gives about the same picture with fewer bits: 15 where the others use 20.
    if (v === 'av1' && g.codec !== 'av1' && g.kbit === 20000) g.kbit = 15000;
    if (v !== 'av1' && g.codec === 'av1' && g.kbit === 15000) g.kbit = 20000;
    g.codec = v;
    g.codecPicked = true;
    renderGoLive();
  });
  $('gl-codec-note').textContent = t(g.codec === 'h264' ? 'golive.codec_h264' : g.codec === 'av1' ? 'golive.codec_av1' : 'golive.codec_h265');
  // While live, sound stays on or off as it started; which sound may change.
  for (const r of document.querySelectorAll('input[name=audio]')) r.disabled = !!g.editing && (g.liveAudio === 'none') !== (r.value === 'none');
  $('gl-start').textContent = g.editing ? t('live.apply') : t('golive.start');
  const n = Math.max(s.peers.length - 1, 1);
  const need = (g.kbit * n) / 1000;
  const limit = uploadLimit();
  $('gl-upload-label').textContent = t('golive.upload', { n });
  $('gl-upload').textContent = limit ? t('up.of', { used: fmt(need), limit: fmt(limit) }) : t('golive.upload_value', { mbit: mbit(g.kbit * n) });
  const bar = $('gl-upload-bar');
  bar.style.width = Math.min(100, (need / (limit || Math.max(need, 1))) * 100) + '%';
  bar.style.background = limit && need > limit ? 'var(--warn)' : '';
  $('gl-upload-note').textContent = t('golive.upload_note', { kbit: mbit(g.kbit) });
  if (document.activeElement !== $('gl-limit')) $('gl-limit').value = limit || '';
  const over = $('gl-over');
  over.classList.toggle('hidden', !(limit && need > limit));
  over.textContent = limit && need > limit ? t('up.over', { need: fmt(need), limit: fmt(limit) }) : '';
}
// The go-live dialog; `editing`: change the running stream instead of starting one.
async function openGoLive(editing) {
  state.gl.editing = !!editing;
  if (!editing) {
    // The settings' defaults, each time; what this computer cannot do falls back in renderGoLive.
    const d = streamPrefs(), g = state.gl;
    g.resPicked = !!d.res;
    if (d.res) g.res = d.res;
    if (d.fps) g.fps = d.fps;
    if (d.kbit) g.kbit = +d.kbit;
    g.codecPicked = !!d.codec;
    if (d.codec) g.codec = d.codec;
    g.audioPicked = false;
  }
  $('gl-error').classList.add('hidden');
  state.gl.sources = await invoke('sources');
  if (!state.gl.codecs) state.gl.codecs = await invoke('codecs');
  state.gl.thumbs = {};
  renderGoLive();
  $('golive').showModal();
  refreshThumbs().then(renderGoLive);
}
$('go-live').addEventListener('click', () => openGoLive(false));
for (const r of document.querySelectorAll('input[name=audio]')) r.addEventListener('change', () => { state.gl.audioPicked = true; });
$('gl-tab-screen').addEventListener('click', () => { state.gl.tab = 'screen'; renderGoLive(); refreshThumbs(); });
$('gl-tab-window').addEventListener('click', () => { state.gl.tab = 'window'; renderGoLive(); refreshThumbs(); });
$('gl-cancel').addEventListener('click', () => $('golive').close());
$('gl-limit').addEventListener('input', (e) => { setUploadLimit(+e.target.value); renderGoLive(); });
$('gl-start').addEventListener('click', async () => {
  const g = state.gl;
  if (!g.pick) { $('gl-error').textContent = t('golive.pick_window'); $('gl-error').classList.remove('hidden'); return; }
  const audio = document.querySelector('input[name=audio]:checked').value;
  const req = { kind: g.tab === 'screen' ? 'monitor' : 'window', id: g.pick.id, audio, audio_pid: g.pick.pid ?? null, mode: `${g.res}p${g.fps}`, kbit: g.kbit, codec: g.codec };
  if (g.editing) {
    try { await call('update_live', { req }, $('gl-error')); } catch { return; }
  } else {
    await call('go_live', { req }, $('gl-error'));
    g.liveAudio = audio;
  }
  $('golive').close();
  tick();
});

// ---------- loop ----------
function showNotice(n) {
  if (!n) return;
  const [kind, rest] = [n.split(':')[0], n.slice(n.indexOf(':') + 1)];
  const text = kind === 'direct_ended' ? t('notice.direct_ended') : kind === 'lost' ? t('notice.lost') : kind === 'watch_ended' ? t('notice.watch_ended', { reason: rest }) : t('notice.error', { msg: rest });
  const box = $('join-error');
  box.textContent = text;
  box.classList.remove('hidden');
}
async function tick() {
  try {
    const s = await invoke('snapshot');
    // A direct connection that ended leaves nothing to reuse: the form starts over.
    if (state.snap && state.snap.direct === 'connected' && s.direct !== 'connected') { state.directStep = {}; renderDirectSteps(); }
    state.snap = s;
    showNotice(s.notice);
    render(s);
    if (s.link) handleLink(s.link);
  } catch (e) {
    console.error(e);
  }
}

setLanguage(lang);
// Tests only: STREAMER_APP_LANG forces the language for screenshots.
invoke('forced_language').then((l) => { if (l) { setLanguage(l); fillLanguages(); applyStrings(); } });
// Tests only: keep the UI busy: open and close the go-live dialog (which takes pictures
// of every source), switch the language, every 1.5 s.
invoke('test_ui_stress').then((tab) => {
  if (!tab) return;
  state.gl.tab = tab;
  let n = 0;
  setInterval(() => {
    n++;
    if (!state.snap || !state.snap.connected) return;
    if ($('golive').open) $('golive').close(); else $('go-live').click();
    if (n % 2 === 0) { setLanguage(LANGS[(n / 2) % LANGS.length].id); $('main-header').dataset.key = ''; applyStrings(); }
  }, 1500);
});
invoke('test_form').then((spec) => {
  if (!spec) return;
  const [mode, value, name, server] = spec.split('|');
  setTimeout(() => {
    if (mode === 'show') { state.joinOpen = true; tick(); return; }
    // "direct|<pasted>|<name>": the direct form, invited side, with that text pasted.
    if (mode === 'direct') { state.joinOpen = true; state.joinMode = 'direct'; state.directRole = 'answer'; state.directStep = { inviteIn: value }; $('f-name').value = name; renderJoinForm(); tick(); return; }
    // "leave|<other room code>|": leave the room we are in, then go to the other one
    // through the rail.
    if (mode === 'leave') {
      const report = (what) => invoke('test_report', { msg: `${what}: shell ${!$('shell').classList.contains('hidden')} join ${!$('join').classList.contains('hidden')} rail ${document.querySelectorAll('#rail .r:not(.add)').length} header ${$('room-name').textContent} connected ${state.snap && state.snap.connected} room ${state.snap && state.snap.room}` });
      setTimeout(() => {
        report('before');
        $('leave').click();
        setTimeout(() => {
          report('after leave');
          const btn = document.querySelector(`#rail .r[data-room="${value}"]`);
          if (btn) btn.click();
          setTimeout(() => report('after rail click'), 3000);
        }, 2500);
      }, 2000);
      return;
    }
    state.joinMode = mode;
    renderJoinForm();
    $('f-name').value = name;
    if (server) $('f-server').value = server;
    if (mode === 'create') $('f-title').value = value; else $('f-code').value = value;
    $('join-form').requestSubmit();
    const timer = setInterval(() => {
      const s = state.snap;
      if (s && s.connected) {
        clearInterval(timer);
        setTimeout(() => invoke('test_report', { msg: `room ${s.room} title ${state.snap.title} header ${$('room-name').textContent} peers ${state.snap.peers.map((p) => p.name).join(',')}` }), 1500);
      }
    }, 300);
  }, 1500);
});
invoke('test_click_watch').then((name) => {
  if (!name) return;
  const timer = setInterval(() => {
    const chipEl = [...document.querySelectorAll('.chip')].find((c) => c.querySelector('b')?.textContent === name);
    if (chipEl) { clearInterval(timer); chipEl.querySelector('button').click(); }
  }, 300);
});
invoke('test_rooms').then((other) => {
  if (!other) return;
  setTimeout(async () => {
    const first = state.current;
    await switchRoom({ ...first, room: other });
    setTimeout(() => {
      const btn = [...document.querySelectorAll('#rail .r')].find((b) => b.dataset.room === first.room);
      if (btn) btn.click();
      setTimeout(() => {
        const b2 = [...document.querySelectorAll('#rail .r')].find((b) => b.dataset.room === other);
        if (b2) { const r = b2.getBoundingClientRect(); b2.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: r.right - 6, clientY: r.top + 10 })); }
      }, 4000);
    }, 5000);
  }, 5000);
});
invoke('test_open_settings').then((tab) => { if (tab) setTimeout(async () => { await openSettings(tab); if (tab === 'general' && document.body.dataset.clickLogs !== 'done') { document.body.dataset.clickLogs = 'done'; setTimeout(() => $('set-logs').click(), 800); } }, 2500); });
// Tests only: drive the direct connection like a person, texts through files.
// STREAMER_APP_TEST_DIRECT="invite|answer NAME DIR [code|nocode] [live] [watch:NAME] [leave]".
invoke('test_direct').then(async (spec) => {
  if (!spec) return;
  const [role, name, dir, ...rest] = spec.split(' ');
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (f, ms = 30000) => { const end = Date.now() + ms; while (Date.now() < end) { const v = await f(); if (v) return v; await wait(250); } return null; };
  const button = (label) => [...document.querySelectorAll('#direct-steps button')].find((b) => b.textContent === t(label));
  // [code]: swap the short codes when the app shows them, else the full texts.
  // [nocode]: short codes switched off.
  const useCode = rest.includes('code');
  saveSettings({ ...settings(), shortCode: !rest.includes('nocode') });
  const outgoing = async () => {
    const text = await until(() => { const a = document.querySelector('#direct-steps textarea[readonly]'); return a && a.value; }, 20000);
    if (!text || !useCode) return text;
    const code = document.querySelector('#direct-steps .direct-code');
    invoke('test_report', { msg: `direct code ${code ? code.textContent + ' ' + document.querySelector('#direct-steps [data-until]').textContent : 'none: ' + $('direct-steps').textContent.slice(0, 120)}` });
    return code ? code.textContent : text;
  };
  await wait(1500);
  state.joinMode = 'direct'; state.directRole = role; state.directStep = {}; state.joinOpen = true;
  $('f-name').value = name; renderJoinForm(); tick();
  if (role === 'invite') {
    button('direct.make_invite').click();
    const invite = await outgoing();
    if (!invite) { invoke('test_report', { msg: `direct invite failed: ${$('join-error').textContent} | mode ${state.joinMode} role ${state.directRole} step ${JSON.stringify(Object.keys(state.directStep || {}))} | steps: ${$('direct-steps').innerHTML.slice(0, 300)}` }); return; }
    await invoke('test_put', { dir, name: 'invite.txt', text: invite });
    invoke('test_report', { msg: `direct invite ${invite.length} chars` });
    const answer = await until(() => invoke('test_get', { dir, name: 'answer.txt' }), 60000);
    document.querySelectorAll('#direct-steps textarea')[1].value = answer;
    button('direct.connect').click();
  } else {
    const invite = await until(() => invoke('test_get', { dir, name: 'invite.txt' }), 60000);
    document.querySelector('#direct-steps textarea').value = invite;
    button('direct.make_answer').click();
    const answer = await until(outgoing, 30000);
    if (!answer) { invoke('test_report', { msg: `direct answer failed: ${$('join-error').textContent}` }); return; }
    await invoke('test_put', { dir, name: 'answer.txt', text: answer });
    invoke('test_report', { msg: `direct answer ${answer.length} chars` });
  }
  await until(() => state.snap && state.snap.direct === 'connected', 30000);
  await wait(2000);
  invoke('test_report', { msg: `direct connected; join form shown ${!$('join').classList.contains('hidden')}; peers ${state.snap.peers.map((p) => p.name).join(',')}` });
  if (rest.includes('live')) { await wait(1000); $('go-live').click(); await wait(2500); $('gl-start').click(); }
  const w = rest.find((x) => x.startsWith('watch:'));
  if (w) {
    const who = w.slice(6);
    const chip = await until(() => [...document.querySelectorAll('.chip')].find((c) => c.querySelector('b')?.textContent === who), 30000);
    if (chip) chip.querySelector('button').click();
    invoke('test_report', { msg: `direct watch ${who}: ${chip ? 'pressed' : 'no stream to watch'}` });
  }
  await wait(6000); { const w = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT); let nulls = 0; while (w.nextNode()) if (w.currentNode.data.trim() === 'null') nulls++; const sc = (id) => { const e = $(id); return e ? `${e.scrollHeight}/${e.clientHeight}` : '-'; }; invoke('test_report', { msg: `direct page null text nodes ${nulls}; scroll content ${sc('content')} main ${(() => { const m = document.querySelector('.main'); return `${m.scrollHeight}/${m.clientHeight}`; })()} window ${innerHeight}` }); }
  if (rest.includes('leave')) { await wait(12000); $('leave').click(); invoke('test_report', { msg: 'direct left' }); }
  await until(() => state.snap && state.snap.direct !== 'connected', 90000);
  await wait(1500);
  invoke('test_report', { msg: `direct form after the end: ${$('direct-steps').innerText.replace(/\s+/g, ' ').slice(0, 80)}` });
});invoke('test_upload_limit').then((v) => { if (v) setUploadLimit(v); });
invoke('test_open_golive').then((on) => { if (on) setTimeout(() => { $('go-live').click(); if (on === 'start') setTimeout(() => $('gl-start').click(), 2500); if (on === 'audio') setTimeout(() => { const pick = () => document.querySelector('input[name=audio]:checked')?.value; const screen = pick(); $('gl-tab-window').click(); setTimeout(() => invoke('test_report', { msg: `audio screen ${screen} window ${pick()} prefs ${JSON.stringify(settings().stream)}` }), 800); }, 2500); }, 3000); });
// Tests only: settings given by the test run, before anything reads them.
invoke('test_prefs').then((p) => { if (p) saveSettings({ ...settings(), ...JSON.parse(p) }); });
fillLanguages();
loadForm();
invoke('set_close_to_tray', { on: settings().tray });
// At start: check for an update, and install it right away unless turned off (nothing is
// watched or streamed yet). Then every 6 hours, only to show it.
invoke('updates_enabled').then((on) => {
  if (!on) return;
  setTimeout(() => checkUpdate(false, settings().autoUpdate), 1500);
  setInterval(() => checkUpdate(false, false), 6 * 3600 * 1000);
});
// With a saved room, start in the last one opened; the join form is only for new rooms.
// Not when a test joins on its own (STREAMER_APP_AUTOJOIN).
invoke('snapshot').then(async (s) => {
  if (s.link) { handleLink(s.link); return; }
  if (s.connected) return;
  if (await invoke('test_autojoin')) return;
  let last = null;
  try { last = JSON.parse(localStorage.getItem('lastRoom') || 'null'); } catch {}
  const list = savedRooms();
  const r = list.find((x) => sameRoom(x, last)) || list[list.length - 1];
  if (!r) return;
  // A build for one server always uses it, whatever was saved.
  const fixed = await invoke('built_in_server');
  switchRoom(fixed ? { ...r, server: fixed } : r);
});
applyStrings();
tick();
setInterval(tick, 1000);
