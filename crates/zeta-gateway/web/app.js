'use strict';

// zetachat web client. Plain JavaScript, no build step. User content is always
// inserted with textContent, never as HTML.

const API = '/api/v1';
const $ = (id) => document.getElementById(id);

const state = {
  threads: new Map(), // id -> thread
  current: null, // open thread id
  messages: [], // messages of the open thread, oldest first
  hasOlder: false,
  filter: 'all',
  search: '',
  status: null,
  stream: null,
  lastSeq: 0,
  streamOpened: false,
  lastQr: '',
  loginPrompted: false,
  statusLimits: null, // upload limits from the gateway
};

// ------------------------------------------------------------------ helpers

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined && text !== null) node.textContent = text;
  return node;
}

/** Lower-case without Vietnamese diacritics, for search. */
function fold(s) {
  return (s || '')
    .normalize('NFD')
    .replace(/[̀-ͯ]/g, '')
    .replace(/[đĐ]/g, 'd')
    .toLowerCase();
}

function hue(id) {
  let h = 0;
  for (const c of id) h = (h * 31 + c.codePointAt(0)) % 360;
  return h;
}

function threadName(t) {
  if (t.name) return t.name;
  return t.kind === 'group' ? `Nhóm ${t.remote_id}` : `Người dùng ${t.remote_id}`;
}

function initials(name) {
  const words = name.trim().split(/\s+/).filter(Boolean);
  if (!words.length) return '?';
  const first = [...words[0]][0] || '';
  const last = words.length > 1 ? [...words[words.length - 1]][0] : '';
  return (first + last).toUpperCase();
}

function isWebUrl(url) {
  return /^https?:\/\//i.test(url || '');
}

/** Where to load a media file from: Zalo's CDN only through the gateway's cache. */
function mediaSrc(url) {
  if (!url) return '';
  if (url.startsWith('data:image/')) return url; // sample pictures of the demo
  return `${API}/media?u=${encodeURIComponent(url)}`;
}

function avatar(t) {
  const box = el('div', 'avatar', initials(threadName(t)));
  box.style.background = `hsl(${hue(t.id)} 55% 48%)`;
  if (isWebUrl(t.avatar)) {
    const img = new Image();
    img.alt = '';
    img.loading = 'lazy';
    img.onload = () => box.replaceChildren(img);
    img.src = mediaSrc(t.avatar);
  }
  return box;
}

function sameDay(a, b) {
  return new Date(a).toDateString() === new Date(b).toDateString();
}

function fmtClock(ms) {
  return new Date(ms).toLocaleTimeString('vi-VN', { hour: '2-digit', minute: '2-digit' });
}

function fmtListTime(ms) {
  if (!ms) return '';
  const now = new Date();
  if (sameDay(ms, now)) return fmtClock(ms);
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (sameDay(ms, yesterday)) return 'Hôm qua';
  const d = new Date(ms);
  const dm = `${String(d.getDate()).padStart(2, '0')}/${String(d.getMonth() + 1).padStart(2, '0')}`;
  return d.getFullYear() === now.getFullYear() ? dm : `${dm}/${String(d.getFullYear()).slice(-2)}`;
}

function fmtDay(ms) {
  const now = new Date();
  if (sameDay(ms, now)) return 'Hôm nay';
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (sameDay(ms, yesterday)) return 'Hôm qua';
  return new Date(ms).toLocaleDateString('vi-VN', {
    weekday: 'long', day: '2-digit', month: '2-digit', year: 'numeric',
  });
}

let toastTimer = 0;
function toast(text) {
  const box = $('toast');
  box.textContent = text;
  box.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { box.hidden = true; }, 3500);
}

// ---------------------------------------------------------------------- API

class Unauthorized extends Error {}

async function api(path, options = {}) {
  const init = { credentials: 'same-origin', method: options.method || 'GET' };
  if (options.json !== undefined) {
    init.method = options.method || 'POST';
    init.headers = { 'content-type': 'application/json' };
    init.body = JSON.stringify(options.json);
  }
  const res = await fetch(API + path, init);
  if (res.status === 401) throw new Unauthorized('unauthorized');
  const data = await res.json().catch(() => null);
  if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
  return data;
}

function handleError(err, prefix = '') {
  if (err instanceof Unauthorized) {
    showPair('Thiết bị chưa được ghép hoặc token đã bị thu hồi.');
    return;
  }
  console.error(err);
  toast(prefix + (err.message || 'Lỗi không xác định'));
}

// ------------------------------------------------------------------ pairing

function showPair(message) {
  if (state.stream) state.stream.close();
  $('app').hidden = true;
  $('pair').hidden = false;
  const box = $('pair-error');
  box.hidden = !message;
  box.textContent = message || '';
  $('pair-token').focus();
}

$('pair-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const token = $('pair-token').value.trim();
  if (!token) return;
  try {
    await api('/session', { json: { token } });
    $('pair-token').value = '';
    await start();
  } catch (err) {
    showPair(err instanceof Unauthorized ? 'Token không đúng hoặc đã bị thu hồi.' : err.message);
  }
});

// ------------------------------------------------------------------ threads

function visibleThreads() {
  const q = fold(state.search);
  let list = [...state.threads.values()];
  if (state.filter === 'contacts') {
    list.sort((a, b) => threadName(a).localeCompare(threadName(b), 'vi'));
  } else {
    list = list.filter((t) => t.last_message_at > 0);
    if (state.filter === 'unread') list = list.filter((t) => t.unread > 0);
    list.sort((a, b) => b.last_message_at - a.last_message_at);
  }
  if (q) list = list.filter((t) => fold(threadName(t)).includes(q));
  return list;
}

