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

const PRESETS = [2000, 4000, 8000, 12000, 20000, 35000, 50000];
// The streamer's own upload limit in Mbit/s, or 0 when unknown.
function uploadLimit() {
  try { return Math.max(0, +localStorage.getItem('uploadLimit') || 0); } catch { return 0; }
}
function setUploadLimit(v) {
  try { if (v > 0) localStorage.setItem('uploadLimit', String(v)); else localStorage.removeItem('uploadLimit'); } catch {}
}
const fmt = (v) => v.toLocaleString(lang, { maximumFractionDigits: 1 });
const VIEW_MODES = ['1440p60', '1080p60', '1080p30', '720p60', '720p30'];
const state = { snap: null, current: null, joinOpen: false, videosHidden: false, choice: {}, cardsKey: '', liveKey: '', gl: { tab: 'screen', pick: null, res: '1080', fps: '60', kbit: 20000, codec: 'h265', sources: null, thumbs: {} } };

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
  await call('connect', r, $('join-error'));
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
    // Show the join screen with this room filled in and the error.
    for (const k of ['room', 'name']) $('f-' + k).value = r[k];
    state.joinOpen = true;
    tick();
  }
}
function renderRail(s) {
  const list = savedRooms();
  const cur = state.current;
  if (cur && !list.some((x) => sameRoom(x, cur))) list.push(cur);
  const key = JSON.stringify(list) + '|' + JSON.stringify(cur) + '|' + lang;
  if ($('rail').dataset.key === key) return;
  $('rail').dataset.key = key;
  $('rail').replaceChildren(
    ...list.map((r) => el('button', {
      class: 'r' + (sameRoom(r, cur) ? ' on' : ''), type: 'button', title: r.room, text: initials(r.room),
      onclick: () => { if (!sameRoom(r, state.current)) switchRoom(r); },
      oncontextmenu: (e) => { e.preventDefault(); openRoomMenu(e, r); },
    })),
    el('div', { class: 'sep' }),
    el('button', { class: 'r add', type: 'button', title: t('rooms.add'), text: '+', onclick: () => { state.joinOpen = true; tick(); } }));
}
function closeRoomMenu() { $('room-menu').classList.add('hidden'); }
function openRoomMenu(e, r) {
  const m = $('room-menu');
  const item = (text, fn, cls = '') => el('button', { type: 'button', class: cls, text, onclick: () => { closeRoomMenu(); fn(); } });
  m.replaceChildren(
    sameRoom(r, state.current) ? null : item(t('rooms.open'), () => switchRoom(r)),
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
  $('rename-title').textContent = t('rename.title', { room: r.room });
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
    for (const k of ['server', 'room', 'name']) if (saved[k]) $('f-' + k).value = saved[k];
  } catch {}
  // A build made for one server has no server field: the address is fixed.
  invoke('built_in_server').then((url) => {
    if (!url) return;
    $('f-server').value = url;
    $('f-server').closest('label').classList.add('hidden');
  });
}
$('join-form').addEventListener('submit', async (ev) => {
  ev.preventDefault();
  $('join-error').classList.add('hidden');
  const args = { server: $('f-server').value.trim(), room: $('f-room').value.trim(), name: $('f-name').value.trim() };
  try { await joinRoom(args); } catch {}
});
$('join-back').addEventListener('click', () => { state.joinOpen = false; tick(); });
$('leave').addEventListener('click', async () => { await invoke('leave'); state.current = null; $('rail').dataset.key = ''; tick(); });

