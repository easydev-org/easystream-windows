// easystream-webui — est:// client: WebCodecs video (zoom/pan), pointer +
// touch input, and a floating virtual keyboard. est-only (no a11y queries).

const T = { HELLO: 0x01, VIDEO: 0x02,
  MOVE: 0x10, BUTTON: 0x11, SCROLL: 0x12, KEY: 0x13, TEXT: 0x14 };

const $ = (id) => document.getElementById(id);
const cv = $('screen');
const ctx = cv.getContext('2d');
const kbd = $('kbd');
const bar = $('bar');
const S = { ws: null, dec: null, codec: 'avc1.42E01F', w: 0, h: 0, gotKey: false,
  frames: 0, fps: 0, t0: performance.now(), userScale: 1, panX: 0, panY: 0 };

// offscreen holds the latest frame at native size so zoom/pan repaint instantly.
const off = document.createElement('canvas');
const offCtx = off.getContext('2d');

// ── est framing ─────────────────────────────────────────────────────────────
function frame(type, body) {
  const b = body || new Uint8Array(0);
  const out = new Uint8Array(6 + b.length);
  out[0] = type;
  new DataView(out.buffer).setUint32(2, b.length, false);
  out.set(b, 6);
  return out;
}
function send(bytes) { if (S.ws && S.ws.readyState === WebSocket.OPEN) S.ws.send(bytes); }
function sendMove(x, y) { const b = new Uint8Array(4); const d = new DataView(b.buffer);
  d.setUint16(0, x, false); d.setUint16(2, y, false); send(frame(T.MOVE, b)); }
function sendButton(btn, down) { send(frame(T.BUTTON, new Uint8Array([btn, down ? 1 : 0]))); }
// Key: nameLen, name, u32 char, u8 down (matches the daemons; EST.md omits char)
function sendKey(name, ch, down) {
  const nb = new TextEncoder().encode(name || '');
  const b = new Uint8Array(1 + nb.length + 4 + 1);
  b[0] = nb.length; b.set(nb, 1);
  new DataView(b.buffer).setUint32(1 + nb.length, ch >>> 0, true);
  b[1 + nb.length + 4] = down ? 1 : 0;
  send(frame(T.KEY, b));
}
function sendText(s) { send(frame(T.TEXT, new TextEncoder().encode(s))); }

// ── inbound ─────────────────────────────────────────────────────────────────
let rx = new Uint8Array(0);
function onBytes(chunk) {
  if (rx.length) { const m = new Uint8Array(rx.length + chunk.length); m.set(rx); m.set(chunk, rx.length); rx = m; }
  else rx = chunk;
  for (;;) {
    if (rx.length < 6) return;
    const type = rx[0];
    const len = new DataView(rx.buffer, rx.byteOffset).getUint32(2, false);
    if (rx.length < 6 + len) return;
    const body = rx.slice(6, 6 + len);
    rx = rx.slice(6 + len);
    if (type === T.HELLO) {
      if (body[1] === 1) S.codec = 'avc1.42E01F';
      S.w = (body[2] << 8) | body[3]; S.h = (body[4] << 8) | body[5];
      off.width = S.w || 1; off.height = S.h || 1;
      startDecoder(); resize();
    } else if (type === T.VIDEO) {
      decodeVideo(body);
    }
  }
}