function emptyListText() {
  if (state.search) return 'Không tìm thấy hội thoại';
  if (state.filter === 'unread') return 'Không có tin chưa đọc';
  return 'Chưa có hội thoại';
}

function renderThreads() {
  const ul = $('threads');
  const list = visibleThreads();
  ul.replaceChildren();
  if (!list.length) ul.appendChild(el('li', 'list-empty', emptyListText()));
  for (const t of list) {
    let cls = 'thread';
    if (t.id === state.current) cls += ' active';
    if (t.unread > 0) cls += ' unread';
    const li = el('li', cls);
    const body = el('div', 'body');
    body.append(
      el('div', 'name', threadName(t)),
      el('div', 'preview', t.preview || (t.kind === 'group' ? 'Nhóm' : '')),
    );
    const meta = el('div', 'meta');
    meta.append(el('span', null, fmtListTime(t.last_message_at)));
    if (t.unread > 0) meta.append(el('span', 'badge', t.unread > 99 ? '99+' : String(t.unread)));
    li.append(avatar(t), body, meta);
    li.addEventListener('click', () => openThread(t.id));
    ul.appendChild(li);
  }
  updateTitle();
}

function updateTitle() {
  let total = 0;
  for (const t of state.threads.values()) total += t.unread || 0;
  document.title = total > 0 ? `(${total}) zetachat` : 'zetachat';
}

async function loadThreads() {
  const list = await api('/threads?all=true&limit=1000');
  state.threads = new Map(list.map((t) => [t.id, t]));
  renderThreads();
}

for (const tab of document.querySelectorAll('.tab')) {
  tab.addEventListener('click', () => {
    for (const other of document.querySelectorAll('.tab')) other.classList.toggle('active', other === tab);
    state.filter = tab.dataset.filter;
    renderThreads();
  });
}

$('search').addEventListener('input', (e) => {
  state.search = e.target.value;
  renderThreads();
});

// -------------------------------------------------------------------- media

/** Files over this size are not fetched by the gateway; they open from Zalo. */
const GATEWAY_MAX_FILE = 256 * 1024 * 1024;

function hostOf(url) {
  try {
    return new URL(url).hostname.replace(/^www\./, '');
  } catch {
    return '';
  }
}

function fmtSize(bytes) {
  if (!bytes) return '';
  const units = ['B', 'KB', 'MB', 'GB'];
  let n = bytes;
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i += 1;
  }
  const value = n >= 10 || i === 0 ? String(Math.round(n)) : n.toFixed(1).replace('.', ',');
  return `${value} ${units[i]}`;
}

function fmtDuration(ms) {
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
}

function extOf(name) {
  const m = /\.([a-z0-9]{1,5})$/i.exec(name || '');
  return m ? m[1].toUpperCase() : '';
}

/** Size a picture box to fit 280×320 with the picture's proportions. */
function fitBox(node, width, height) {
  if (width > 0 && height > 0) {
    const scale = Math.min(1, 280 / width, 320 / height);
    node.style.width = `${Math.max(64, Math.round(width * scale))}px`;
    node.style.aspectRatio = `${width} / ${height}`;
  } else {
    node.style.width = '280px';
    node.style.aspectRatio = '4 / 3';
  }
}

/** Shown when media cannot be loaded, with a link to the original on Zalo. */
function mediaFailed(text, md) {
  const box = el('div', 'media-failed', text);
  if (isWebUrl(md.url)) {
    const a = el('a', null, 'Mở bản gốc');
    a.href = md.url;
    a.target = '_blank';
    a.rel = 'noopener noreferrer';
    box.append(' · ', a);
  }
  return box;
}

function imageNode(md) {
  const img = new Image();
  img.className = 'media-img';
  img.alt = md.description || 'Hình ảnh';
  img.loading = 'lazy';
  img.decoding = 'async';
  fitBox(img, md.width, md.height);
  // The thumbnail first; the full picture if it fails.
  const sources = [...new Set([md.thumb, md.url].filter(Boolean))];
  let tried = 0;
  img.addEventListener('error', () => {
    tried += 1;
    if (tried < sources.length) img.src = mediaSrc(sources[tried]);
    else img.replaceWith(mediaFailed('Không tải được hình ảnh', md));
  });
  img.addEventListener('click', () => openViewer(md));
  img.src = mediaSrc(sources[0]);
  return img;
}

function gifNode(md) {
  if (!/\.mp4(\?|$)/i.test(md.url)) return imageNode({ ...md, thumb: '' });
  const video = document.createElement('video');
  video.className = 'media-img';
  video.muted = true;
  video.loop = true;
  video.autoplay = true;
  video.playsInline = true;
  fitBox(video, md.width, md.height);
  video.src = mediaSrc(md.url);
  return video;
}

function videoNode(md) {
  const box = el('div', 'media-video-box');
  fitBox(box, md.width, md.height);
  const video = document.createElement('video');
  video.className = 'media-video';
  video.controls = true;
  video.preload = 'none'; // nothing is downloaded before "play"
  video.playsInline = true;
  if (md.thumb) video.poster = mediaSrc(md.thumb);
  video.addEventListener('error', () => box.replaceWith(mediaFailed('Không phát được video', md)));
  video.src = mediaSrc(md.url);
  box.append(video);
  if (md.duration_ms) {
    const badge = el('span', 'media-badge', fmtDuration(md.duration_ms));
    video.addEventListener('play', () => badge.remove(), { once: true });
    box.append(badge);
  }
  return box;
}

