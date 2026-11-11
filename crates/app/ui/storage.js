// What the UI saves: kept in localStorage and, after every change, also written to
// %LOCALAPPDATA%\Backseat\state.json, so a later build that has no WebView2 can read the
// user's rooms and settings. Values that are JSON go into the file as JSON, others as text.
const STORED_KEYS = ['settings', 'rooms', 'join', 'lastRoom', 'uploadLimit', 'lang'];
let mirrored = null;
function storedAll() {
  const all = {};
  for (const k of STORED_KEYS) {
    const v = stored(k);
    if (v == null) continue;
    try { all[k] = JSON.parse(v); } catch { all[k] = v; }
  }
  return all;
}
function mirrorState() {
  const json = JSON.stringify(storedAll());
  if (json === mirrored) return;
  mirrored = json;
  window.__TAURI__.core.invoke('state_save', { json }).catch((e) => { mirrored = null; console.error('state_save', e); });
}
function stored(key) {
  try { return localStorage.getItem(key); } catch { return null; }
}
function store(key, value) {
  try { localStorage.setItem(key, value); } catch {}
  mirrorState();
}
function unstore(key) {
  try { localStorage.removeItem(key); } catch {}
  mirrorState();
}