// ── video ───────────────────────────────────────────────────────────────────
function startDecoder() {
  if (S.dec) return;
  if (typeof VideoDecoder === 'undefined') { setState('err', 'no WebCodecs (need HTTPS)'); return; }
  S.dec = new VideoDecoder({
    output: (f) => {
      if (off.width !== f.displayWidth || off.height !== f.displayHeight) {
        off.width = f.displayWidth; off.height = f.displayHeight;
        S.w = f.displayWidth; S.h = f.displayHeight; resize();
      }
      offCtx.drawImage(f, 0, 0); f.close(); paint();
      S.frames++;
      const now = performance.now();
      if (now - S.t0 >= 1000) {
        S.fps = S.frames; S.frames = 0; S.t0 = now;
        $('stats').textContent = `${S.w}×${S.h} · ${S.fps} fps`;
      }
    },
    error: (e) => setState('err', String(e)),
  });
  S.dec.configure({ codec: S.codec, optimizeForLatency: true });
  setState('live');
}
function hasIDR(b) {
  let i = 0;
  while (i + 3 < b.length) {
    if (b[i] === 0 && b[i + 1] === 0 && (b[i + 2] === 1 || (b[i + 2] === 0 && b[i + 3] === 1))) {
      const o = b[i + 2] === 1 ? i + 3 : i + 4;
      if (o < b.length && (b[o] & 0x1f) === 5) return true;
      i = o; continue;
    }
    i++;
  }
  return false;
}
function decodeVideo(b) {
  if (!S.dec || S.dec.state !== 'configured') return;
  const key = hasIDR(b);
  if (!key && !S.gotKey) return;
  if (key) S.gotKey = true;
  try { S.dec.decode(new EncodedVideoChunk({ type: key ? 'key' : 'delta',
    timestamp: performance.now() * 1000, data: b })); } catch {}
}

// ── view: fit × userScale + pan, letterboxed ────────────────────────────────
function vp() { return { vw: cv.clientWidth || innerWidth, vh: cv.clientHeight || innerHeight }; }
function drawRect() {
  const { vw, vh } = vp();
  const sw = S.w || off.width || 1, sh = S.h || off.height || 1;
  const fit = Math.min(vw / sw, vh / sh);
  const scale = fit * S.userScale;
  const w = sw * scale, h = sh * scale;
  return { left: (vw - w) / 2 + S.panX, top: (vh - h) / 2 + S.panY, width: w, height: h, fit };
}
function clampPan() {
  const { vw, vh } = vp(); const r = drawRect();
  const maxX = Math.max(0, (r.width - vw) / 2), maxY = Math.max(0, (r.height - vh) / 2);
  S.panX = Math.max(-maxX, Math.min(maxX, S.panX));
  S.panY = Math.max(-maxY, Math.min(maxY, S.panY));
}
function resize() {
  const { vw, vh } = vp();
  if (cv.width !== vw || cv.height !== vh) { cv.width = vw; cv.height = vh; }
  clampPan(); paint();
}
function paint() {
  if (!off.width) return;
  const { vw, vh } = vp();
  if (cv.width !== vw || cv.height !== vh) { cv.width = vw; cv.height = vh; }
  // The letterbox area is GREY, so the screen's bounds are obvious against a
  // black guest desktop (noVNC does the same). The screen itself gets a thin
  // border and a drop shadow to read as a distinct surface.
  ctx.fillStyle = '#4a4a4a'; ctx.fillRect(0, 0, vw, vh);
  const r = drawRect();
  ctx.imageSmoothingEnabled = true;
  ctx.save();
  ctx.shadowColor = '#000000aa'; ctx.shadowBlur = 12; ctx.shadowOffsetY = 2;
  ctx.fillStyle = '#000';
  ctx.fillRect(r.left, r.top, r.width, r.height);
  ctx.restore();
  ctx.drawImage(off, r.left, r.top, r.width, r.height);
  ctx.strokeStyle = '#00000088'; ctx.lineWidth = 1;
  ctx.strokeRect(r.left + 0.5, r.top + 0.5, r.width - 1, r.height - 1);
  $('zoomLabel').textContent = Math.round(S.userScale * 100) + '%';
}
function zoomTo(scale, cx, cy) {
  const { vw, vh } = vp();
  const px = cx ?? vw / 2, py = cy ?? vh / 2;
  const before = drawRect();
  const sx = (px - before.left) / before.width, sy = (py - before.top) / before.height;
  S.userScale = Math.max(0.25, Math.min(8, scale));
  const after = drawRect();
  S.panX += px - (after.left + sx * after.width);
  S.panY += py - (after.top + sy * after.height);
  clampPan(); paint();
}
function zoomBy(f, cx, cy) { zoomTo(S.userScale * f, cx, cy); }
function fitView() { S.userScale = 1; S.panX = 0; S.panY = 0; clampPan(); paint(); }
function oneToOne() {
  const { vw, vh } = vp(); const sw = S.w || 1, sh = S.h || 1;
  const fit = Math.min(vw / sw, vh / sh);
  const dpr = window.devicePixelRatio || 1;
  zoomTo(1 / (fit * dpr));
}
$('zIn').onclick = () => zoomBy(1.25);
$('zOut').onclick = () => zoomBy(0.8);
$('zFit').onclick = fitView;
$('zOne').onclick = oneToOne;