function stickerNode(md) {
  const img = new Image();
  img.className = 'sticker-img';
  img.alt = 'Nhãn dán';
  img.loading = 'lazy';
  img.addEventListener('error', () => img.replaceWith(el('div', 'media-failed', '[Nhãn dán]')));
  img.src = mediaSrc(md.url);
  return img;
}

function audioNode(md) {
  const audio = document.createElement('audio');
  audio.className = 'media-audio';
  audio.controls = true;
  audio.preload = 'none';
  audio.addEventListener('error', () => audio.replaceWith(mediaFailed('Không phát được tin nhắn thoại', md)));
  audio.src = mediaSrc(md.url);
  return audio;
}

function fileNode(md) {
  const card = el('a', 'file-card');
  if (md.size > GATEWAY_MAX_FILE && isWebUrl(md.url)) {
    card.href = md.url;
    card.target = '_blank';
    card.rel = 'noopener noreferrer';
  } else {
    card.href = mediaSrc(md.url);
    card.download = md.name || '';
  }
  const info = el('div', 'file-info');
  const size = fmtSize(md.size);
  info.append(el('div', 'file-name', md.name || 'Tập tin'), el('div', 'file-size', size ? `${size} · Tải về` : 'Tải về'));
  card.append(el('div', 'file-icon', extOf(md.name) || 'TỆP'), info);
  return card;
}

function linkNode(md) {
  if (!isWebUrl(md.url)) return el('div', 'media-failed', md.name || 'Liên kết');
  const card = el('a', 'link-card');
  card.href = md.url;
  card.target = '_blank';
  card.rel = 'noopener noreferrer';
  if (md.thumb) {
    const img = new Image();
    img.alt = '';
    img.loading = 'lazy';
    img.addEventListener('error', () => img.remove());
    img.src = mediaSrc(md.thumb);
    card.append(img);
  }
  const info = el('div', 'link-info');
  info.append(el('div', 'link-title', md.name || md.url));
  if (md.description) info.append(el('div', 'link-desc', md.description));
  info.append(el('div', 'link-host', hostOf(md.url)));
  card.append(info);
  return card;
}

/** Renderer and bubble style per media kind. */
const MEDIA = {
  image: [imageNode, 'media'],
  gif: [gifNode, 'media'],
  video: [videoNode, 'media'],
  sticker: [stickerNode, 'sticker'],
  audio: [audioNode, 'card'],
  file: [fileNode, 'card'],
  link: [linkNode, 'card'],
};

// ----------------------------------------------------------- picture viewer

function openViewer(md) {
  const src = mediaSrc(md.url || md.thumb);
  $('viewer-img').src = src;
  $('viewer-img').alt = md.description || 'Hình ảnh';
  $('viewer-save').href = src;
  $('viewer').hidden = false;
  $('viewer-close').focus();
}

function closeViewer() {
  $('viewer').hidden = true;
  $('viewer-img').removeAttribute('src');
}

$('viewer-close').addEventListener('click', closeViewer);
$('viewer').addEventListener('click', (e) => {
  if (e.target === $('viewer') || e.target === $('viewer-stage')) closeViewer();
});
document.addEventListener('keydown', (e) => {
  if (e.key === 'Escape' && !$('viewer').hidden) closeViewer();
});

// ------------------------------------------------------------- conversation

const URL_PATTERN = /https?:\/\/[^\s<>"']+/gi;

/** Append text to `parent`, turning web addresses into links. */
function appendText(parent, text) {
  let last = 0;
  for (const match of text.matchAll(URL_PATTERN)) {
    const url = match[0].replace(/[).,!?;:\]]+$/, '');
    if (match.index > last) parent.append(text.slice(last, match.index));
    const a = el('a', null, url);
    a.href = url;
    a.target = '_blank';
    a.rel = 'noopener noreferrer';
    parent.append(a);
    last = match.index + url.length;
  }
  if (last < text.length) parent.append(text.slice(last));
}

function messageNode(m, prev, isGroup) {
  const frag = document.createDocumentFragment();
  if (!prev || !sameDay(prev.sent_at, m.sent_at)) frag.appendChild(el('div', 'day', fmtDay(m.sent_at)));
  const recalled = m.kind === 'chat.undo';
  const md = recalled ? null : m.media;
  const media = md && MEDIA[md.kind];
  let cls = `msg ${m.outgoing ? 'out' : 'in'}`;
  if (media) cls += ` ${media[1]}`;
  if (recalled) cls += ' recalled';
  const bubble = el('div', cls);
  bubble.dataset.seq = String(m.seq);
  const newSender = !prev || prev.outgoing || prev.sender_id !== m.sender_id || !sameDay(prev.sent_at, m.sent_at);
  if (isGroup && !m.outgoing && newSender) bubble.appendChild(el('div', 'sender', m.sender_name || 'Thành viên'));
  if (m.quote && !recalled) {
    const quote = el('div', 'quote');
    if (m.quote.sender_name) quote.appendChild(el('div', 'quote-name', m.quote.sender_name));
    quote.appendChild(el('div', 'quote-text', m.quote.text || ''));
    bubble.appendChild(quote);
  }
  if (recalled) {
    bubble.appendChild(document.createTextNode(m.text));
  } else if (media) {
    bubble.appendChild(media[0](md));
    if ((md.kind === 'image' || md.kind === 'video') && md.description) {
      const caption = el('div', 'caption');
      appendText(caption, md.description);
      bubble.appendChild(caption);
    }
  } else {
    appendText(bubble, m.text || '');
  }
  bubble.appendChild(el('span', 'time', fmtClock(m.sent_at)));
  frag.appendChild(bubble);
  return frag;
}

