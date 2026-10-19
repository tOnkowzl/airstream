/* Airstream — cast-only remote console (no in-app video). */
const $ = (s) => document.querySelector(s);
const el = (t, c, txt) => { const n = document.createElement(t); if (c) n.className = c; if (txt != null) n.textContent = txt; return n; };

let STATE = null;
let items = [];
let selected = null;
// Multi-selection: `selected` is the primary item (shown in the track pickers).
let multi = new Set();
let anchorId = null;
let deviceId = '';
let position = { elapsed: 0, duration: 0, state: 'IDLE' };
let probeQueue = [], probing = 0;
let nowHls = false;
let playlistRendered = false;
let dragId = null;
let dragIds = [];
// Remembered track choices (sent to the server, applied to every new episode).
let audioPref = null, subtitlePref = null, subtitleOff = false;
let loadedTracks = { audio: [], subtitles: [] };
const ITEM_MIME = 'application/x-airstream-item';

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
  const lastId = s.last_item;
  if (lastId && items.some((x) => x.id === lastId)) select(lastId);
}

function fillSettings(s) {
  audioPref = s.audio_pref || null;
  subtitlePref = s.subtitle_pref || null;
  subtitleOff = !!s.subtitle_off;
  preferredRate = Number(s.playback_rate) || 1;
  updateSpeed(confirmedRate, rateAvailable);
  $('#sQuality').value = s.quality || 'variable-1080p';
  $('#sEncoder').value = s.encoder || 'auto';
  $('#sSegment').value = String(s.hls_segment || 4);
  $('#sAudioLang').value = s.audioLanguages || 'AUTO';
  $('#sSubLang').value = s.subtitleLanguages || 'TH;NONE';
  $('#sSubRender').checked = s.subtitleRendering !== false;
  $('#sSubScale').value = s.subtitleScaleAdjustment || s.subtitle_scale || 1;
  $('#sAutoPlay').checked = s.auto_play !== false;
  $('#sResume').checked = s.resume !== false;
}

