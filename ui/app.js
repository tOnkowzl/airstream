/* Airstream — cast-only remote console (no in-app video). */
const $ = (s) => document.querySelector(s);
const el = (t, c, txt) => { const n = document.createElement(t); if (c) n.className = c; if (txt != null) n.textContent = txt; return n; };

let STATE = null;
let items = [];
let selected = null;
let deviceId = '';
let position = { elapsed: 0, duration: 0, state: 'IDLE' };
let probeQueue = [], probing = 0;
let nowHls = false;

const fmt = (s) => {
  if (!isFinite(s) || s <= 0) return '--:--:--';
  s = Math.floor(s);
  const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60), x = s % 60;
  return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}:${String(x).padStart(2, '0')}`;
};

function toast(msg, ms = 2800) {
  const t = $('#toast');
  t.textContent = msg;
  t.classList.remove('hidden');
  clearTimeout(t._h);
  t._h = setTimeout(() => t.classList.add('hidden'), ms);
}

async function api(path, opts) {
  const r = await fetch(path, opts);
  const txt = await r.text();
  let j = null;
  try { j = txt ? JSON.parse(txt) : null; } catch { j = { raw: txt }; }
  if (!r.ok) throw new Error((j && (j.error || j.raw)) || ('HTTP ' + r.status));
  return j;
}
const post = (p, body) => api(p, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(body || {}) });

/* ----------------------------- boot ----------------------------- */
async function boot() {
  STATE = await api('/api/state');
  const s = STATE.prefs.settings || {};
  deviceId = s.last_device || '';

  $('#setVersion').textContent = `airstream ${STATE.version} (Rust)`;
  $('#setHw').textContent = STATE.hardware;
  $('#setUrl').textContent = `http://${STATE.lanIp}:${STATE.port}`;
  $('#setPrefs').textContent = STATE.prefsPath;
  $('#footUrl').textContent = `http://${STATE.lanIp}:${STATE.port}`;

  fillSettings(s);
  if (STATE.now && STATE.now.device_name) {
    $('#devName').textContent = STATE.now.device_name;
    $('#npTitle').textContent = STATE.now.title || '—';
    nowHls = !!STATE.now.hls;
    $('#chipMode').textContent = STATE.now.mode || '—';
    $('#chipQuality').textContent = STATE.now.quality || '—';
    $('#chipProto').textContent = nowHls ? 'HLS (fMP4)' : 'direct play';
  } else {
    $('#devName').textContent = deviceId ? 'อุปกรณ์ที่เคยใช้' : 'เลือกอุปกรณ์';
  }
  render();
}

function fillSettings(s) {
  preferredRate = Number(s.playback_rate) || 1;
  updateSpeed(confirmedRate, rateAvailable);
  $('#sQuality').value = s.quality || 'variable-1080p';
  $('#sEncoder').value = s.encoder || 'auto';
  $('#sSegment').value = String(s.hls_segment || 4);
  $('#sAudioLang').value = s.audioLanguages || 'AUTO';
  $('#sSubLang').value = s.subtitleLanguages || 'TH;NONE';
  $('#sSubRender').checked = s.subtitleRendering !== false;
  $('#sSubScale').value = s.subtitleScaleAdjustment || s.subtitle_scale || 1;
  $('#sAutoPlay').checked = !!s.auto_play;
  $('#sLoop').checked = !!s.looping;
  $('#sResume').checked = s.resume !== false;
}

function readSettings() {
  return {
    playback_rate: preferredRate,
    quality: $('#sQuality').value,
    encoder: $('#sEncoder').value,
    hls_segment: parseInt($('#sSegment').value, 10) || 4,
    audioLanguages: $('#sAudioLang').value,
    audio_stream: '',
    subtitleLanguages: $('#sSubLang').value,
    subtitleRendering: parseInt($('#selSubs').value, 10) >= 0,
    subtitleScaleAdjustment: parseFloat($('#sSubScale').value) || 1,
    audio_track: parseInt($('#selAudio').value, 10),
    subtitle_track: parseInt($('#selSubs').value, 10),
    auto_play: $('#sAutoPlay').checked,
    looping: $('#sLoop').checked,
    resume: $('#sResume').checked,
    last_device: deviceId,
    last_item: selected || '',
  };
}