function currentIsGroup() {
  const t = state.threads.get(state.current);
  return Boolean(t && t.kind === 'group');
}

function renderMessages(scrollToEnd) {
  const box = $('messages');
  const fromBottom = box.scrollHeight - box.scrollTop;
  box.replaceChildren();
  if (state.hasOlder) {
    const more = el('button', 'btn secondary older', 'Xem tin cũ hơn');
    more.type = 'button';
    more.addEventListener('click', () => loadOlder().catch(handleError));
    box.appendChild(more);
  }
  if (!state.messages.length) box.appendChild(el('div', 'empty', 'Chưa có tin nhắn nào trong hội thoại này.'));
  let prev = null;
  for (const m of state.messages) {
    box.appendChild(messageNode(m, prev, currentIsGroup()));
    prev = m;
  }
  box.scrollTop = scrollToEnd ? box.scrollHeight : box.scrollHeight - fromBottom;
}

async function fetchMessages(id, before) {
  const query = before ? `&before=${before}` : '';
  return api(`/threads/${encodeURIComponent(id)}/messages?limit=50${query}`);
}

async function openThread(id) {
  const t = state.threads.get(id);
  if (!t) return;
  askNotificationPermission();
  state.current = id;
  state.messages = [];
  state.hasOlder = false;
  $('app').classList.add('show-chat');
  $('empty').hidden = true;
  $('conversation').hidden = false;
  const head = avatar(t);
  head.id = 'chat-avatar';
  $('chat-avatar').replaceWith(head);
  $('chat-name').textContent = threadName(t);
  $('chat-sub').textContent = t.kind === 'group' ? 'Nhóm' : '';
  $('messages').replaceChildren();
  renderThreads();
  renderUploads();
  try {
    const list = await fetchMessages(id);
    if (state.current !== id) return;
    state.messages = list;
    state.hasOlder = list.length === 50;
    renderMessages(true);
  } catch (err) {
    handleError(err);
  }
  markRead(id);
  $('input').focus();
}

async function loadOlder() {
  const id = state.current;
  if (!id || !state.messages.length) return;
  const list = await fetchMessages(id, state.messages[0].seq);
  if (state.current !== id) return;
  state.messages = list.concat(state.messages);
  state.hasOlder = list.length === 50;
  renderMessages(false);
}

function appendMessage(m) {
  if (state.messages.some((x) => x.seq === m.seq)) return;
  const box = $('messages');
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 120;
  const prev = state.messages[state.messages.length - 1] || null;
  state.messages.push(m);
  if (state.messages.length === 1) {
    renderMessages(true);
    return;
  }
  box.appendChild(messageNode(m, prev, currentIsGroup()));
  if (nearBottom || m.outgoing) box.scrollTop = box.scrollHeight;
}

async function markRead(id) {
  const t = state.threads.get(id);
  if (!t || !t.unread) return;
  t.unread = 0;
  renderThreads();
  try {
    await api(`/threads/${encodeURIComponent(id)}/read`, { method: 'POST' });
  } catch (err) {
    if (err instanceof Unauthorized) handleError(err);
  }
}

$('back').addEventListener('click', () => {
  $('app').classList.remove('show-chat');
  state.current = null;
  renderThreads();
});

// ----------------------------------------------------------------- composer

const input = $('input');

function autosize() {
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  // A scrollbar only once the text is taller than the box can grow.
  input.style.overflowY = input.scrollHeight > 160 ? 'auto' : 'hidden';
}

input.addEventListener('input', autosize);
input.addEventListener('keydown', (e) => {
  // Enter sends; Shift+Enter is a new line; never send while an IME is composing.
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
    e.preventDefault();
    $('composer').requestSubmit();
  }
});

$('composer').addEventListener('submit', async (e) => {
  e.preventDefault();
  const text = input.value.trim();
  const id = state.current;
  if (!text || !id) return;
  input.value = '';
  autosize();
  $('send').disabled = true;
  try {
    const message = await api(`/threads/${encodeURIComponent(id)}/messages`, { json: { text } });
    onMessage({ message, thread: null });
  } catch (err) {
    input.value = text;
    autosize();
    handleError(err, 'Không gửi được: ');
  } finally {
    $('send').disabled = false;
    input.focus();
  }
});

// -------------------------------------------------------------- attachments
//
// A file goes to the gateway first (progress from the browser), which answers
// with an upload; the gateway then sends it to Zalo in parts and reports on it
// with `upload` events. Files go to the gateway one at a time.

/** Zalo's rules until the gateway tells its own (`/status` → `limits`). */
const DEFAULT_LIMITS = {
  max_file_bytes: 1024 * 1024 * 1024,
  max_photo_bytes: 100 * 1024 * 1024,
  blocked_extensions: ['exe', 'cmd', 'bat', 'com', 'lnk', 'vbs', 'msi', 'vb', 'ws', 'wsf', 'scf', 'scr', 'pif', 'chm'],
};