// ── input ───────────────────────────────────────────────────────────────────
function toSource(clientX, clientY) {
  const el = cv.getBoundingClientRect();
  const r = drawRect();
  const x = Math.round(((clientX - el.left - r.left) / r.width) * (S.w || 1));
  const y = Math.round(((clientY - el.top - r.top) / r.height) * (S.h || 1));
  return [Math.max(0, Math.min(S.w || 1e9, x)), Math.max(0, Math.min(S.h || 1e9, y))];
}
const cur = document.createElement('div');
cur.style.cssText = 'position:fixed;z-index:5;pointer-events:none;width:16px;height:16px;display:none';
cur.innerHTML = '<svg viewBox="0 0 16 16" width="16" height="16"><path fill="#fff" stroke="#000" d="M1 1l6 14 2-6 6-2z"/></svg>';
document.body.appendChild(cur);
function showCursor(x, y) { cur.style.display = ''; cur.style.left = (x - 2) + 'px'; cur.style.top = (y - 2) + 'px'; }

const pts = new Map();
let pinch = null;
let touch = null; // single-touch mouse emulation

function pDist(a, b) { return Math.hypot(a.x - b.x, a.y - b.y); }
function pMid(a, b) { return { x: (a.x + b.x) / 2, y: (a.y + b.y) / 2 }; }

cv.addEventListener('pointerdown', (e) => {
  try { cv.setPointerCapture(e.pointerId); } catch {}
  pts.set(e.pointerId, { x: e.clientX, y: e.clientY, type: e.pointerType });
  cv.focus({ preventScroll: true });
  if (e.pointerType === 'mouse') {
    const [x, y] = toSource(e.clientX, e.clientY); sendMove(x, y);
    sendButton(e.button === 2 ? 3 : e.button === 1 ? 2 : 1, true);
  } else if (pts.size === 1) {
    const [x, y] = toSource(e.clientX, e.clientY);
    touch = { startX: e.clientX, startY: e.clientY, moved: false, down: false, t0: Date.now() };
    sendMove(x, y);
  } else if (pts.size === 2) {
    if (touch) { if (touch.down) sendButton(1, false); touch = null; }
    const [a, b] = [...pts.values()];
    pinch = { dist: pDist(a, b), mid: pMid(a, b), scale0: S.userScale, panX0: S.panX, panY0: S.panY };
  }
});

cv.addEventListener('pointermove', (e) => {
  if (!pts.has(e.pointerId)) {
    if (e.pointerType === 'mouse') { showCursor(e.clientX, e.clientY);
      const [x, y] = toSource(e.clientX, e.clientY); sendMove(x, y); }
    return;
  }
  pts.set(e.pointerId, { x: e.clientX, y: e.clientY, type: e.pointerType });
  if (pts.size === 2 && pinch) {
    const [a, b] = [...pts.values()];
    const d = pDist(a, b), m = pMid(a, b);
    S.userScale = Math.max(0.25, Math.min(8, pinch.scale0 * (d / pinch.dist)));
    S.panX = pinch.panX0 + (m.x - pinch.mid.x);
    S.panY = pinch.panY0 + (m.y - pinch.mid.y);
    clampPan(); paint();
    return;
  }
  if (e.pointerType === 'mouse') {
    showCursor(e.clientX, e.clientY);
    const [x, y] = toSource(e.clientX, e.clientY); sendMove(x, y);
    if (e.clientY < 8) bar.classList.add('show'); else if (e.clientY > 64) bar.classList.remove('show');
    return;
  }
  if (touch) {
    const dx = e.clientX - touch.startX, dy = e.clientY - touch.startY;
    if (!touch.moved && Math.hypot(dx, dy) > 6) { touch.moved = true; sendButton(1, true); touch.down = true; }
    if (touch.moved) { const [x, y] = toSource(e.clientX, e.clientY); sendMove(x, y); }
  }
});