function readSettings() {
  return {
    playback_rate: preferredRate,
    audio_pref: audioPref,
    subtitle_pref: subtitlePref,
    subtitle_off: subtitleOff,
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
    autoplay_choice: true,
    looping: false,
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
  multi = new Set(Array.from(multi).filter((id) => items.some((x) => x.id === id)));
  if (selected && !items.some((x) => x.id === selected)) selected = multi.size ? Array.from(multi).pop() : null;
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
  rows.classList.toggle('noanim', playlistRendered);
  playlistRendered = true;
  for (const it of items) {
    const row = el('div', 'row');
    row.dataset.id = it.id;
    row.draggable = true;
    if (it.id === selected || multi.has(it.id)) row.classList.add('sel');
    if (STATE.now && STATE.now.item_id === it.id) row.classList.add('playing');
    const grip = el('div', 'grip', '⋮⋮');
    grip.title = 'ลากเพื่อสลับตำแหน่ง';
    row.appendChild(grip);
    row.appendChild(el('div', 'dot'));
    const title = it.title || it.path.split(/[\\/]/).pop();
    row.appendChild(el('div', 'name', title));
    const meta = el('div', 'meta');
    const codec = el('span', 'codec', (it.vcodec || '').toUpperCase());
    meta.appendChild(codec);
    meta.appendChild(document.createTextNode(it.duration > 0 ? fmt(it.duration) : ''));
    if (it.resume_pos > 10 && (!it.duration || it.resume_pos < it.duration - 30)) {
      const badge = el('span', 'resume', '▶ ' + fmt(it.resume_pos));
      badge.title = 'ดูต่อจากนาทีนี้';
      meta.insertBefore(badge, meta.firstChild);
    }
    row.appendChild(meta);
    const del = el('button', 'rowdel', '✕');
    del.type = 'button';
    del.title = 'เอาออกจาก Playlist (ไม่ลบไฟล์จริง)';
    del.setAttribute('aria-label', `เอา ${title} ออกจาก Playlist`);
    del.onclick = (ev) => { ev.stopPropagation(); removeItems(multi.has(it.id) && multi.size > 1 ? Array.from(multi) : [it.id]); };
    del.ondblclick = (ev) => ev.stopPropagation();
    row.appendChild(del);
    row.onclick = (ev) => clickRow(ev, it.id);
    row.ondblclick = () => castItem(it.id);
    row.addEventListener('dragstart', (ev) => {
      dragId = it.id;
      dragIds = multi.has(it.id) && multi.size > 1 ? items.filter((x) => multi.has(x.id)).map((x) => x.id) : [it.id];
      ev.dataTransfer.effectAllowed = 'move';
      ev.dataTransfer.setData(ITEM_MIME, it.id);
      ev.dataTransfer.setData('text/plain', title);
      requestAnimationFrame(() => row.classList.add('dragging'));
    });
    row.addEventListener('dragend', () => { dragId = null; dragIds = []; row.classList.remove('dragging'); clearDropMarks(); });
    rows.appendChild(row);
    if (!it.duration) probeQueue.push(it.id);
  }
  paintSelection();
  pumpProbes();
}

function showSelectedTitle() {
  const it = items.find((x) => x.id === selected);
  if (it && !(STATE.now && STATE.now.item_id)) $('#npTitle').textContent = it.title || it.path;
}
function paintSelection() {
  showSelectedTitle();
  document.querySelectorAll('.row').forEach((r) => r.classList.toggle('sel', multi.has(r.dataset.id)));
  const button = $('#btnRemoveSel');
  button.classList.toggle('hidden', multi.size < 2);
  button.textContent = `ลบที่เลือก (${multi.size})`;
  if (multi.size > 1) $('#footInfo').textContent = `เลือก ${multi.size} จาก ${items.length} รายการ · Delete เพื่อเอาออก · Esc ยกเลิก`;
  else $('#footInfo').textContent = `${items.length} รายการ · ดับเบิลคลิกเพื่อ cast`;
}

// Click: plain = one, Ctrl/Cmd = toggle, Shift = range from the anchor row.
function clickRow(ev, id) {
  if (ev.shiftKey && anchorId && items.some((x) => x.id === anchorId)) {
    const a = items.findIndex((x) => x.id === anchorId);
    const b = items.findIndex((x) => x.id === id);
    const [from, to] = a < b ? [a, b] : [b, a];
    multi = new Set(items.slice(from, to + 1).map((x) => x.id));
    selected = id;
    paintSelection();
    return;
  }
  if (ev.ctrlKey || ev.metaKey) {
    if (multi.has(id)) {
      multi.delete(id);
      if (selected === id) selected = multi.size ? Array.from(multi).pop() : null;
    } else {
      multi.add(id);
      selected = id;
    }
    anchorId = id;
    paintSelection();
    if (selected) loadTracks(selected);
    return;
  }
  select(id);
}

function select(id) {
  selected = id;
  multi = new Set([id]);
  anchorId = id;
  paintSelection();
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
    loadedTracks = { audio: t.audio || [], subtitles: t.subtitles || [] };
    // The server resolves the saved choices against THIS file (same language/title/codec).
    const r = t.resolved || { audio: -1, subtitle: -1 };
    a.value = String(r.audio);
    s.value = String(r.subtitle);
    if (!a.value) a.value = '-1';
    if (!s.value) s.value = '-1';
    $('#sSubRender').checked = parseInt(s.value, 10) >= 0;
    showTrackHint(r);
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
    watchPlaylistAdvance(p, st, total);
    const map = { PLAYING: ['playing', 'PLAYING'], PAUSED: ['paused', 'PAUSED'], STOPPED: ['stopped', 'STOPPED'], NO_MEDIA_PRESENT: ['', 'NO MEDIA'], TRANSITIONING: ['', 'BUFFERING'] };
    const [cls, label] = map[st] || ['', st];
    $('#tState').textContent = label;
    $('#tState').className = 'state ' + cls;
    $('#iconPlay').style.display = st === 'PLAYING' ? 'none' : '';
    $('#iconPause').style.display = st === 'PLAYING' ? '' : 'none';
  } catch { /* renderer busy */ }
}
setInterval(pollPosition, 2000);
setInterval(async () => { try { const fresh = await api('/api/state'); STATE.prefs = fresh.prefs; const byId = new Map((fresh.prefs.playlists[0].items || []).map((x) => [x.id, x])); items.forEach((x) => { const f = byId.get(x.id); if (f) x.resume_pos = f.resume_pos; }); document.querySelectorAll('.row').forEach((row) => { const it = byId.get(row.dataset.id); const badge = row.querySelector('.resume'); const show = it && it.resume_pos > 10 && (!it.duration || it.resume_pos < it.duration - 30); if (show && badge) badge.textContent = '▶ ' + fmt(it.resume_pos); else if (show && !badge) { const b = el('span', 'resume', '▶ ' + fmt(it.resume_pos)); const meta = row.querySelector('.meta'); if (meta) meta.insertBefore(b, meta.firstChild); } else if (!show && badge) badge.remove(); }); } catch {} }, 15000);
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
async function castItem(id, fromStart = false) {
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
    const r = await post('/api/cast', { device: deviceId, id, from_start: fromStart });
    if (r.preparing) {
      $('#npTitle').textContent = it ? it.title : '';
      $('#npSub').textContent = 'กำลังเตรียมไฟล์ให้ทีวี…';
      await waitPrepare(id, it);
      return castItem(id, fromStart);
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
    toast(r.resumed_from > 10 ? `ดูต่อจาก ${fmt(r.resumed_from)}` : 'ส่งขึ้นทีวีแล้ว');
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
// Play with a live TV session = pause/resume. Otherwise (just reopened the app, finished,
// stopped, TV app closed) start the selected item; saved resume position applies.
const LIVE_STATES = ['PLAYING', 'PAUSED', 'BUFFERING', 'TRANSITIONING'];
$('#btnPlayPause').onclick = () => {
  const live = position.via === 'cast' && LIVE_STATES.includes((position.state || '').toUpperCase());
  if (live) return transport(position.state === 'PLAYING' ? 'pause' : 'play');
  const target = selected || (STATE.now && STATE.now.item_id) || (items[0] && items[0].id);
  if (!target) { toast('เลือกหนังก่อน'); return; }
  if (!selected) select(target);
  castItem(target);
};
$('#btnStop').onclick = () => { if (position.via === 'cast' && LIVE_STATES.includes((position.state || '').toUpperCase())) transport('stop'); else toast('ไม่มีอะไรกำลังเล่นบนทีวี'); };

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
['#sQuality', '#sEncoder', '#sSegment', '#sAudioLang', '#sSubLang', '#sSubRender', '#sSubScale', '#sAutoPlay', '#sResume']
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


/* ------------------ playlist: remove, reorder, drop from Explorer ------------------ */
async function refreshPlaylist() {
  const fresh = await api('/api/state');
  STATE.prefs = fresh.prefs;
  STATE.now = fresh.now;
  render();
}

async function removeItems(ids) {
  const wanted = ids.filter((id) => items.some((x) => x.id === id));
  if (!wanted.length) return;
  const firstIndex = Math.min(...wanted.map((id) => items.findIndex((x) => x.id === id)));
  const names = wanted.map((id) => { const it = items.find((x) => x.id === id); return it.title || it.path; });
  const playing = STATE.now && wanted.includes(STATE.now.item_id);
  try {
    if (wanted.length === 1) await api('/api/item/' + encodeURIComponent(wanted[0]), { method: 'DELETE' });
    else await post('/api/playlist/remove', { ids: wanted });
    if (selected && wanted.includes(selected)) { selected = null; tracksFor = ''; }
    wanted.forEach((id) => multi.delete(id));
    await refreshPlaylist();
    if (!selected && items.length) select(items[Math.min(firstIndex, items.length - 1)].id);
    const what = wanted.length === 1 ? names[0] : `${wanted.length} รายการ`;
    toast(`เอาออกแล้ว: ${what}${playing ? ' (ทีวียังเล่นอยู่ กด หยุด เพื่อหยุด)' : ''} · ไฟล์จริงยังอยู่`, 4200);
  } catch (e) { toast('เอาออกไม่สำเร็จ: ' + e.message, 5000); }
}
const removeItem = (id) => removeItems([id]);
$('#btnRemoveSel').onclick = () => removeItems(Array.from(multi));

async function moveItems(ids, beforeId) {
  try {
    await post('/api/playlist/move', { ids, before: beforeId || null });
  } catch (e) { toast(e.message, 3500); }
  await refreshPlaylist();
}

async function moveItem(id, beforeId) {
  try {
    await post('/api/playlist/move', { id, before: beforeId || null });
  } catch (e) { toast(e.message, 3500); }
  await refreshPlaylist();
}

function listArea() { return document.querySelector('section.list'); }
function overList(e) {
  const r = listArea().getBoundingClientRect();
  return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
}
function clearDropMarks() {
  document.querySelectorAll('.row.drop-before').forEach((n) => n.classList.remove('drop-before'));
  $('#rows').classList.remove('drop-end');
}
function dropTarget(clientY) {
  for (const row of document.querySelectorAll('#rows .row')) {
    if (row.dataset.id === dragId || dragIds.includes(row.dataset.id)) continue;
    const box = row.getBoundingClientRect();
    if (clientY < box.top + box.height / 2) return { before: row.dataset.id, row };
  }
  return { before: null, row: null };
}
function markDrop(e) {
  clearDropMarks();
  if (!overList(e)) return;
  const target = dropTarget(e.clientY);
  if (target.row) target.row.classList.add('drop-before'); else $('#rows').classList.add('drop-end');
}
const hasType = (e, type) => Array.from((e.dataTransfer && e.dataTransfer.types) || []).includes(type);

document.addEventListener('dragenter', (e) => {
  if (hasType(e, 'Files')) { e.preventDefault(); $('#dropHint').classList.remove('hidden'); }
});
document.addEventListener('dragover', (e) => {
  const internal = hasType(e, ITEM_MIME);
  const external = hasType(e, 'Files');
  if (!internal && !external) return;
  e.preventDefault(); // also prevents Edge from navigating to a mistakenly dropped file
  e.dataTransfer.dropEffect = internal ? 'move' : 'copy';
  if (external) $('#dropHint').classList.remove('hidden');
  markDrop(e);
});
document.addEventListener('dragleave', (e) => {
  if (e.relatedTarget === null) { $('#dropHint').classList.add('hidden'); clearDropMarks(); }
});

const VIDEO_NAME = /\.(mkv|mp4|m4v|avi|mov|ts|m2ts|webm|flv|wmv|mpg|mpeg)$/i;
async function collectDropped(entries, plainFiles) {
  const out = [];
  let folders = 0;
  const addFile = (f) => { if (VIDEO_NAME.test(f.name) && out.length < 1000) out.push({ name: f.name, size: f.size, modified: Math.trunc(f.lastModified) }); };
  const walk = async (entry, depth) => {
    if (out.length >= 1000 || depth > 6) return;
    if (entry.isFile) addFile(await new Promise((ok, no) => entry.file(ok, no)));
    else if (entry.isDirectory) {
      folders++;
      const reader = entry.createReader();
      const children = [];
      for (;;) {
        const batch = await new Promise((ok, no) => reader.readEntries(ok, no));
        if (!batch.length) break;
        children.push(...batch);
      }
      // Episodes should keep natural order (E2 before E10), whatever order the browser returns.
      children.sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: 'base' }));
      for (const child of children) await walk(child, depth + 1);
    }
  };
  if (entries.length) { for (const entry of entries) await walk(entry, 0); }
  else plainFiles.forEach(addFile);
  return { files: out, folders };
}