/** Uploads of this page, by key; `id` is the gateway's once it has the file. */
const uploads = new Map();
/** Latest state of each upload the gateway reported, by id. */
const serverUploads = new Map();
/** Files waiting for their turn to go to the gateway. */
const uploadQueue = [];
let uploadKeys = 0;
let uploadPumping = false;

const UPLOAD_RANK = { queued: 0, sending: 1, finishing: 2, done: 3, failed: 3, cancelled: 3 };

function limits() {
  return (state.statusLimits && state.statusLimits.max_file_bytes) ? state.statusLimits : DEFAULT_LIMITS;
}

/** Keep the most advanced of two reports on the same upload. */
function newerUpload(a, b) {
  if (!a) return b;
  if (!b) return a;
  const ra = UPLOAD_RANK[a.state] ?? 0;
  const rb = UPLOAD_RANK[b.state] ?? 0;
  if (ra !== rb) return ra > rb ? a : b;
  return (b.sent || 0) >= (a.sent || 0) ? b : a;
}

/** Width and height of a picture; zeros when the browser cannot read it. */
async function pictureSize(file) {
  if (!file.type.startsWith('image/') || typeof createImageBitmap !== 'function') return [0, 0];
  try {
    const bitmap = await createImageBitmap(file);
    const size = [bitmap.width, bitmap.height];
    bitmap.close();
    return size;
  } catch {
    return [0, 0];
  }
}

/** POST a file to the gateway; resolves with its answer (`{ upload }`). */
function postFile(entry, query) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    entry.xhr = xhr;
    xhr.open('POST', `${API}/threads/${encodeURIComponent(entry.thread_id)}/attachments?${query}`);
    xhr.setRequestHeader('content-type', entry.file.type || 'application/octet-stream');
    xhr.responseType = 'json';
    xhr.upload.onprogress = (e) => {
      entry.loaded = e.loaded;
      scheduleUploads();
    };
    xhr.onload = () => {
      entry.xhr = null;
      if (xhr.status === 401) reject(new Unauthorized('unauthorized'));
      else if (xhr.status >= 200 && xhr.status < 300) resolve(xhr.response || {});
      else reject(new Error((xhr.response && xhr.response.error) || `HTTP ${xhr.status}`));
    };
    xhr.onerror = () => {
      entry.xhr = null;
      reject(new Error('mất kết nối tới gateway'));
    };
    xhr.onabort = () => {
      entry.xhr = null;
      reject(new DOMException('cancelled', 'AbortError'));
    };
    xhr.send(entry.file);
  });
}

/** Send queued files to the gateway, one at a time. */
async function pumpUploads() {
  if (uploadPumping) return;
  uploadPumping = true;
  try {
    while (uploadQueue.length) {
      const entry = uploadQueue.shift();
      if (!uploads.has(entry.key)) continue; // cancelled while waiting
      entry.phase = 'local';
      scheduleUploads();
      const [width, height] = await pictureSize(entry.file);
      const query = new URLSearchParams({ name: entry.name, width, height });
      if (entry.asFile) query.set('as', 'file');
      try {
        const data = await postFile(entry, query);
        const u = data.upload;
        if (!u) throw new Error('gateway trả lời thiếu thông tin tệp');
        entry.id = u.id;
        entry.phase = 'server';
        serverUploads.set(u.id, newerUpload(serverUploads.get(u.id), u));
        settleUpload(entry);
      } catch (err) {
        if (err.name === 'AbortError') {
          uploads.delete(entry.key);
        } else if (err instanceof Unauthorized) {
          uploads.delete(entry.key);
          handleError(err);
        } else {
          entry.phase = 'server';
          entry.localError = err.message || 'lỗi không rõ';
        }
      }
      scheduleUploads();
    }
  } finally {
    uploadPumping = false;
  }
}

/** Drop an upload that ended well; its message comes as a `message` event too. */
function settleUpload(entry) {
  const u = entry.id && serverUploads.get(entry.id);
  if (!u) return;
  if (u.state === 'done' || u.state === 'cancelled') {
    uploads.delete(entry.key);
    serverUploads.delete(entry.id);
    if (u.message) onMessage({ message: u.message, thread: null });
  }
}

/** Queue files for the conversation open when they were picked. */
function sendFiles(files, asFile = false) {
  const thread = state.current;
  if (!thread) return;
  const rules = limits();
  for (const file of files) {
    const ext = (file.type.split('/')[1] || 'bin').replace('jpeg', 'jpg');
    const name = file.name || `zetachat-${Date.now()}.${ext}`;
    const blocked = (/\.([^.]+)$/.exec(name) || [])[1];
    if (blocked && rules.blocked_extensions.includes(blocked.toLowerCase())) {
      toast(`Zalo không cho gửi tệp .${blocked.toLowerCase()}, hãy nén thành .zip rồi gửi`);
      continue;
    }
    if (!file.size) {
      toast(`${name}: tệp rỗng`);
      continue;
    }
    if (file.size > rules.max_file_bytes) {
      toast(`${name}: lớn hơn ${fmtSize(rules.max_file_bytes)}, Zalo không nhận`);
      continue;
    }
    uploadKeys += 1;
    const entry = {
      key: `local-${uploadKeys}`,
      id: '',
      thread_id: thread,
      name,
      size: file.size,
      file,
      asFile,
      phase: 'waiting',
      loaded: 0,
      xhr: null,
      localError: '',
    };
    uploads.set(entry.key, entry);
    uploadQueue.push(entry);
  }
  scheduleUploads();
  pumpUploads();
}

