const { invoke } = window.__TAURI__.core;
const val = (id) => document.getElementById(id).value;

document.getElementById('watch').addEventListener('click', async () => {
  try {
    const r = await invoke('watch', { server: val('server'), room: val('room'), streamer: val('streamer') });
    document.getElementById('stats').textContent = r;
  } catch (e) {
    document.getElementById('stats').textContent = 'error: ' + e;
  }
});

setInterval(async () => {
  const s = await invoke('stats');
  if (s) document.getElementById('stats').textContent = s;
}, 1000);