function endPointer(e) {
  if (!pts.has(e.pointerId)) return;
  pts.delete(e.pointerId);
  if (e.pointerType === 'mouse') {
    sendButton(e.button === 2 ? 3 : e.button === 1 ? 2 : 1, false);
  } else if (pts.size === 0 && touch) {
    if (touch.moved) { if (touch.down) sendButton(1, false); }
    else {
      const [x, y] = toSource(e.clientX, e.clientY); sendMove(x, y);
      const held = Date.now() - touch.t0;
      if (held >= 600) { sendButton(3, true); sendButton(3, false); }
      else { sendButton(1, true); sendButton(1, false); }
    }
    touch = null;
  }
  if (pts.size < 2) pinch = null;
}
cv.addEventListener('pointerup', endPointer);
cv.addEventListener('pointercancel', endPointer);
cv.addEventListener('pointerleave', (e) => { if (e.pointerType === 'mouse') cur.style.display = 'none'; });
cv.addEventListener('contextmenu', (e) => e.preventDefault());
cv.addEventListener('wheel', (e) => {
  e.preventDefault();
  if (e.ctrlKey || e.metaKey) { zoomBy(e.deltaY < 0 ? 1.1 : 0.9, e.clientX, e.clientY); return; }
  const [x, y] = toSource(e.clientX, e.clientY);
  const b = new Uint8Array(8); const d = new DataView(b.buffer);
  d.setUint16(0, x, false); d.setUint16(2, y, false);
  d.setInt16(4, e.deltaX, false); d.setInt16(6, e.deltaY, false);
  send(frame(T.SCROLL, b));
}, { passive: false });

// physical keyboard
const NAMED = { Enter: 'Return', NumpadEnter: 'Return', Escape: 'Escape', Tab: 'Tab',
  Backspace: 'Backspace', Delete: 'Delete', Space: 'Space',
  ArrowUp: 'Up', ArrowDown: 'Down', ArrowLeft: 'Left', ArrowRight: 'Right',
  Home: 'Home', End: 'End', PageUp: 'PageUp', PageDown: 'PageDown', Insert: 'Insert',
  ShiftLeft: 'Shift', ShiftRight: 'Shift', ControlLeft: 'Control', ControlRight: 'Control',
  AltLeft: 'Alt', AltRight: 'Alt', MetaLeft: 'Meta', MetaRight: 'Meta',
  F1: 'F1', F2: 'F2', F3: 'F3', F4: 'F4', F5: 'F5', F6: 'F6', F7: 'F7', F8: 'F8',
  F9: 'F9', F10: 'F10', F11: 'F11', F12: 'F12' };
function typingInField() { const a = document.activeElement; return a && (a.tagName === 'INPUT' || a.tagName === 'TEXTAREA'); }
addEventListener('keydown', (e) => {
  if (typingInField()) return; // let the IME field work
  const named = NAMED[e.code];
  if (named) { e.preventDefault(); sendKey(named, 0, true); return; }
  // A printable key with a modifier held (Ctrl+C, Alt+F, Meta+K, …) is a KEY
  // event carrying the character, NOT a Text event: the daemon presses the
  // named key while the modifier is held (the modifier keydown already fired).
  if (e.key.length === 1) {
    e.preventDefault();
    if (e.ctrlKey || e.altKey || e.metaKey) {
      sendKey(e.key, e.key.codePointAt(0), true);
      sendKey(e.key, e.key.codePointAt(0), false);
    } else {
      sendText(e.key);
    }
  }
});
addEventListener('keyup', (e) => {
  if (typingInField()) return;
  const named = NAMED[e.code];
  if (named) { e.preventDefault(); sendKey(named, 0, false); }
});