/** What to show for an upload: a line of text and the bar's fraction (or null). */
function uploadStatus(entry) {
  const total = fmtSize(entry.size);
  const pct = (done) => `${Math.floor((100 * done) / Math.max(1, entry.size))}%`;
  if (entry.localError) return [`Không gửi được: ${entry.localError}`, null];
  if (entry.phase === 'waiting') return [`Đang chờ · ${total}`, null];
  if (entry.phase === 'local') {
    return [`Đang tải lên ${pct(entry.loaded)} · ${fmtSize(entry.loaded) || '0 B'} / ${total}`, entry.loaded / entry.size];
  }
  const u = serverUploads.get(entry.id) || { state: 'queued', sent: 0 };
  switch (u.state) {
    case 'queued':
      return [`Chờ gửi lên Zalo · ${total}`, 0];
    case 'sending':
      if (u.sent >= entry.size) return ['Zalo đang xử lý tệp…', 1];
      return [`Đang gửi lên Zalo ${pct(u.sent)} · ${fmtSize(u.sent) || '0 B'} / ${total}`, u.sent / entry.size];
    case 'finishing':
      return ['Đang gửi tin…', 1];
    case 'failed':
      return [`Không gửi được: ${u.error || 'lỗi không rõ'}`, null];
    default:
      return ['', null];
  }
}

function uploadFailed(entry) {
  if (entry.localError) return true;
  const u = entry.phase === 'server' && serverUploads.get(entry.id);
  return Boolean(u && u.state === 'failed');
}

let uploadsFrame = 0;
function scheduleUploads() {
  if (!uploadsFrame) uploadsFrame = requestAnimationFrame(renderUploads);
}

function renderUploads() {
  uploadsFrame = 0;
  const box = $('uploads');
  const list = [...uploads.values()].filter((entry) => entry.thread_id === state.current);
  box.hidden = !list.length;
  box.replaceChildren();
  for (const entry of list) {
    const failed = uploadFailed(entry);
    const row = el('div', failed ? 'upload failed' : 'upload');
    const [text, fraction] = uploadStatus(entry);
    const info = el('div', 'upload-info');
    info.append(el('div', 'file-name', entry.name), el('div', 'upload-status', text));
    if (fraction !== null) {
      const bar = el('div', 'upload-bar');
      const fill = el('div', 'upload-fill');
      fill.style.width = `${Math.min(100, Math.round(fraction * 100))}%`;
      bar.append(fill);
      info.append(bar);
    }
    row.append(el('div', 'file-icon', extOf(entry.name) || 'TỆP'), info);
    const u = serverUploads.get(entry.id);
    if (!(u && u.state === 'finishing' && !failed)) {
      const close = el('button', 'upload-cancel', '×');
      close.type = 'button';
      close.title = failed ? 'Đóng' : 'Hủy gửi';
      close.setAttribute('aria-label', close.title);
      close.addEventListener('click', () => cancelUpload(entry));
      row.append(close);
    }
    box.append(row);
  }
}

async function cancelUpload(entry) {
  if (uploadFailed(entry) || entry.phase === 'waiting') {
    uploads.delete(entry.key);
    scheduleUploads();
    return;
  }
  if (entry.xhr) {
    entry.xhr.abort();
    return;
  }
  if (!entry.id) return;
  try {
    const data = await api(`/uploads/${encodeURIComponent(entry.id)}`, { method: 'DELETE' });
    if (data && data.upload) serverUploads.set(entry.id, data.upload);
    settleUpload(entry);
  } catch (err) {
    handleError(err, 'Không hủy được: ');
  }
  scheduleUploads();
}

/** An `upload` event from the gateway. */
function onUpload(payload) {
  const u = payload && payload.upload;
  if (!u) return;
  serverUploads.set(u.id, newerUpload(serverUploads.get(u.id), u));
  for (const entry of uploads.values()) {
    if (entry.id === u.id) settleUpload(entry);
  }
  scheduleUploads();
}

/** Uploads the gateway has (after a reload or a reconnection). */
async function loadUploads() {
  let list;
  try {
    list = await api('/uploads');
  } catch {
    return;
  }
  const known = new Set(list.map((u) => u.id));
  for (const u of list) {
    serverUploads.set(u.id, newerUpload(serverUploads.get(u.id), u));
    const tracked = [...uploads.values()].some((entry) => entry.id === u.id);
    if (!tracked && (UPLOAD_RANK[u.state] ?? 3) < 3) {
      uploads.set(u.id, {
        key: u.id, id: u.id, thread_id: u.thread_id, name: u.name, size: u.size,
        phase: 'server', loaded: u.size, xhr: null, localError: '',
      });
    }
  }
  for (const entry of uploads.values()) {
    if (entry.phase !== 'server' || !entry.id || entry.localError) continue;
    if (!known.has(entry.id)) {
      const u = serverUploads.get(entry.id);
      // The gateway forgot it: it restarted before the file reached Zalo.
      if (!u || UPLOAD_RANK[u.state] < 3) entry.localError = 'gateway đã khởi động lại, hãy gửi lại tệp này';
    }
    settleUpload(entry);
  }
  scheduleUploads();
}

window.addEventListener('beforeunload', (e) => {
  // Files still going to the gateway are lost with the page; those it already
  // has keep going to Zalo.
  if ([...uploads.values()].some((entry) => entry.phase === 'local' || entry.phase === 'waiting')) {
    e.preventDefault();
    e.returnValue = '';
  }
});

