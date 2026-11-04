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

function avatar(t) {
  const box = el('div', 'avatar', initials(threadName(t)));
  box.style.background = `hsl(${hue(t.id)} 55% 48%)`;
  if (t.avatar && t.avatar.startsWith('https://')) {
    const img = new Image();
    img.alt = '';
    img.loading = 'lazy';
    img.referrerPolicy = 'no-referrer';
    img.onload = () => box.replaceChildren(img);
    img.src = t.avatar;
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

// ------------------------------------------------------------- conversation

function messageNode(m, prev, isGroup) {
  const frag = document.createDocumentFragment();
  if (!prev || !sameDay(prev.sent_at, m.sent_at)) frag.appendChild(el('div', 'day', fmtDay(m.sent_at)));
  const bubble = el('div', `msg ${m.outgoing ? 'out' : 'in'}`);
  const newSender = !prev || prev.outgoing || prev.sender_id !== m.sender_id || !sameDay(prev.sent_at, m.sent_at);
  if (isGroup && !m.outgoing && newSender) bubble.appendChild(el('div', 'sender', m.sender_name || 'Thành viên'));
  bubble.appendChild(document.createTextNode(m.text));
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
  es.addEventListener('status', (e) => { track(e); onStatus(JSON.parse(e.data)); });
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
  renderStatus(st.zalo);
  await loadThreads();
  openStream();
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