let droppedBusy = false;
async function addDropped(entries, plainFiles, beforeId) {
  if (droppedBusy) { toast('กำลังเพิ่มไฟล์จากการวางก่อนหน้า กรุณารอสักครู่'); return; }
  droppedBusy = true;
  try {
    const { files } = await collectDropped(entries, plainFiles);
    if (!files.length) { toast('ไม่พบไฟล์วิดีโอที่รองรับในสิ่งที่วาง'); return; }
    toast(`กำลังหาตำแหน่งไฟล์ ${files.length} รายการ…`, 30000);
    const res = await post('/api/add_dropped', { files, before: beforeId || null });
    await refreshPlaylist();
    const names = (res.missing || []).slice(0, 2).join(', ');
    let msg = `เพิ่ม ${res.added} ไฟล์`;
    if (res.skipped) msg += ` · ข้ามซ้ำ ${res.skipped}`;
    if ((res.missing || []).length) msg += ` · หาไม่พบ ${res.missing.length} (${names}${res.missing.length > 2 ? '…' : ''}) ลองปุ่ม ＋ ไฟล์`;
    if (res.timed_out) msg += ' · ค้นหาหมดเวลา';
    toast(msg, 6500);
  } catch (e) { toast('เพิ่มจากการวางไม่สำเร็จ: ' + e.message, 6500); }
  finally { droppedBusy = false; }
}