// ── virtual keyboard ────────────────────────────────────────────────────────
const mods = { Control: false, Alt: false, Shift: false, Meta: false };
function updateSticky() {
  const on = Object.entries(mods).filter(([, v]) => v).map(([k]) => k);
  $('stickyBadge').textContent = on.length ? 'latched: ' + on.join(' + ') : '';
}
function toggleKbd(show) { kbd.classList.toggle('show', show ?? !kbd.classList.contains('show')); }
$('kbdToggle').onclick = () => toggleKbd();
$('kbdHide').onclick = () => toggleKbd(false);
$('btnKbd').onclick = () => toggleKbd();
$('btnFs').onclick = () => { if (document.fullscreenElement) document.exitFullscreen(); else document.documentElement.requestFullscreen?.(); };
$('btnRetry').onclick = connect;
$('hotzone').addEventListener('mouseenter', () => bar.classList.add('show'));
bar.addEventListener('mouseleave', () => bar.classList.remove('show'));

function buildRow(el, chars) {
  for (const c of chars) {
    const d = document.createElement('div');
    d.className = 'key';
    d.dataset.txt = c;
    d.textContent = c;
    el.appendChild(d);
  }
}
buildRow($('rowDigits'), '1234567890'.split(''));
buildRow($('row1'), 'qwertyuiop'.split(''));
buildRow($('row2'), 'asdfghjkl'.split(''));
buildRow($('row3'), 'zxcvbnm'.split(''));

document.querySelectorAll('#mods .key').forEach((el) => {
  el.onclick = () => { const m = el.dataset.mod; mods[m] = !mods[m]; el.classList.toggle('on', mods[m]); updateSticky(); };
});
function latchedDown() { for (const [m, on] of Object.entries(mods)) if (on) sendKey(m, 0, true); }
function latchedUp() {
  for (const [m, on] of Object.entries(mods)) if (on) { sendKey(m, 0, false); mods[m] = false; }
  document.querySelectorAll('#mods .key').forEach((x) => x.classList.remove('on'));
  updateSticky();
}
function anyLatched() { return Object.values(mods).some(Boolean); }

document.querySelectorAll('#kbd .key[data-key]').forEach((el) => {
  el.onclick = () => {
    const name = el.dataset.key;
    latchedDown(); sendKey(name, 0, true); sendKey(name, 0, false); latchedUp();
  };
});
document.querySelectorAll('#kbd .key[data-txt]').forEach((el) => {
  el.onclick = () => {
    const txt = el.dataset.txt;
    if (anyLatched()) {
      latchedDown(); sendKey('', txt.codePointAt(0), true); sendKey('', txt.codePointAt(0), false); latchedUp();
    } else { sendText(txt); }
  };
});
$('ime').addEventListener('input', (e) => { const v = e.target.value; if (v) { sendText(v); e.target.value = ''; } });

// ── connection ──────────────────────────────────────────────────────────────
function setState(kind, msg) {
  const el = $('state');
  el.className = 'badge ' + (kind === 'live' ? 'live' : kind === 'err' ? 'err' : 'wait');
  el.textContent = kind === 'live' ? 'live' : kind === 'err' ? (msg || 'error') : 'connecting';
}
function connect() {
  close();
  setState('wait');
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  // Resolve /est/ws RELATIVE to the page, so the UI works under any mount
  // prefix (e.g. served at /easystream/ by a forwarder) — no absolute path.
  const base = location.pathname.endsWith('/') ? location.pathname : location.pathname + '/';
  const ws = new WebSocket(`${proto}//${location.host}${base}est/ws`);
  ws.binaryType = 'arraybuffer';
  ws.onmessage = (ev) => { if (typeof ev.data === 'string') { setState('err', ev.data); return; } onBytes(new Uint8Array(ev.data)); };
  ws.onerror = () => setState('err', 'websocket error');
  ws.onclose = () => { if ($('state').textContent === 'live') setState('err', 'closed'); };
  S.ws = ws;
}
function close() {
  if (S.dec) { try { S.dec.close(); } catch {} S.dec = null; }
  if (S.ws) { try { S.ws.close(); } catch {} S.ws = null; }
  S.gotKey = false;
}
addEventListener('resize', resize);
addEventListener('orientationchange', () => setTimeout(resize, 150));
resize();
connect();