let saveTimer = null;
function scheduleSave() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(async () => {
    try { await post('/api/settings', readSettings()); toast('บันทึกการตั้งค่าแล้ว', 1200); }
    catch (e) { toast('บันทึกไม่สำเร็จ: ' + e.message); }
  }, 350);
}

/* ----------------------------- playlist ----------------------------- */
function render() {
  const pl = (STATE.prefs.playlists && STATE.prefs.playlists[0]) || { name: 'Playlist', items: [] };
  items = pl.items || [];
  $('#plName').textContent = pl.name || 'Playlist';
  $('#plCount').textContent = String(items.length);
  $('#footInfo').textContent = `${items.length} รายการ · ดับเบิลคลิกเพื่อ cast`;

  const rows = $('#rows');
  rows.innerHTML = '';
  if (!items.length) {
    const e = el('div', 'dev-empty', 'ยังไม่มีไฟล์ — กด “+ เพิ่มไฟล์” หรือ “+ เพิ่มโฟลเดอร์”');
    e.style.padding = '18px 14px';
    rows.appendChild(e);
    return;
  }
  for (const it of items) {
    const row = el('div', 'row');
    row.dataset.id = it.id;
    if (it.id === selected) row.classList.add('sel');
    if (STATE.now && STATE.now.item_id === it.id) row.classList.add('playing');
    row.appendChild(el('div', 'dot'));
    row.appendChild(el('div', 'name', it.title || it.path.split(/[\\/]/).pop()));
    const meta = el('div', 'meta');
    const codec = el('span', 'codec', (it.vcodec || '').toUpperCase());
    meta.appendChild(codec);
    meta.appendChild(document.createTextNode(it.duration > 0 ? fmt(it.duration) : ''));
    row.appendChild(meta);
    row.onclick = () => select(it.id);
    row.ondblclick = () => castItem(it.id);
    rows.appendChild(row);
    if (!it.duration) probeQueue.push(it.id);
  }
  pumpProbes();
}

function select(id) {
  selected = id;
  document.querySelectorAll('.row').forEach((r) => r.classList.toggle('sel', r.dataset.id === id));
  const it = items.find((x) => x.id === id);
  if (it && !(STATE.now && STATE.now.item_id)) {
    $('#npTitle').textContent = it.title || it.path;
    $('#npSub').textContent = 'พร้อม cast — กดปุ่ม Cast';
  }
  loadTracks(id);
}

/* ------------------------ track pickers (from real streams) ------------------------ */
let tracksFor = '';
let trackRequest = null;
function loadTracks(id) {
  if (!id) return Promise.resolve();
  if (tracksFor === id) return trackRequest || Promise.resolve();
  tracksFor = id;
  trackRequest = populateTracks(id);
  return trackRequest;
}
async function populateTracks(id) {
  const a = $('#selAudio'), s = $('#selSubs');
  a.innerHTML = '<option value="-1">กำลังอ่านไฟล์…</option>';
  s.innerHTML = '<option value="-1">กำลังอ่านไฟล์…</option>';
  try {
    const t = await api('/api/item/' + id + '/tracks');
    if (tracksFor !== id) return;
    a.innerHTML = '';
    a.appendChild(new Option('อัตโนมัติ (ค่าเริ่มต้นของไฟล์)', '-1'));
    (t.audio || []).forEach((tr) => {
      const label = `#${tr.n + 1} ${tr.lang || 'unknown'} · ${tr.codec}${tr.channels ? ' · ' + tr.channels + 'ch' : ''}${tr.title ? ' · ' + tr.title : ''}${tr.default ? ' ★' : ''}`;
      a.appendChild(new Option(label, String(tr.n)));
    });
    s.innerHTML = '';
    s.appendChild(new Option('ปิด', '-1'));
    (t.subtitles || []).forEach((tr) => {
      const label = `#${tr.n + 1} ${tr.lang || 'unknown'} · ${tr.codec}${tr.title ? ' · ' + tr.title : ''}${tr.burnable ? '' : ' (ฝังไม่ได้)'}`;
      const o = new Option(label, String(tr.n));
      if (!tr.burnable) o.disabled = true;
      s.appendChild(o);
    });
    const st = STATE.prefs.settings || {};
    if (st.audio_track >= 0) a.value = String(st.audio_track);
    if (st.subtitle_track >= 0) s.value = String(st.subtitle_track);
    if (!s.value) s.value = '-1';
    if (!a.value) a.value = '-1';
    $('#sSubRender').checked = parseInt(s.value, 10) >= 0;
    // auto-pick preferred language when nothing explicit is chosen
    if (st.audio_track === -1 && st.audioLanguages) {
      const pref = st.audioLanguages.toUpperCase();
      const lang = {THA:'TH',ENG:'EN',JPN:'JP',KOR:'KO'};
      const hit = (t.audio || []).find((tr) => (lang[(tr.lang || '').toUpperCase()] || (tr.lang || '').toUpperCase()) === pref);
      if (hit) a.value = String(hit.n);
    }
    if (st.subtitle_track === -1 && (st.subtitleLanguages || '').startsWith('TH')) {
      const hit = (t.subtitles || []).find((tr) => ['TH','THA'].includes((tr.lang || '').toUpperCase()) && tr.burnable);
      if (hit) s.value = String(hit.n);
    }
    $('#sSubRender').checked = parseInt(s.value, 10) >= 0;
  } catch (e) {
    a.innerHTML = '<option value="-1">อ่านไฟล์ไม่สำเร็จ</option>';
    s.innerHTML = '<option value="-1">ปิด</option>';
  }
}