document.addEventListener('drop', (e) => {
  const internal = hasType(e, ITEM_MIME);
  const external = hasType(e, 'Files');
  if (!internal && !external) return;
  e.preventDefault();
  $('#dropHint').classList.add('hidden');
  const target = overList(e) ? dropTarget(e.clientY) : { before: null };
  clearDropMarks();
  if (internal) {
    const id = e.dataTransfer.getData(ITEM_MIME) || dragId;
    const ids = dragIds.length ? dragIds.slice() : (id ? [id] : []);
    if (ids.length && overList(e)) moveItems(ids, target.before);
    return;
  }
  // DataTransfer objects expire after this handler returns: read them now.
  const entries = Array.from(e.dataTransfer.items || [])
    .filter((item) => item.kind === 'file')
    .map((item) => (item.webkitGetAsEntry ? item.webkitGetAsEntry() : null))
    .filter(Boolean);
  const plain = Array.from(e.dataTransfer.files || []);
  addDropped(entries, plain, target.before);
});

document.addEventListener('keydown', (e) => {
  const tag = (e.target.tagName || '').toLowerCase();
  if (['input', 'select', 'textarea'].includes(tag) || (!selected && !multi.size)) return;
  if (e.key === 'Delete') { e.preventDefault(); removeItems(multi.size ? Array.from(multi) : [selected]); return; }
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') { e.preventDefault(); multi = new Set(items.map((x) => x.id)); paintSelection(); return; }
  if (e.key === 'Escape' && multi.size > 1) { select(selected); return; }
  if (e.altKey && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
    e.preventDefault();
    const i = items.findIndex((x) => x.id === selected);
    const j = e.key === 'ArrowUp' ? i - 1 : i + 1;
    if (i < 0 || j < 0 || j >= items.length) return;
    moveItem(selected, e.key === 'ArrowUp' ? items[j].id : (items[j + 1] ? items[j + 1].id : null));
  }
});


