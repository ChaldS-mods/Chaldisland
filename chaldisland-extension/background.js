// Sends the state of Chrome downloads to the Island app (it listens on 127.0.0.1:47821).
const URL_ISLAND = 'http://127.0.0.1:47821/dl';
let timer = null;

const base = p => (p || '').split(/[\\/]/).pop();
const host = u => { try { return new URL(u).hostname.replace(/^www\./, ''); } catch { return ''; } };
const fromUrl = u => { try { return decodeURIComponent(base((u || '').split('?')[0])); } catch { return base((u || '').split('?')[0]); } };

function map(i) {
  const state = i.state === 'complete' ? 'complete'
    : i.state === 'interrupted' ? (i.error === 'USER_CANCELED' ? 'cancelled' : 'interrupted')
    : 'running';
  return {
    id: i.id,
    name: base(i.filename) || fromUrl(i.finalUrl || i.url) || 'файл',
    path: i.filename || '',
    host: host(i.finalUrl || i.url),
    mime: i.mime || '',
    loaded: i.bytesReceived || 0,
    total: i.totalBytes > 0 ? i.totalBytes : (i.fileSize > 0 ? i.fileSize : 0),
    state,
    paused: !!i.paused,
    err: i.error || ''
  };
}

async function push(list) {
  if (!list.length) return;
  try {
    await fetch(URL_ISLAND, { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Island': '1' }, body: JSON.stringify(list) });
  } catch { /* Island is not running */ }
}

// chrome.downloads has no progress event, so poll while something is downloading
async function tick() {
  const items = await chrome.downloads.search({ state: 'in_progress' });
  await push(items.map(map));
  if (!items.length) { clearInterval(timer); timer = null; }
}
function start() { if (!timer) { timer = setInterval(tick, 500); tick(); } }

chrome.downloads.onCreated.addListener(start);
chrome.downloads.onChanged.addListener(async d => {
  if (!(d.state || d.error || d.paused || d.filename)) return;
  const [i] = await chrome.downloads.search({ id: d.id });
  if (!i) return;
  await push([map(i)]);
  if (i.state === 'in_progress') start();
});
chrome.runtime.onStartup.addListener(start);
chrome.runtime.onInstalled.addListener(start);

// heartbeat: lets Island know the extension is installed (so it stops suggesting it)
const hello = () => push([{ hello: 1 }]);
hello();
chrome.alarms.create('island-hello', { periodInMinutes: 0.5 });
chrome.alarms.onAlarm.addListener(a => { if (a.name === 'island-hello') hello(); });