$('#btnApplyTracks').onclick = async () => {
  await post('/api/settings', readSettings());
  toast('ใช้ค่าที่เลือก');
  const target = (STATE.now && STATE.now.item_id) || selected;
  if (target) castItem(target);
};

function pumpProbes() {
  while (probing < 3 && probeQueue.length) {
    const id = probeQueue.shift();
    probing++;
    post('/api/item/' + id + '/info')
      .then((mi) => {
        const it = items.find((x) => x.id === id);
        if (it && mi) {
          it.duration = mi.duration || 0;
          it.vcodec = mi.vcodec || '';
          it.passthrough = mi.passthrough_ok;
          const row = document.querySelector(`.row[data-id="${id}"]`);
          if (row) {
            row.querySelector('.meta').lastChild.textContent = it.duration > 0 ? fmt(it.duration) : '';
            row.querySelector('.codec').textContent = it.vcodec.toUpperCase();
          }
        }
      })
      .catch(() => {})
      .finally(() => { probing--; pumpProbes(); });
  }
}

/* ----------------------------- position polling ----------------------------- */
async function pollPosition() {
  try {
    const p = await api('/api/position');
    updateSpeed(p.playback_rate || 1, p.via === 'cast' && ['PLAYING','PAUSED','BUFFERING'].includes(p.state));
    if (p.state === 'NO_DEVICE') { $('#tState').textContent = 'NO DEVICE'; $('#tState').className = 'state'; return; }
    position = p;
    const total = p.duration > 0 ? p.duration : (items.find((i) => i.id === selected)?.duration || 0);
    const pct = total > 0 ? Math.min(100, (p.elapsed / total) * 100) : 0;
    $('#fill').style.width = pct + '%';
    $('#knob').style.left = pct + '%';
    $('#tElapsed').textContent = fmt(p.elapsed);
    $('#tTotal').textContent = total > 0 ? fmt(total) : '--:--:--';
    const st = (p.state || 'IDLE').toUpperCase();
    const map = { PLAYING: ['playing', 'PLAYING'], PAUSED: ['paused', 'PAUSED'], STOPPED: ['stopped', 'STOPPED'], NO_MEDIA_PRESENT: ['', 'NO MEDIA'], TRANSITIONING: ['', 'BUFFERING'] };
    const [cls, label] = map[st] || ['', st];
    $('#tState').textContent = label;
    $('#tState').className = 'state ' + cls;
    $('#iconPlay').style.display = st === 'PLAYING' ? 'none' : '';
    $('#iconPause').style.display = st === 'PLAYING' ? '' : 'none';
  } catch { /* renderer busy */ }
}
setInterval(pollPosition, 2000);
setInterval(async () => {
  try {
    const n = await api('/api/now');
    if (n && n.item_id) {
      if ($('#npTitle').textContent !== n.title) {
        $('#npTitle').textContent = n.title;
        $('#npSub').textContent = `casting on ${n.device_name}`;
      }
      $('#devName').textContent = n.device_name;
      deviceId = n.device_id || deviceId;
      $('#chipDev').textContent = n.device_name;
      $('#chipDev').classList.add('on');
      $('#chipMode').textContent = n.mode || '—';
      $('#chipQuality').textContent = n.quality || '—';
      $('#chipProto').textContent = n.hls ? 'HLS (fMP4)' : 'direct play';
    }
  } catch {}
}, 5000);