/* ------------------ remember audio/subtitle for every episode ------------------ */
const trackName = (p) => [p.lang, p.title].filter(Boolean).join(' · ') || 'ไม่ระบุ';
function showTrackHint(resolved) {
  const notes = [];
  if (audioPref && resolved.audio < 0) notes.push(`ไฟล์นี้ไม่มีเสียงที่จำไว้ (${trackName(audioPref)}) — ใช้อัตโนมัติ`);
  if (subtitlePref && !subtitleOff && resolved.subtitle < 0) notes.push(`ไฟล์นี้ไม่มีซับที่จำไว้ (${trackName(subtitlePref)}) — ปิดซับ`);
  if (!notes.length && (audioPref || subtitlePref || subtitleOff)) notes.push('จำค่าเสียง/ซับ/ความเร็วนี้ไว้ใช้กับตอนถัดไปแล้ว');
  $('#trackHint').textContent = notes.join(' · ');
}
function trackPref(list, track) {
  return {
    lang: track.lang || '', title: track.title || '', codec: track.codec || '', channels: track.channels || 0,
    nth: list.slice(0, track.n).filter((x) => (x.lang || '') === (track.lang || '')).length,
  };
}
function rememberAudio() {
  const n = parseInt($('#selAudio').value, 10);
  const track = loadedTracks.audio[n];
  audioPref = n >= 0 && track ? trackPref(loadedTracks.audio, track) : null;
}
function rememberSubtitle() {
  const n = parseInt($('#selSubs').value, 10);
  const track = loadedTracks.subtitles[n];
  if (n >= 0 && track) { subtitlePref = trackPref(loadedTracks.subtitles, track); subtitleOff = false; }
  else { subtitlePref = null; subtitleOff = true; }
  $('#sSubRender').checked = n >= 0;
}
$('#selAudio').addEventListener('change', () => { rememberAudio(); showTrackHint({ audio: parseInt($('#selAudio').value, 10), subtitle: parseInt($('#selSubs').value, 10) }); scheduleSave(); });
$('#selSubs').addEventListener('change', () => { rememberSubtitle(); showTrackHint({ audio: parseInt($('#selAudio').value, 10), subtitle: parseInt($('#selSubs').value, 10) }); scheduleSave(); });
$('#sSubRender').addEventListener('change', () => {
  if (!$('#sSubRender').checked) { $('#selSubs').value = '-1'; rememberSubtitle(); scheduleSave(); }
});