$('attach-photo').addEventListener('click', () => $('photo-input').click());
$('photo-input').addEventListener('change', (e) => {
  const files = [...e.target.files];
  e.target.value = '';
  sendFiles(files);
});
$('attach').addEventListener('click', () => $('file-input').click());
$('file-input').addEventListener('change', (e) => {
  const files = [...e.target.files];
  e.target.value = '';
  sendFiles(files, true);
});

// Pasting a screenshot sends it.
input.addEventListener('paste', (e) => {
  const files = [...((e.clipboardData && e.clipboardData.files) || [])];
  if (!files.length) return;
  e.preventDefault();
  sendFiles(files);
});

// Dropping files on the conversation sends them.
const dropZone = $('conversation');
let dragDepth = 0;
const carriesFiles = (e) => [...((e.dataTransfer && e.dataTransfer.types) || [])].includes('Files');
dropZone.addEventListener('dragenter', (e) => {
  if (!carriesFiles(e)) return;
  dragDepth += 1;
  dropZone.classList.add('dropping');
});
dropZone.addEventListener('dragleave', () => {
  dragDepth = Math.max(0, dragDepth - 1);
  if (!dragDepth) dropZone.classList.remove('dropping');
});
dropZone.addEventListener('dragover', (e) => {
  if (carriesFiles(e)) e.preventDefault();
});
dropZone.addEventListener('drop', (e) => {
  if (!carriesFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  dropZone.classList.remove('dropping');
  sendFiles([...e.dataTransfer.files]);
});

// ------------------------------------------------------------ notifications

function askNotificationPermission() {
  if ('Notification' in window && Notification.permission === 'default') {
    Notification.requestPermission().catch(() => {});
  }
}

function notify(t, m) {
  if (!('Notification' in window) || Notification.permission !== 'granted') return;
  const title = t.kind === 'group' ? `${threadName(t)} · ${m.sender_name || 'Thành viên'}` : threadName(t);
  const n = new Notification(title, { body: m.text, tag: m.thread_id, icon: '/icon.svg' });
  n.onclick = () => {
    window.focus();
    openThread(m.thread_id);
    n.close();
  };
}

// ------------------------------------------------------------------- events

function threadFromMessage(m) {
  const [, code, remote] = m.thread_id.split(':');
  return {
    id: m.thread_id,
    kind: code === 'g' ? 'group' : 'user',
    remote_id: remote || '',
    name: m.outgoing || code === 'g' ? '' : m.sender_name,
    avatar: '',
    last_message_at: 0,
    unread: 0,
    preview: '',
  };
}

function onMessage(payload) {
  const m = payload.message;
  if (!m) return;
  let t = state.threads.get(m.thread_id);
  if (payload.thread) t = Object.assign(t || {}, payload.thread);
  if (!t) t = threadFromMessage(m);
  state.threads.set(t.id, t);
  t.last_message_at = Math.max(t.last_message_at || 0, m.sent_at);
  t.preview = m.text;
  const isOpen = m.thread_id === state.current;
  const seen = isOpen && document.visibilityState === 'visible';
  if (isOpen) appendMessage(m);
  if (seen) {
    if (t.unread) markRead(t.id);
  } else if (!m.outgoing) {
    notify(t, m);
  }
  renderThreads();
}

/** A stored message changed (recalled by its sender). */
function onMessageUpdated(payload) {
  const m = payload.message;
  if (!m) return;
  if (payload.thread) {
    const t = state.threads.get(m.thread_id);
    state.threads.set(m.thread_id, Object.assign(t || {}, payload.thread));
    renderThreads();
  }
  if (m.thread_id !== state.current) return;
  const i = state.messages.findIndex((x) => x.seq === m.seq);
  if (i < 0) return;
  state.messages[i] = m;
  renderMessages(false);
}

function onStatus(s) {
  const wasValid = state.status && state.status.session === 'valid';
  renderStatus(s);
  if (!wasValid && s.session === 'valid') loadThreads().catch(handleError);
  if ((s.session === 'logged_out' || s.session === 'rejected') && !state.loginPrompted) {
    state.loginPrompted = true;
    openLogin();
  }
}

function track(e) {
  const seq = Number(e.lastEventId);
  if (seq > state.lastSeq) state.lastSeq = seq;
}

async function resync() {
  loadUploads();
  try {
    await loadThreads();
    const id = state.current;
    if (!id) return;
    const list = await fetchMessages(id);
    if (state.current !== id) return;
    state.messages = list;
    state.hasOlder = list.length === 50;
    renderMessages(true);
  } catch (err) {
    handleError(err);
  }
}

function openStream() {
  if (state.stream) state.stream.close();
  const es = new EventSource(`${API}/events/stream?since=${state.lastSeq}`);
  state.stream = es;
  es.addEventListener('message', (e) => { track(e); onMessage(JSON.parse(e.data)); });
  es.addEventListener('message_updated', (e) => { track(e); onMessageUpdated(JSON.parse(e.data)); });
  es.addEventListener('status', (e) => { track(e); onStatus(JSON.parse(e.data)); });
  // Upload progress: not in the event log, so no id to track.
  es.addEventListener('upload', (e) => onUpload(JSON.parse(e.data)));
  // History was loaded on the gateway: reload lists instead of replaying messages.
  es.addEventListener('threads_changed', (e) => { track(e); resync(); });
  es.addEventListener('thread_read', (e) => {
    track(e);
    const t = state.threads.get(JSON.parse(e.data).thread_id);
    if (t) {
      t.unread = 0;
      renderThreads();
    }
  });
  es.onopen = () => {
    // After a reconnection, reload lists in case events were pruned meanwhile.
    if (state.streamOpened) resync();
    state.streamOpened = true;
  };
  es.onerror = () => {
    if (es.readyState !== EventSource.CLOSED) return; // the browser retries by itself
    api('/status')
      .then(() => setTimeout(openStream, 3000))
      .catch((err) => (err instanceof Unauthorized ? handleError(err) : setTimeout(openStream, 5000)));
  };
}

document.addEventListener('visibilitychange', () => {
  if (document.visibilityState === 'visible' && state.current) markRead(state.current);
});

// ------------------------------------------------------------- Zalo status

const SESSION_TEXT = {
  starting: 'Zalo: đang khởi động',
  logged_out: 'Zalo: chưa đăng nhập',
  rejected: 'Zalo: phiên hết hạn',
  unknown: 'Zalo: chưa rõ trạng thái',
};

function renderStatus(s) {
  state.status = s;
  const pill = $('zalo-status');
  let cls = 'bad';
  let text = SESSION_TEXT[s.session] || `Zalo: ${s.session}`;
  if (s.session === 'valid') {
    cls = s.realtime ? 'ok' : 'warn';
    text = s.realtime ? 'Zalo: đã kết nối' : 'Zalo: đang nối lại';
  } else if (s.session === 'starting' || s.session === 'unknown') {
    cls = 'warn';
  }
  pill.className = `status-pill ${cls}`;
  pill.textContent = text;
  pill.title = s.detail || (s.display_name ? `Tài khoản: ${s.display_name}` : '');
  renderLogin();
}

$('zalo-status').addEventListener('click', () => {
  if (state.status && state.status.session !== 'valid') openLogin();
});

// -------------------------------------------------------------- Zalo login

function openLogin() {
  $('login').hidden = false;
  renderLogin();
}

function closeLogin() {
  $('login').hidden = true;
}

function renderLogin() {
  if ($('login').hidden || !state.status) return;
  const login = state.status.login || { state: 'idle' };
  const qr = $('login-qr');
  const text = $('login-text');
  const start = $('login-start');
  if (state.status.session === 'valid' && login.state === 'idle') {
    text.textContent = 'Đã đăng nhập Zalo.';
    qr.hidden = true;
    setTimeout(closeLogin, 800);
    return;
  }
  if (login.state === 'waiting_scan') {
    text.textContent = 'Mở Zalo trên điện thoại, chọn quét mã QR rồi quét mã dưới đây.';
    if (login.qr !== state.lastQr) {
      state.lastQr = login.qr;
      qr.src = `${API}/zalo/login/qr.svg?t=${Date.now()}`;
    }
    qr.hidden = false;
    start.textContent = 'Tạo mã mới';
  } else if (login.state === 'waiting_confirm') {
    text.textContent = 'Đã quét. Bấm “Đăng nhập” trên điện thoại để hoàn tất.';
    qr.hidden = true;
  } else if (login.state === 'failed') {
    text.textContent = `Đăng nhập không thành công: ${login.reason || ''}`;
    qr.hidden = true;
    start.textContent = 'Thử lại';
  } else {
    text.textContent = 'Gateway chưa có phiên Zalo. Tạo mã QR rồi quét bằng app Zalo trên điện thoại.';
    qr.hidden = true;
    start.textContent = 'Tạo mã QR';
  }
}

$('login-close').addEventListener('click', closeLogin);
$('login-start').addEventListener('click', async () => {
  const button = $('login-start');
  button.disabled = true;
  try {
    const login = await api('/zalo/login', { method: 'POST' });
    state.status = { ...(state.status || {}), login };
    renderLogin();
  } catch (err) {
    handleError(err);
  } finally {
    button.disabled = false;
  }
});

// ------------------------------------------------------------------ startup

async function start() {
  const st = await api('/status');
  $('pair').hidden = true;
  $('app').hidden = false;
  state.lastSeq = st.last_event_seq || 0;
  state.statusLimits = st.limits || null;
  renderStatus(st.zalo);
  await loadThreads();
  openStream();
  loadUploads();
  if (st.zalo.session === 'logged_out' || st.zalo.session === 'rejected') {
    state.loginPrompted = true;
    openLogin();
  }
}

async function boot() {
  // Links carry `token` (pairing) and `thread` in the fragment, which never
  // reaches a server or a proxy log.
  const params = new URLSearchParams(location.hash.slice(1));
  const token = params.get('token');
  const openId = params.get('thread');
  if (location.hash) history.replaceState(null, '', location.pathname + location.search);
  if (token) {
    try {
      await api('/session', { json: { token } });
    } catch (err) {
      showPair(err instanceof Unauthorized ? 'Link ghép thiết bị không hợp lệ.' : err.message);
      return;
    }
  }
  try {
    await start();
    if (openId && state.threads.has(openId)) await openThread(openId);
  } catch (err) {
    showPair(err instanceof Unauthorized ? '' : `Không kết nối được gateway: ${err.message}`);
  }
  if ('serviceWorker' in navigator && window.isSecureContext) {
    navigator.serviceWorker.register('/sw.js').catch(() => {});
  }
}

boot();