/* ----------------------------- cast ----------------------------- */
async function castItem(id) {
  if (!deviceId) { openDevices(); toast('เลือกอุปกรณ์ก่อน'); return; }
  const it = items.find((x) => x.id === id);
  toast(`กำลังส่ง ${it ? it.title : ''} …`, 4000);
  try {
    // A speed click can still be persisting when Cast is clicked immediately.
    for (let i = 0; rateBusy && i < 100; i++) await new Promise(r => setTimeout(r, 50));
    if (rateBusy) throw new Error('กำลังบันทึกความเร็ว กรุณาลองกด Cast อีกครั้ง');
    await loadTracks(id);
    clearTimeout(saveTimer);
    const settings = {...readSettings(), last_item: id};
    await post('/api/settings', settings);
    STATE.prefs.settings = settings;
    const r = await post('/api/cast', { device: deviceId, id });
    if (r.preparing) {
      $('#npTitle').textContent = it ? it.title : '';
      $('#npSub').textContent = 'กำลังเตรียมไฟล์ให้ทีวี…';
      await waitPrepare(id, it);
      return castItem(id);
    }
    $('#npTitle').textContent = r.message ? (it ? it.title : '') : '';
    $('#npSub').textContent = `casting on ${r.device || ''}`;
    $('#devName').textContent = r.device || $('#devName').textContent;
    $('#chipDev').textContent = r.device || '—';
    $('#chipDev').classList.add('on');
    $('#chipMode').textContent = r.mode || '—';
    $('#chipQuality').textContent = r.quality || '—';
    $('#chipProto').textContent = (r.url || '').includes('.m3u8') ? 'HLS (fMP4)' : 'direct play';
    $('#mediaUrl').textContent = r.url || '';
    toast('ส่งขึ้นทีวีแล้ว');
    STATE.now = { item_id: id, title: it ? it.title : '', device_name: r.device };
    document.querySelectorAll('.row').forEach((row) => row.classList.toggle('playing', row.dataset.id === id));
    setTimeout(pollPosition, 800);
  } catch (e) {
    toast('Cast ไม่สำเร็จ: ' + e.message, 6000);
  }
}

/* first cast of a non-MP4 source prepares a faststart MP4 (with progress) */
async function waitPrepare(id, it) {
  for (let i = 0; i < 1200; i++) {
    await new Promise((r) => setTimeout(r, 1200));
    let st;
    try { st = await api('/api/prepare/' + id); } catch { continue; }
    if (st.state === 'ready') { toast('เตรียมไฟล์เสร็จ — เริ่มเล่น'); return; }
    if (st.state === 'error') { toast('เตรียมไฟล์ไม่สำเร็จ: ' + (st.note || ''), 6000); throw new Error(st.note || 'prepare failed'); }
    const pct = Math.round(st.progress || 0);
    $('#npSub').textContent = `กำลังเตรียมไฟล์ให้ทีวี… ${pct}%`;
    $('#fill').style.width = pct + '%';
    $('#knob').style.left = pct + '%';
  }
  throw new Error('prepare timeout');
}

$('#btnCast').onclick = () => { if (selected) castItem(selected); else toast('เลือกหนังก่อน'); };