// ---------- render ----------
function render(s) {
  // Boolean on purpose: toggle(name, undefined) flips the class instead of clearing it.
  const joinShown = !s.connected || !!state.joinOpen;
  $('join').classList.toggle('hidden', !joinShown);
  $('join-back').classList.toggle('hidden', !s.connected);
  $('shell').classList.toggle('hidden', joinShown);
  if (!s.connected) { syncVideos(); return; }
  const me = s.peers.find((p) => p.me) || { name: '?' };
  // Joined some other way (tests): remember what we are in.
  if (!state.current || state.current.room !== s.room) {
    state.current = { server: $('f-server').value.trim(), room: s.room, name: me.name };
    rememberRoom(state.current);
    try { localStorage.setItem('lastRoom', JSON.stringify(state.current)); } catch {}
  }
  $('room-name').textContent = s.room;
  $('me-avatar').replaceWith(Object.assign(avatar(me.name), { id: 'me-avatar' }));
  $('me-state').textContent = s.live ? t('room.live_for', { t: clock(s.live.since_secs) }) : t('room.not_live');
  $('me-state').style.color = s.live ? '#ff9aa0' : '';
  $('go-live').classList.toggle('hidden', !!s.live);

  renderRail(s);
  renderPeople(s);
  renderHeader(s);
  renderContent(s);
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
      el('button', { class: 'btn', type: 'button', 'data-k': 'ov', 'aria-pressed': 'false', title: 'F3', text: t('tile.stats'), onclick: (e) => act(e.currentTarget.getAttribute('aria-pressed') === 'true' ? 'no_overlay' : 'overlay') }),
      el('button', { class: 'btn icon', type: 'button', 'data-k': 'pop', 'data-action': 'pop_out', text: '⧉', onclick: (e) => { act(e.currentTarget.dataset.action); setTimeout(tick, 150); } }),
      el('button', { class: 'btn icon', type: 'button', title: t('tile.fullscreen'), 'aria-label': t('tile.fullscreen'), text: '⛶', onclick: () => act('toggle_fullscreen') }),
      el('button', { class: 'btn danger', type: 'button', text: t('card.stop'), onclick: async () => { await invoke('unwatch', { streamer: p.id }); tick(); } })));
}
// Someone live we are not watching.
function chip(p) {
  return el('div', { class: 'chip' }, avatar(p.name), el('b', { text: p.name }),
    el('button', { class: 'btn primary', type: 'button', text: t('card.watch'), onclick: async () => { await invoke('watch', { streamer: p.id, quality: null }); tick(); } }));
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
  panel.replaceChildren(
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
    el('p', { class: 'small', style: 'margin:0', text: t('live.isolation') }));
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
    ? (g.sources?.monitors || []).map((m) => ({ id: m.handle, title: `${m.name.replace('\\\\.\\', '')}${m.primary ? ' · ' + t('golive.primary') : ''}`, sub: `${m.width}×${m.height}` }))
    : (g.sources?.windows || []).map((w) => ({ id: w.hwnd, title: w.title, sub: w.exe, minimized: w.minimized, pid: w.pid }));
  if (!list.some((x) => g.pick && x.id === g.pick.id)) g.pick = g.tab === 'screen' ? list[0] || null : null;
  const shot = (id) => g.thumbs[`${g.tab}:${id}`] || {};
  $('gl-sources').replaceChildren(...list.map((x) => el('button', { type: 'button', class: 'source', 'aria-pressed': String(g.pick?.id === x.id), 'data-src': x.id, onclick: () => { g.pick = x; renderGoLive(); } },
    el('span', { class: 'shot' }, shot(x.id).image ? el('img', { src: shot(x.id).image, alt: '' }) : el('span', { class: 'small', text: x.minimized ? t('golive.minimized') : '' })),
    el('span', { class: 'src-title' }, shot(x.id).icon ? el('img', { class: 'src-icon', src: shot(x.id).icon, alt: '' }) : null, el('span', { text: x.title })),
    el('span', { class: 'small', text: x.sub }))));
  $('gl-audio-app').classList.toggle('hidden', g.tab !== 'window');
  if (g.tab !== 'window' && document.querySelector('input[name=audio]:checked')?.value === 'app') document.querySelector('input[name=audio][value=system]').checked = true;
  seg($('gl-res'), [['720', '720p'], ['1080', '1080p'], ['1440', '1440p']], g.res, (v) => { g.res = v; renderGoLive(); });
  seg($('gl-fps'), [['30', '30'], ['60', '60']], g.fps, (v) => { g.fps = v; renderGoLive(); });
  seg($('gl-kbit'), PRESETS.map((k) => [k, mbit(k)]), g.kbit, (v) => { g.kbit = v; renderGoLive(); });
  seg($('gl-codec'), [['h265', 'H.265'], ['h264', 'H.264']], g.codec, (v) => { g.codec = v; renderGoLive(); });
  $('gl-codec-note').textContent = t(g.codec === 'h264' ? 'golive.codec_h264' : 'golive.codec_h265');
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
$('go-live').addEventListener('click', async () => {
  $('gl-error').classList.add('hidden');
  state.gl.sources = await invoke('sources');
  state.gl.thumbs = {};
  renderGoLive();
  $('golive').showModal();
  refreshThumbs().then(renderGoLive);
});
$('gl-tab-screen').addEventListener('click', () => { state.gl.tab = 'screen'; renderGoLive(); refreshThumbs(); });
$('gl-tab-window').addEventListener('click', () => { state.gl.tab = 'window'; renderGoLive(); refreshThumbs(); });
$('gl-cancel').addEventListener('click', () => $('golive').close());
$('gl-limit').addEventListener('input', (e) => { setUploadLimit(+e.target.value); renderGoLive(); });
$('gl-start').addEventListener('click', async () => {
  const g = state.gl;
  if (!g.pick) { $('gl-error').textContent = t('golive.pick_window'); $('gl-error').classList.remove('hidden'); return; }
  const audio = document.querySelector('input[name=audio]:checked').value;
  await call('go_live', { req: { kind: g.tab === 'screen' ? 'monitor' : 'window', id: g.pick.id, audio, audio_pid: g.pick.pid ?? null, mode: `${g.res}p${g.fps}`, kbit: g.kbit, codec: g.codec } }, $('gl-error'));
  $('golive').close();
  tick();
});

// ---------- loop ----------
function showNotice(n) {
  if (!n) return;
  const [kind, rest] = [n.split(':')[0], n.slice(n.indexOf(':') + 1)];
  const text = kind === 'lost' ? t('notice.lost') : kind === 'watch_ended' ? t('notice.watch_ended', { reason: rest }) : t('notice.error', { msg: rest });
  const box = $('join-error');
  box.textContent = text;
  box.classList.remove('hidden');
}
async function tick() {
  try {
    const s = await invoke('snapshot');
    state.snap = s;
    showNotice(s.notice);
    render(s);
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
invoke('test_rooms').then((other) => {
  if (!other) return;
  setTimeout(async () => {
    const first = state.current;
    await switchRoom({ ...first, room: other });
    setTimeout(() => {
      const btn = [...document.querySelectorAll('#rail .r')].find((b) => b.title === first.room);
      if (btn) btn.click();
      setTimeout(() => {
        const b2 = [...document.querySelectorAll('#rail .r')].find((b) => b.title === other);
        if (b2) { const r = b2.getBoundingClientRect(); b2.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, clientX: r.right - 6, clientY: r.top + 10 })); }
      }, 4000);
    }, 5000);
  }, 5000);
});
invoke('test_upload_limit').then((v) => { if (v) setUploadLimit(v); });
invoke('test_open_golive').then((on) => { if (on) setTimeout(() => $('go-live').click(), 3000); });
fillLanguages();
loadForm();
// With a saved room, start in the last one opened; the join form is only for new rooms.
// Not when a test joins on its own (STREAMER_APP_AUTOJOIN).
invoke('snapshot').then(async (s) => {
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