/* ------------------ right-click menu (Shift+right-click extends the selection) ------------------ */
const ctxMenu = document.createElement('div');
ctxMenu.className = 'ctxmenu hidden';
ctxMenu.setAttribute('role', 'menu');
document.body.appendChild(ctxMenu);

function closeCtx() { ctxMenu.classList.add('hidden'); ctxMenu.innerHTML = ''; }

function ctxItem(label, hint, onPick, opts = {}) {
  const button = document.createElement('button');
  button.type = 'button';
  button.setAttribute('role', 'menuitem');
  if (opts.danger) button.classList.add('danger');
  button.disabled = !!opts.disabled;
  const text = document.createElement('span'); text.textContent = label;
  button.appendChild(text);
  if (hint) { const k = document.createElement('kbd'); k.textContent = hint; button.appendChild(k); }
  button.onclick = () => { closeCtx(); onPick(); };
  return button;
}

async function moveSelection(toTop) {
  const ids = items.filter((x) => multi.has(x.id)).map((x) => x.id);
  if (!ids.length) return;
  if (toTop) { await moveItems(ids, items.find((x) => !multi.has(x.id))?.id || null); }
  else await moveItems(ids, null);
}

function openCtx(ev, rowId) {
  const chosen = items.length && rowId;
  if (chosen) {
    if (ev.shiftKey) clickRow({ shiftKey: true }, rowId);
    else if (ev.ctrlKey || ev.metaKey) { if (!multi.has(rowId)) clickRow({ ctrlKey: true }, rowId); }
    else if (!multi.has(rowId)) select(rowId);
  }
  const ids = Array.from(multi);
  const many = ids.length > 1;
  ctxMenu.innerHTML = '';
  const head = document.createElement('div'); head.className = 'ctxhead';
  head.textContent = many ? `เลือกอยู่ ${ids.length} รายการ` : (items.find((x) => x.id === selected)?.title || 'Playlist');
  ctxMenu.appendChild(head);
  ctxMenu.appendChild(ctxItem('Cast ไปทีวี', 'ดับเบิลคลิก', () => selected && castItem(selected), { disabled: many || !selected }));
  ctxMenu.appendChild(ctxItem('เล่นตั้งแต่ต้น', '', () => selected && castItem(selected, true), { disabled: many || !selected }));
  ctxMenu.appendChild(ctxItem(many ? 'เลื่อนไปบนสุด' : 'เลื่อนไปบนสุด', '', () => moveSelection(true), { disabled: !ids.length }));
  ctxMenu.appendChild(ctxItem('เลื่อนไปล่างสุด', '', () => moveSelection(false), { disabled: !ids.length }));
  ctxMenu.appendChild(document.createElement('hr'));
  ctxMenu.appendChild(ctxItem('เลือกทั้งหมด', 'Ctrl+A', () => { multi = new Set(items.map((x) => x.id)); paintSelection(); }, { disabled: !items.length }));
  ctxMenu.appendChild(ctxItem('ยกเลิกการเลือก', 'Esc', () => { if (selected) select(selected); }, { disabled: !many }));
  ctxMenu.appendChild(document.createElement('hr'));
  ctxMenu.appendChild(ctxItem(many ? `เอาออกจาก Playlist (${ids.length})` : 'เอาออกจาก Playlist', 'Delete', () => removeItems(ids), { danger: true, disabled: !ids.length }));
  ctxMenu.classList.remove('hidden');
  const box = ctxMenu.getBoundingClientRect();
  ctxMenu.style.left = Math.max(6, Math.min(ev.clientX, window.innerWidth - box.width - 6)) + 'px';
  ctxMenu.style.top = Math.max(6, Math.min(ev.clientY, window.innerHeight - box.height - 6)) + 'px';
  const first = ctxMenu.querySelector('button:not(:disabled)');
  if (first) first.focus({ preventScroll: true });
}