/* ----------------------------- transport ----------------------------- */
async function transport(action) {
  try { await post('/api/transport', { action }); setTimeout(pollPosition, 500); }
  catch (e) { toast('คำสั่งไม่สำเร็จ: ' + e.message); }
}
$('#btnPlayPause').onclick = () => transport(position.state === 'PLAYING' ? 'pause' : 'play');
$('#btnStop').onclick = () => transport('stop');

async function seekTo(sec) {
  try { await post('/api/seek', { seconds: Math.max(0, sec) }); setTimeout(pollPosition, 700); }
  catch (e) { toast('seek ไม่สำเร็จ: ' + e.message); }
}
$('#btnBack10').onclick = () => seekTo(position.elapsed - 10);
$('#btnFwd30').onclick = () => seekTo(position.elapsed + 30);

$('#bar').onclick = (e) => {
  const total = position.duration || (items.find((i) => i.id === selected)?.duration || 0);
  if (!total) return;
  const r = e.currentTarget.getBoundingClientRect();
  const ratio = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
  seekTo(ratio * total);
};

/* ----------------------------- panels ----------------------------- */
$('#btnPickFiles').onclick = async () => {
  const r = await post('/api/pick_files');
  if (!r.path) return;
  const paths = r.path.split('\n').map((s) => s.trim()).filter(Boolean);
  const res = await post('/api/add', { paths });
  toast(`เพิ่ม ${res.added} ไฟล์` + (res.skipped ? ` (ข้ามซ้ำ ${res.skipped})` : ''));
  await boot();
};

$('#btnPickFolder').onclick = async () => {
  const r = await post('/api/pick_folder');
  if (!r.path) return;
  const res = await post('/api/add', { paths: [r.path] });
  toast(`เพิ่ม ${res.added} ไฟล์จากโฟลเดอร์` + (res.skipped ? ` (ข้ามซ้ำ ${res.skipped})` : ''));
  await boot();
};

$('#btnNewList').onclick = () => toast('หลาย playlist จะมาในเวอร์ชันถัดไป');

/* ----------------------------- devices ----------------------------- */
function closePops() { $('#devPopup').classList.add('hidden'); $('#setPopup').classList.add('hidden'); }

let knownDevices = [];
async function openDevices() {
  closePops();
  $('#devPopup').classList.remove('hidden');
  const list = $('#devList');
  // Keep the existing list visible while refreshing the snapshot.
  if (!list.children.length) list.innerHTML = '<div class="dev-empty">กำลังค้นหาอุปกรณ์เบื้องหลัง…</div>';
  try {
    const r = await api('/api/devices');
    list.innerHTML = '';
    knownDevices = r.devices;
    if (!r.devices.length) list.innerHTML = '<div class="dev-empty">ยังไม่พบอุปกรณ์ — กำลังค้นหาเบื้องหลัง ตรวจว่าทีวีอยู่ LAN เดียวกัน</div>';
    for (const d of r.devices) {
      const row = el('div', 'dev');
      if (d.id === deviceId) row.style.background = 'rgba(166,111,224,.2)';
      row.appendChild(el('span', 'kind ' + d.kind, d.kind));
      row.appendChild(el('span', 'name', d.name));
      row.appendChild(el('span', 'addr', `${d.host}:${d.port}`));
      row.onclick = () => {
        deviceId = d.id;
        $('#devName').textContent = d.name;
        $('#chipDev').textContent = d.name;
        $('#chipDev').classList.add('on');
        $('#devPopup').classList.add('hidden');
        scheduleSave();
        toast(`เลือก ${d.name}`);
      };
      list.appendChild(row);
    }
  } catch (e) {
    list.innerHTML = `<div class="dev-empty">ค้นหาล้มเหลว: ${e.message}</div>`;
  }
}
$('#btnDevices').onclick = openDevices;
$('#btnRefreshDevices').onclick = openDevices;

/* ----------------------------- settings ----------------------------- */
$('#btnSettings').onclick = () => { closePops(); $('#setPopup').classList.remove('hidden'); };
$('#btnCloseSet').onclick = () => $('#setPopup').classList.add('hidden');
['#sQuality', '#sEncoder', '#sSegment', '#sAudioLang', '#sSubLang', '#sSubRender', '#sSubScale', '#sAutoPlay', '#sLoop', '#sResume']
  .forEach((sel) => {
    const n = $(sel);
    n.addEventListener('change', scheduleSave);
    if (n.type === 'range') n.addEventListener('input', () => {
      $('#sSubScale').title = '×' + n.value;
    });
  });
$('#btnOpenPrefs').onclick = () => post('/api/open_prefs').catch(() => {});

document.addEventListener('click', (e) => {
  if (!e.target.closest('.popover') && !e.target.closest('#btnDevices') && !e.target.closest('#btnSettings')) closePops();
});
document.addEventListener('keydown', (e) => {
  if (e.code === 'Space') { e.preventDefault(); $('#btnPlayPause').click(); }
  if (e.key === 'ArrowLeft') seekTo(position.elapsed - 10);
  if (e.key === 'ArrowRight') seekTo(position.elapsed + 30);
  if (e.key === 'Enter' && selected) castItem(selected);
});

boot().catch((e) => toast('โหลดไม่สำเร็จ: ' + e.message, 6000));
pollPosition();

/* heartbeat: lets the app exit when this window is closed */
setInterval(() => fetch('/api/ping').catch(() => {}), 3000);
fetch('/api/ping').catch(() => {});

/* click outside closes popovers */
document.getElementById('scrim').onclick = closePops;
document.addEventListener('keydown', (e) => { if (e.key === 'Escape') closePops(); });

// Prime the device menu during startup, not after the user clicks it.
async function primeDevices() {
  try {
    const r = await api('/api/devices');
    knownDevices = r.devices || [];
    const last = STATE?.prefs?.settings?.last_device;
    const d = knownDevices.find(x => x.id === (deviceId || last));
    if (d) { deviceId = d.id; $('#devName').textContent = d.name; }
    if (!$('#devPopup').classList.contains('hidden')) await openDevices();
  } catch {}
}
primeDevices();
setInterval(primeDevices, 1500);

// Selecting a subtitle is the on/off control; keep the advanced toggle in sync.
$('#selSubs').addEventListener('change', () => {
  $('#sSubRender').checked = parseInt($('#selSubs').value, 10) >= 0;
  scheduleSave();
});
$('#selAudio').addEventListener('change', scheduleSave);
$('#sSubRender').addEventListener('change', () => {
  if (!$('#sSubRender').checked) $('#selSubs').value = '-1';
});

let rateBusy = false, confirmedRate = 1, preferredRate = 1, rateAvailable = false;
function updateSpeed(rate, available) {
  confirmedRate = rate;
  rateAvailable = available;
  const shown = available ? rate : preferredRate;
  document.querySelectorAll('[data-playback-rate]').forEach(b => {
    const active = Math.abs(Number(b.dataset.playbackRate) - shown) < 0.01;
    b.classList.toggle('active', active);
    b.setAttribute('aria-pressed', String(active));
    b.disabled = rateBusy;
  });
  $('#rateStatus').textContent = rateBusy ? 'กำลังบันทึก / รอทีวียืนยัน…' : available ? `ทีวียืนยัน ×${rate}` : `พร้อมเล่น ×${preferredRate} — เลือกไว้ก่อน Cast ได้`;
}
document.querySelectorAll('[data-playback-rate]').forEach(button => {
  button.addEventListener('click', async () => {
    if (rateBusy || !STATE) return;
    const wanted = Number(button.dataset.playbackRate);
    rateBusy = true;
    updateSpeed(confirmedRate, rateAvailable);
    try {
      if (rateAvailable) {
        const result = await post('/api/rate', {rate:wanted});
        confirmedRate = result.rate;
      }
      const settings = await api('/api/settings');
      settings.playback_rate = wanted;
      await post('/api/settings', settings);
      preferredRate = wanted;
      STATE.prefs.settings.playback_rate = wanted;
      toast(rateAvailable ? `ความเร็ว ×${confirmedRate}` : `เลือก ×${wanted} ไว้แล้ว — ใช้เมื่อกด Cast`);
    } catch (e) { toast(e.message, 6500); }
    finally { rateBusy = false; updateSpeed(confirmedRate, rateAvailable); await pollPosition(); }
  });
});