document.addEventListener('contextmenu', (ev) => {
  const row = ev.target.closest && ev.target.closest('#rows .row');
  const inList = ev.target.closest && ev.target.closest('#rows');
  if (!row && !inList) { closeCtx(); return; }
  ev.preventDefault();
  openCtx(ev, row ? row.dataset.id : null);
});
document.addEventListener('mousedown', (ev) => { if (!ctxMenu.contains(ev.target)) closeCtx(); });
window.addEventListener('blur', closeCtx);
window.addEventListener('resize', closeCtx);
document.addEventListener('scroll', closeCtx, true);
document.addEventListener('keydown', (ev) => {
  if (ctxMenu.classList.contains('hidden')) return;
  const buttons = Array.from(ctxMenu.querySelectorAll('button:not(:disabled)'));
  const at = buttons.indexOf(document.activeElement);
  if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); closeCtx(); }
  else if (ev.key === 'ArrowDown' && buttons.length) { ev.preventDefault(); buttons[(at + 1) % buttons.length].focus(); }
  else if (ev.key === 'ArrowUp' && buttons.length) { ev.preventDefault(); buttons[(at - 1 + buttons.length) % buttons.length].focus(); }
}, true);


/* ------------------ auto-play the next playlist item ------------------ */
const prewarmed = new Set();      // next items already asked to prepare
const advancedFrom = new Set();   // finished sessions that already advanced
let advancing = false;

function nextAfter(id) {
  const at = items.findIndex((x) => x.id === id);
  if (at < 0) return null;
  // Play what is in the list, then stop: never wrap around to the first item.
  return at + 1 < items.length ? items[at + 1] : null;
}

async function prewarmNext(currentId) {
  const next = nextAfter(currentId);
  if (!next || next.id === currentId || prewarmed.has(next.id) || !deviceId) return;
  prewarmed.add(next.id);
  try {
    const r = await post('/api/cast', { device: deviceId, id: next.id, prepare_only: true });
    if (r.preparing) setTimeout(() => prewarmed.delete(next.id), 20 * 60 * 1000); // allow a retry later
  } catch { prewarmed.delete(next.id); }
}

async function watchPlaylistAdvance(p, st, total) {
  if (!$('#sAutoPlay').checked || advancing || p.via !== 'cast') return;
  const playingId = STATE.now && STATE.now.item_id;
  if (!playingId) return;
  // Warm the next episode while this one is still playing (last 3 minutes).
  if (st === 'PLAYING' && total > 0 && total - p.elapsed < 180) prewarmNext(playingId);
  if (st !== 'IDLE' || p.idle_reason !== 'FINISHED') return;   // Stop/interrupt/error must not advance
  const key = `${playingId}:${p.media_session_id}`;
  if (advancedFrom.has(key)) return;
  advancedFrom.add(key);
  const next = nextAfter(playingId);
  if (!next) { toast('เล่นจบทั้ง Playlist แล้ว'); return; }
  advancing = true;
  try {
    toast(`เล่นต่อ: ${next.title || next.path}`, 3500);
    select(next.id);
    await castItem(next.id);
  } finally { advancing = false; }
}
