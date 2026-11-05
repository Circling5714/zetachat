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
  typing: new Map(), // thread id -> { name, timer }: someone is typing there
  replyTo: null, // the message the next one answers
  typingSent: new Map(), // thread id -> when typing was last told to the gateway
  people: new Map(), // user id -> { uid, name, avatar, friend }: friends, group members
  friends: [], // user ids of the friend list
  hits: null, // messages the search box found; null while searching
  meKey: '', // what the account's picture shows, to redraw it only on change
  requests: [], // friend requests received and not answered
  mentions: [], // people named in the message being written: { uid, label }
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
  if (t.alias) return t.alias;
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

/**
 * A round picture over initials on a colour picked from `key`. The picture is
 * in the page from the start: a lazy image outside the page never loads.
 */
function avatarBox(key, name, url) {
  const box = el('div', 'avatar', initials(name || '?'));
  box.style.background = `hsl(${hue(key || name || '?')} 55% 48%)`;
  if (isWebUrl(url) || String(url || '').startsWith('data:image/')) {
    const img = new Image();
    img.alt = '';
    img.loading = 'lazy';
    img.decoding = 'async';
    img.addEventListener('load', () => box.classList.add('pictured'));
    img.addEventListener('error', () => img.remove());
    img.src = mediaSrc(url);
    box.append(img);
    box.dataset.src = url;
  }
  return box;
}

function avatar(t) {
  return avatarBox(t.id, threadName(t), t.avatar);
}

function personName(p) {
  return (p && (p.alias || p.name || p.zalo_name)) || '';
}

/** Who sent a group message: the account's name for them first. */
function senderLabel(m) {
  const p = state.people.get(m.sender_id);
  return (p && p.alias) || m.sender_name || personName(p);
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

function membersText(t) {
  return t.members > 0 ? `${t.members} thành viên` : 'Nhóm';
}

/** A conversation in a list; `sub` replaces the last message under its name. */
function threadRow(t, sub) {
  let cls = 'thread';
  if (t.id === state.current) cls += ' active';
  if (t.unread > 0) cls += ' unread';
  const li = el('li', cls);
  const body = el('div', 'body');
  const typing = state.typing.get(t.id);
  body.append(
    el('div', 'name', threadName(t)),
    typing
      ? el('div', 'preview typing', typingText(t, typing))
      : el('div', 'preview', sub || displayText(t.preview) || (t.kind === 'group' ? membersText(t) : '')),
  );
  const meta = el('div', 'meta');
  meta.append(el('span', null, fmtListTime(t.last_message_at)));
  if (t.unread > 0) meta.append(el('span', 'badge', t.unread > 99 ? '99+' : String(t.unread)));
  li.append(avatar(t), body, meta);
  li.addEventListener('click', () => openThread(t.id));
  return li;
}

/** A user in a list (friend, search result); opens the chat with them. */
function personRow(p) {
  const id = `zalo:u:${p.uid}`;
  const li = el('li', id === state.current ? 'thread active' : 'thread');
  const body = el('div', 'body');
  body.append(el('div', 'name', personName(p) || `Người dùng ${p.uid}`));
  li.append(avatarBox(id, personName(p), p.avatar), body, el('div', 'meta'));
  li.addEventListener('click', () => openPerson(p));
  return li;
}

function friendList() {
  return state.friends
    .map((uid) => state.people.get(uid))
    .filter(Boolean)
    .sort((a, b) => personName(a).localeCompare(personName(b), 'vi'));
}

function groupList() {
  return [...state.threads.values()]
    .filter((t) => t.kind === 'group')
    .sort((a, b) => b.last_message_at - a.last_message_at || threadName(a).localeCompare(threadName(b), 'vi'));
}

function renderConversations(ul) {
  let list = [...state.threads.values()].filter((t) => t.last_message_at > 0);
  if (state.filter === 'unread') list = list.filter((t) => t.unread > 0);
  list.sort((a, b) => b.last_message_at - a.last_message_at);
  if (!list.length) {
    ul.appendChild(el('li', 'list-empty', state.filter === 'unread' ? 'Không có tin chưa đọc' : 'Chưa có hội thoại'));
  }
  for (const t of list) ul.appendChild(threadRow(t));
}

/** A friend request in the list: who, their words, and the two answers. */
function requestRow(r) {
  const name = r.display_name || r.zalo_name || `Người dùng ${r.uid}`;
  const li = el('li', 'thread request');
  const body = el('div', 'body');
  body.append(el('div', 'name', name));
  if (r.message) body.append(el('div', 'preview', r.message));
  const answers = el('div', 'request-actions');
  const yes = el('button', 'btn small-btn', 'Đồng ý');
  const no = el('button', 'btn secondary small-btn', 'Từ chối');
  for (const [button, accept] of [[yes, true], [no, false]]) {
    button.type = 'button';
    button.addEventListener('click', (e) => {
      e.stopPropagation();
      answerRequest(r.uid, accept);
    });
    answers.append(button);
  }
  body.append(answers);
  const meta = el('div', 'meta');
  if (r.time) meta.append(el('span', null, fmtListTime(r.time)));
  li.append(avatarBox(`zalo:u:${r.uid}`, name, r.avatar), body, meta);
  li.addEventListener('click', () => openPerson({ uid: r.uid, name, avatar: r.avatar }));
  return li;
}

/** Friend requests first, then the friend list by name, under first letters. */
function renderFriends(ul) {
  if (state.requests.length) {
    ul.appendChild(el('li', 'list-section', `Lời mời kết bạn · ${state.requests.length}`));
    for (const r of state.requests) ul.appendChild(requestRow(r));
  }
  const list = friendList();
  if (!list.length) {
    ul.appendChild(el('li', 'list-empty', 'Chưa có bạn bè'));
    return;
  }
  ul.appendChild(el('li', 'list-section', `${list.length} bạn bè`));
  let letter = '';
  for (const p of list) {
    const first = fold(personName(p)).charAt(0).toUpperCase();
    const l = /[A-Z]/.test(first) ? first : '#';
    if (l !== letter) {
      letter = l;
      ul.appendChild(el('li', 'list-letter', l));
    }
    ul.appendChild(personRow(p));
  }
}

function renderGroups(ul) {
  const list = groupList();
  if (!list.length) {
    ul.appendChild(el('li', 'list-empty', 'Chưa tham gia nhóm nào'));
    return;
  }
  ul.appendChild(el('li', 'list-section', `${list.length} nhóm`));
  for (const t of list) ul.appendChild(threadRow(t, membersText(t)));
}

/** What the search box found: people, groups, then messages (from the gateway). */
function renderSearch(ul) {
  const q = fold(state.search.trim());
  const people = [];
  const shown = new Set();
  for (const p of friendList()) {
    if (fold(personName(p)).includes(q)) {
      people.push(personRow(p));
      shown.add(`zalo:u:${p.uid}`);
    }
  }
  for (const t of state.threads.values()) {
    if (t.kind === 'user' && !shown.has(t.id) && fold(threadName(t)).includes(q)) people.push(threadRow(t));
  }
  const groups = groupList()
    .filter((t) => fold(threadName(t)).includes(q))
    .map((t) => threadRow(t, membersText(t)));
  const LIMIT = 20;
  if (people.length) {
    ul.appendChild(el('li', 'list-section', 'Liên hệ'));
    ul.append(...people.slice(0, LIMIT));
  }
  if (groups.length) {
    ul.appendChild(el('li', 'list-section', 'Nhóm'));
    ul.append(...groups.slice(0, LIMIT));
  }
  if (q.length < 2) {
    if (!people.length && !groups.length) ul.appendChild(el('li', 'list-empty', 'Không tìm thấy kết quả'));
    return;
  }
  ul.appendChild(el('li', 'list-section', 'Tin nhắn'));
  if (state.hits === null) ul.appendChild(el('li', 'list-note', 'Đang tìm…'));
  else if (!state.hits.length) ul.appendChild(el('li', 'list-note', 'Không có tin nhắn nào khớp'));
  else for (const m of state.hits) ul.appendChild(hitRow(m));
}

/** A message the search found; opens its conversation at it. */
function hitRow(m) {
  const t = state.threads.get(m.thread_id) || threadFromMessage(m);
  const li = el('li', 'thread hit');
  const body = el('div', 'body');
  let who = '';
  if (m.outgoing) who = 'Bạn';
  else if (t.kind === 'group') who = senderLabel(m);
  body.append(el('div', 'name', threadName(t)), el('div', 'preview', who ? `${who}: ${m.text}` : m.text));
  const meta = el('div', 'meta');
  meta.append(el('span', null, fmtListTime(m.sent_at)));
  li.append(avatar(t), body, meta);
  li.addEventListener('click', () => {
    if (!state.threads.has(t.id)) state.threads.set(t.id, t);
    openThread(t.id, m.seq);
  });
  return li;
}

function renderThreads() {
  const ul = $('threads');
  ul.replaceChildren();
  if (state.search.trim()) renderSearch(ul);
  else if (state.filter === 'friends') renderFriends(ul);
  else if (state.filter === 'groups') renderGroups(ul);
  else renderConversations(ul);
  updateTitle();
}

/** Open the chat with a user, even one without messages yet. */
function openPerson(p) {
  const id = `zalo:u:${p.uid}`;
  if (!state.threads.has(id)) {
    state.threads.set(id, {
      id, kind: 'user', remote_id: p.uid, name: personName(p), avatar: p.avatar || '',
      last_message_at: 0, unread: 0, preview: '', members: 0,
    });
  }
  openThread(id);
}

async function loadFriends() {
  try {
    const list = await api('/friends');
    for (const p of list) state.people.set(p.uid, { ...(state.people.get(p.uid) || {}), ...p });
    state.friends = list.map((p) => p.uid);
    renderThreads();
  } catch (err) {
    if (err instanceof Unauthorized) handleError(err);
  }
}

async function loadRequests() {
  try {
    state.requests = await api('/friends/requests');
  } catch (err) {
    if (err instanceof Unauthorized) handleError(err);
    return;
  }
  updateRequestsBadge();
  if (state.filter === 'friends' && !state.search.trim()) renderThreads();
}

function updateRequestsBadge() {
  const badge = $('requests-badge');
  const n = state.requests.length;
  badge.hidden = !n;
  badge.textContent = n > 99 ? '99+' : String(n);
}

/** Accept or decline a friend request; true when Zalo took the answer. */
async function answerRequest(uid, accept) {
  try {
    await api(`/friends/requests/${encodeURIComponent(uid)}/${accept ? 'accept' : 'reject'}`, { method: 'POST' });
  } catch (err) {
    handleError(err, 'Không trả lời được lời mời: ');
    return false;
  }
  state.requests = state.requests.filter((r) => r.uid !== uid);
  updateRequestsBadge();
  toast(accept ? 'Đã đồng ý kết bạn' : 'Đã từ chối lời mời kết bạn');
  if (accept) loadFriends();
  renderThreads();
  return true;
}

// ------------------------------------------------------------ renaming
//
// A 1-1 chat gets a "tên gợi nhớ" (kept by Zalo, on all the account's
// devices, seen only by this account); a group gets a new name for everyone.

let renaming = null;

function openRename(id) {
  const t = state.threads.get(id);
  if (!t) return;
  renaming = id;
  const group = t.kind === 'group';
  $('rename-title').textContent = group ? 'Đổi tên nhóm' : 'Đặt tên gợi nhớ';
  $('rename-note').textContent = group
    ? 'Mọi thành viên trong nhóm sẽ thấy tên mới.'
    : `Chỉ bạn thấy tên này, trên mọi thiết bị của tài khoản. Để trống để dùng lại tên ${t.name || 'Zalo của họ'}.`;
  const input = $('rename-input');
  input.value = group ? threadName(t) : t.alias || '';
  input.placeholder = group ? 'Tên nhóm' : t.name || 'Tên gợi nhớ';
  $('rename').hidden = false;
  input.focus();
  input.select();
}

function closeRename() {
  $('rename').hidden = true;
  renaming = null;
}

/** The open conversation's name at the top, after it changed. */
function renderChatHead() {
  const t = state.threads.get(state.current);
  if (t) $('chat-name').textContent = threadName(t);
}

$('rename-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const id = renaming;
  const t = state.threads.get(id);
  if (!t) return;
  const name = $('rename-input').value.trim();
  if (t.kind === 'group' && !name) {
    toast('Tên nhóm không được để trống');
    return;
  }
  $('rename-save').disabled = true;
  try {
    const updated = await api(`/threads/${encodeURIComponent(id)}/name`, { method: 'PUT', json: { name } });
    state.threads.set(id, Object.assign(t, updated));
    if (t.kind === 'user') {
      const p = state.people.get(t.remote_id);
      if (p) p.alias = updated.alias || '';
    }
    closeRename();
    renderThreads();
    renderChatHead();
    if (info.thread === id) renderInfo();
    toast(t.kind === 'group' ? 'Đã đổi tên nhóm' : name ? 'Đã đặt tên gợi nhớ' : 'Đã bỏ tên gợi nhớ');
  } catch (err) {
    handleError(err, 'Không đổi được tên: ');
  } finally {
    $('rename-save').disabled = false;
  }
});
$('rename-cancel').addEventListener('click', closeRename);
$('rename').addEventListener('click', (e) => {
  if (e.target === $('rename')) closeRename();
});

// ------------------------------------------------------------ new groups

const newGroup = { chosen: new Set() };

function openNewGroup() {
  newGroup.chosen = new Set();
  $('group-name').value = '';
  $('group-search').value = '';
  renderGroupChoices();
  $('new-group').hidden = false;
  $('group-name').focus();
}

function closeNewGroup() {
  $('new-group').hidden = true;
}

function renderGroupChoices() {
  const q = fold($('group-search').value.trim());
  const ul = $('group-list');
  ul.replaceChildren();
  const list = friendList().filter((p) => !q || fold(personName(p)).includes(q));
  if (!list.length) ul.append(el('li', 'list-empty', q ? 'Không tìm thấy bạn nào' : 'Chưa có bạn bè'));
  for (const p of list) {
    const chosen = newGroup.chosen.has(p.uid);
    const li = el('li', chosen ? 'forward-item chosen' : 'forward-item');
    const box = el('input');
    box.type = 'checkbox';
    box.checked = chosen;
    box.tabIndex = -1;
    box.setAttribute('aria-label', personName(p));
    const body = el('div', 'body');
    body.append(el('div', 'name', personName(p)));
    li.append(box, avatarBox(`zalo:u:${p.uid}`, personName(p), p.avatar), body);
    li.addEventListener('click', () => {
      if (newGroup.chosen.has(p.uid)) newGroup.chosen.delete(p.uid);
      else if (newGroup.chosen.size < 100) newGroup.chosen.add(p.uid);
      renderGroupChoices();
    });
    ul.append(li);
  }
  const n = newGroup.chosen.size;
  const create = $('group-create');
  create.disabled = n < 2;
  create.textContent = n ? `Tạo nhóm (${n + 1} người)` : 'Tạo nhóm';
  create.title = n < 2 ? 'Chọn ít nhất 2 người' : '';
}

async function createGroup() {
  const create = $('group-create');
  create.disabled = true;
  try {
    const made = await api('/groups', {
      json: { name: $('group-name').value.trim(), members: [...newGroup.chosen] },
    });
    closeNewGroup();
    const t = made.thread;
    if (t) {
      state.threads.set(t.id, t);
      openThread(t.id);
    }
    const failed = (made.failed || []).length;
    toast(failed ? `Đã tạo nhóm; ${failed} người chưa thêm được` : 'Đã tạo nhóm');
  } catch (err) {
    create.disabled = false;
    handleError(err, 'Không tạo được nhóm: ');
  }
}

$('new-group-btn').addEventListener('click', openNewGroup);
$('group-search').addEventListener('input', renderGroupChoices);
$('group-cancel').addEventListener('click', closeNewGroup);
$('group-create').addEventListener('click', createGroup);
$('new-group').addEventListener('click', (e) => {
  if (e.target === $('new-group')) closeNewGroup();
});

// ------------------------------------------------------------ adding friends

const FRIENDSHIP_TEXT = {
  friend: 'Đã là bạn bè',
  request_sent: 'Đã gửi lời mời kết bạn, đang chờ trả lời',
  request_received: 'Người này đã gửi lời mời kết bạn cho bạn',
};

function openAddFriend() {
  $('add-friend').hidden = false;
  $('find-result').replaceChildren();
  $('find-phone').value = '';
  $('find-phone').focus();
}

function closeAddFriend() {
  $('add-friend').hidden = true;
}

/** What a phone number search found: a card with what can be done next. */
function renderFound(user) {
  const box = $('find-result');
  box.replaceChildren();
  if (!user) {
    box.append(el('p', 'muted find-none', 'Không tìm thấy tài khoản Zalo nào với số này, hoặc người đó không cho tìm bằng số điện thoại.'));
    return;
  }
  const name = user.name || user.zalo_name || 'Người dùng Zalo';
  const card = el('div', 'found-card');
  const picture = avatarBox(`zalo:u:${user.uid}`, name, user.avatar);
  picture.classList.add('info-avatar');
  card.append(picture, el('div', 'info-name', name));
  if (FRIENDSHIP_TEXT[user.friendship]) card.append(el('div', 'muted small', FRIENDSHIP_TEXT[user.friendship]));
  const actions = el('div', 'row');
  const chat = el('button', 'btn secondary', 'Nhắn tin');
  chat.type = 'button';
  chat.addEventListener('click', () => {
    closeAddFriend();
    openPerson({ uid: user.uid, name, avatar: user.avatar });
  });
  if (user.friendship === 'none') {
    const note = el('textarea', 'field found-note');
    note.rows = 2;
    note.maxLength = 150;
    note.setAttribute('aria-label', 'Lời nhắn kèm lời mời');
    const me = state.status && state.status.display_name;
    note.value = me ? `Xin chào, mình là ${me}. Kết bạn với mình nhé!` : 'Xin chào, kết bạn với mình nhé!';
    const add = el('button', 'btn', 'Kết bạn');
    add.type = 'button';
    add.addEventListener('click', async () => {
      add.disabled = true;
      try {
        await api('/friends/requests', { json: { uid: user.uid, message: note.value } });
        toast('Đã gửi lời mời kết bạn');
        renderFound({ ...user, friendship: 'request_sent' });
      } catch (err) {
        add.disabled = false;
        handleError(err, 'Không gửi được lời mời: ');
      }
    });
    card.append(note);
    actions.append(add, chat);
  } else if (user.friendship === 'request_received') {
    const accept = el('button', 'btn', 'Đồng ý kết bạn');
    accept.type = 'button';
    accept.addEventListener('click', async () => {
      if (await answerRequest(user.uid, true)) renderFound({ ...user, friendship: 'friend' });
    });
    actions.append(accept, chat);
  } else {
    actions.append(chat);
  }
  card.append(actions);
  box.append(card);
}

$('add-friend-btn').addEventListener('click', openAddFriend);
$('add-close').addEventListener('click', closeAddFriend);
$('add-friend').addEventListener('click', (e) => {
  if (e.target === $('add-friend')) closeAddFriend();
});
$('find-form').addEventListener('submit', async (e) => {
  e.preventDefault();
  const phone = $('find-phone').value.trim();
  if (!phone) return;
  const box = $('find-result');
  box.replaceChildren(el('p', 'muted find-none', 'Đang tìm…'));
  try {
    const data = await api(`/users/find?phone=${encodeURIComponent(phone)}`);
    renderFound(data.user);
  } catch (err) {
    if (err instanceof Unauthorized) {
      handleError(err);
      return;
    }
    box.replaceChildren(el('p', 'error', err.message));
  }
});

// Names and pictures of people who are not friends (group members), asked for
// as their messages show up, once per page load.
const peopleAsked = new Set();
const peopleWanted = new Set();
let peopleTimer = 0;

function wantPeople(uids) {
  for (const uid of uids) {
    if (!/^\d+$/.test(uid || '') || peopleAsked.has(uid)) continue;
    const known = state.people.get(uid);
    if (known && known.avatar) continue;
    peopleWanted.add(uid);
  }
  if (peopleWanted.size && !peopleTimer) peopleTimer = setTimeout(fetchPeople, 30);
}

async function fetchPeople() {
  const uids = [...peopleWanted].slice(0, 100);
  for (const uid of uids) {
    peopleWanted.delete(uid);
    peopleAsked.add(uid);
  }
  try {
    const list = await api(`/people?ids=${uids.join(',')}`);
    for (const p of list) state.people.set(p.uid, { ...(state.people.get(p.uid) || {}), ...p });
    refreshAvatars();
  } catch (err) {
    if (err instanceof Unauthorized) handleError(err);
  } finally {
    peopleTimer = peopleWanted.size ? setTimeout(fetchPeople, 30) : 0;
  }
}

/** Put pictures that just became known on the messages shown. */
function refreshAvatars() {
  for (const node of document.querySelectorAll('#messages .msg-avatar[data-uid]')) {
    const p = state.people.get(node.dataset.uid);
    if (!p || !p.avatar || node.dataset.src === p.avatar) continue;
    const fresh = avatarBox(`zalo:u:${p.uid}`, node.title || personName(p), p.avatar);
    fresh.classList.add('msg-avatar');
    fresh.dataset.uid = p.uid;
    fresh.title = node.title;
    node.replaceWith(fresh);
  }
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
  renderChatHead();
}

for (const tab of document.querySelectorAll('.tab')) {
  tab.addEventListener('click', () => {
    for (const other of document.querySelectorAll('.tab')) other.classList.toggle('active', other === tab);
    state.filter = tab.dataset.filter;
    renderThreads();
    // Requests come and go: look again when the list is opened.
    if (state.filter === 'friends') loadRequests();
  });
}

let searchTimer = 0;
let searchTicket = 0;

$('search').addEventListener('input', (e) => {
  state.search = e.target.value;
  state.hits = null;
  clearTimeout(searchTimer);
  const q = state.search.trim();
  if (q.length >= 2) searchTimer = setTimeout(() => searchMessages(q), 250);
  renderThreads();
});

async function searchMessages(q) {
  const ticket = ++searchTicket;
  try {
    const data = await api(`/search?q=${encodeURIComponent(q)}&limit=40`);
    if (ticket !== searchTicket || state.search.trim() !== q) return;
    state.hits = data.messages || [];
  } catch (err) {
    if (ticket !== searchTicket) return;
    state.hits = [];
    handleError(err, 'Không tìm được: ');
  }
  renderThreads();
}

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

// Animated stickers: Zalo draws them from a sprite sheet, the frames side by
// side, each shown for a while. The still picture shows first; once the
// gateway says the sticker moves and the sheet has loaded, it takes over.

// v2: v1 took stickers whose frames Zalo leaves to the sprite for still ones.
const MOTION_KEY = 'zetachat.stickerMotion.v2';
const motions = new Map(); // sticker id -> { sprite, frames, duration_ms } | null
const motionWaiting = new Map(); // sticker id -> callbacks
let motionTimer = 0;

try {
  for (const [id, m] of JSON.parse(localStorage.getItem(MOTION_KEY) || '[]')) motions.set(id, m);
} catch {
  // A convenience: without it the gateway is asked again.
}

function saveMotions() {
  try {
    localStorage.setItem(MOTION_KEY, JSON.stringify([...motions].slice(-1500)));
  } catch {
    // Same.
  }
}

/** A sticker's id: kept with the message, else in its picture's address. */
function stickerId(md) {
  if (md.sticker_id) return Number(md.sticker_id);
  const found = /[?&]eid=(\d+)/.exec(md.url || '');
  return found ? Number(found[1]) : 0;
}

/** Call `then` with how sticker `id` moves (null when it does not). */
function stickerMotion(id, then) {
  if (!id) return;
  if (motions.has(id)) {
    then(motions.get(id));
    return;
  }
  if (!motionWaiting.has(id)) motionWaiting.set(id, []);
  motionWaiting.get(id).push(then);
  if (!motionTimer) motionTimer = setTimeout(askMotions, 40);
}

async function askMotions() {
  motionTimer = 0;
  const ids = [...motionWaiting.keys()].slice(0, 50);
  let answer = {};
  try {
    answer = await api(`/stickers/motion?ids=${ids.join(',')}`);
  } catch {
    // Still pictures, then.
  }
  for (const id of ids) {
    const waiting = motionWaiting.get(id) || [];
    motionWaiting.delete(id);
    const m = Object.prototype.hasOwnProperty.call(answer, id) ? answer[id] : null;
    if (Object.prototype.hasOwnProperty.call(answer, id)) motions.set(id, m);
    for (const then of waiting) then(m);
  }
  saveMotions();
  if (motionWaiting.size) motionTimer = setTimeout(askMotions, 40);
}

/**
 * Replace a still sticker picture by its moving one, once loaded. The sprite
 * decides, as Zalo often leaves the frame count to it: frames side by side
 * slide by one at a time; another picture (an animated WebP) plays by
 * itself; a sprite just like the still picture changes nothing.
 */
function animateSticker(img, m) {
  if (!m || !m.sprite || !img.isConnected) return;
  const sheet = new Image();
  sheet.alt = img.alt;
  sheet.addEventListener('load', () => {
    if (!img.isConnected || !sheet.naturalHeight) return;
    const ratio = sheet.naturalWidth / sheet.naturalHeight;
    const frames = m.frames > 1 ? m.frames : Math.round(ratio);
    if (frames >= 2 && Math.abs(ratio - frames) < 0.05) {
      const box = el('div', `${img.className} sticker-moving`);
      sheet.className = 'sticker-strip';
      sheet.style.animationDuration = `${frames * (m.duration_ms || 100)}ms`;
      sheet.style.animationTimingFunction = `steps(${frames})`;
      box.append(sheet);
      img.replaceWith(box);
    } else if (m.ext === 2 || sheet.naturalWidth !== img.naturalWidth || sheet.naturalHeight !== img.naturalHeight) {
      sheet.className = img.className;
      img.replaceWith(sheet);
    }
  }, { once: true });
  sheet.src = mediaSrc(m.sprite);
}

function stickerNode(md) {
  const img = new Image();
  img.className = 'sticker-img';
  img.alt = 'Nhãn dán';
  img.loading = 'lazy';
  img.addEventListener('error', () => img.replaceWith(el('div', 'media-failed', '[Nhãn dán]')));
  img.addEventListener('load', () => stickerMotion(stickerId(md), (m) => animateSticker(img, m)), { once: true });
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

/** Zalo's text emoticons (the same codes as its reactions) and how they look. */
const ZALO_EMOTICONS = {
  '/-strong': '👍', '/-heart': '❤️', ':>': '😆', ':o': '😮', ':-((': '😭', ':-h': '😡',
  ':-*': '😘', ":')": '😂', '/-shit': '💩', '/-rose': '🌹', '/-break': '💔', '/-weak': '👎',
  ';xx': '😍', ';-/': '😕', ';-)': '😉', '/-fade': '🥀', '/-li': '☀️', '/-bd': '🎂',
  '/-bome': '💣', '/-ok': '👌', '/-v': '✌️', '/-thanks': '🙏', '/-punch': '👊', '_()_': '🙏',
  '/-no': '🙅', '/-loveu': '🤟', '--b': '😓', ':((': '😢', 'x-)': '😎', '8-)': '🤓',
  ';-d': '😁', 'b-)': '😎', ':--|': '😐', 'p-(': '😞', ':-bye': '👋', '|-)': '😴',
  ':wipe': '😅', '&-(': '😧', ':handclap': '👏', '>-|': '😠', ';-x': '🤐', ':-o': '😲',
  ';-s': '😳', ';-a': '😨', ':-<': '😟', ':))': '😄', '$-)': '🤑', '/-beer': '🍺',
};

/**
 * Text as Zalo shows it: a message that is just an emoticon code becomes its
 * emoji, and `/-name` codes standing alone between spaces do too. Other
 * codes (`:o`…) are left alone inside sentences, where they may be plain text.
 */
function displayText(text) {
  const whole = ZALO_EMOTICONS[(text || '').trim()];
  if (whole) return whole;
  return inlineEmoticons(text);
}

function inlineEmoticons(text) {
  return (text || '').replace(/(^|\s)(\/-[a-z]+)(?=\s|$)/g, (m, pre, code) => pre + (ZALO_EMOTICONS[code] || code));
}

function mentionsMe(m) {
  const me = state.status && state.status.uid;
  return (m.mentions || []).some((x) => x.uid === '-1' || (me && x.uid === me));
}

/** Text with the people it names picked out (positions in UTF-16 units, as Zalo counts). */
function appendMentioned(parent, text, mentions) {
  const me = state.status && state.status.uid;
  let last = 0;
  for (const r of [...mentions].sort((a, b) => a.pos - b.pos)) {
    if (r.pos < last || r.pos + r.len > text.length) continue;
    if (r.pos > last) appendText(parent, inlineEmoticons(text.slice(last, r.pos)));
    const mine = r.uid === '-1' || (me && r.uid === me);
    parent.append(el('span', mine ? 'mention me' : 'mention', text.slice(r.pos, r.pos + r.len)));
    last = r.pos + r.len;
  }
  if (last < text.length) appendText(parent, inlineEmoticons(text.slice(last)));
}

const INLINE_STYLE = /^(?:b|i|u|s|c_[0-9a-f]{6}|f_\d{1,3})$/i;
const LINE_STYLE = /^(?:lst_[12]|ind_\d{1,3})$/;
const PALETTE = new Set(['db342e', 'f27806', 'f7b503', '15a85f']);

/** Draw a run of text as Zalo's codes say (`b`, `i`, `u`, `s`, colours, sizes). */
function decorate(node, styles) {
  const lines = [];
  for (const s of styles) {
    if (s.st === 'b') node.style.fontWeight = '700';
    else if (s.st === 'i') node.style.fontStyle = 'italic';
    else if (s.st === 'u') lines.push('underline');
    else if (s.st === 's') lines.push('line-through');
    else if (s.st.startsWith('c_')) {
      const hex = s.st.slice(2).toLowerCase();
      if (PALETTE.has(hex)) node.classList.add(`rt-c-${hex}`);
      else node.style.color = `#${hex}`;
    } else if (s.st.startsWith('f_')) {
      const px = Number(s.st.slice(2));
      if (px >= 8 && px <= 40) node.style.fontSize = `${px}px`;
    }
  }
  if (lines.length) node.style.textDecorationLine = lines.join(' ');
}

/**
 * A styled text (positions in UTF-16 units, as Zalo counts): line by line,
 * lists (`lst_1` bullets, `lst_2` numbers) and indents (`ind_<n>0`) for
 * whole lines, the other codes on runs; the people it names picked out.
 */
function appendRich(parent, text, styles, mentions) {
  const me = state.status && state.status.uid;
  const inline = styles.filter((s) => INLINE_STYLE.test(s.st));
  const perLine = styles.filter((s) => LINE_STYLE.test(s.st));
  const named = (mentions || []).filter((r) => r.len > 0 && r.pos + r.len <= text.length);
  const box = el('div', 'rich');
  let from = 0;
  let number = 0;
  let previous = null;
  for (const line of text.split('\n')) {
    const to = from + line.length;
    const covers = (s) => s.start < Math.max(to, from + 1) && s.start + s.len > from;
    const row = el('div', 'rt-line');
    const list = perLine.find((s) => s.st.startsWith('lst_') && covers(s));
    if (list) {
      number = previous === 'lst_2' && list.st === 'lst_2' ? number + 1 : 1;
      row.classList.add('rt-li');
      row.dataset.marker = list.st === 'lst_2' ? `${number}.` : '•';
    }
    previous = list ? list.st : null;
    const indent = Math.max(0, ...perLine.filter((s) => s.st.startsWith('ind_') && covers(s)).map((s) => Math.floor(Number(s.st.slice(4)) / 10)));
    if (indent) row.style.paddingLeft = `${Math.min(indent, 6) * 20}px`;
    const cuts = new Set([from, to]);
    const cut = (at) => { if (at > from && at < to) cuts.add(at); };
    for (const s of inline) { cut(s.start); cut(s.start + s.len); }
    for (const r of named) { cut(r.pos); cut(r.pos + r.len); }
    const points = [...cuts].sort((a, b) => a - b);
    for (let k = 0; k + 1 < points.length; k += 1) {
      const [a, b] = [points[k], points[k + 1]];
      const part = text.slice(a, b);
      const on = inline.filter((s) => s.start <= a && s.start + s.len >= b);
      const who = named.find((r) => r.pos <= a && r.pos + r.len >= b);
      if (!on.length && !who) {
        appendText(row, inlineEmoticons(part));
        continue;
      }
      const span = el('span', who ? (who.uid === '-1' || (me && who.uid === me) ? 'mention me' : 'mention') : null);
      decorate(span, on);
      if (who) span.textContent = part;
      else appendText(span, inlineEmoticons(part));
      row.append(span);
    }
    if (!line.length) row.append(document.createElement('br'));
    box.append(row);
    from = to + 1;
  }
  parent.append(box);
}

const URGENCY_TEXT = ['', 'Quan trọng', 'Khẩn cấp'];

const EMOJI_ONLY = /^(?:\p{Extended_Pictographic}|\p{Emoji_Modifier}|\p{Regional_Indicator}|[\u200d\ufe0f\u20e3]|\s)+$/u;

/** Up to three emoji and nothing else: shown large, without a bubble. */
function emojiOnly(text) {
  const t = (text || '').trim();
  if (!t || t.length > 40 || !EMOJI_ONLY.test(t)) return false;
  if (typeof Intl.Segmenter !== 'function') return t.length <= 8;
  return [...new Intl.Segmenter().segment(t.replace(/\s+/g, ''))].length <= 3;
}

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

/** The picture next to an incoming message: the chat's, or the group member's. */
function senderAvatar(m, t) {
  if (t && t.kind === 'user') return avatar(t);
  const p = state.people.get(m.sender_id);
  const name = senderLabel(m);
  const box = avatarBox(`zalo:u:${m.sender_id}`, name, p && p.avatar);
  box.dataset.uid = m.sender_id;
  box.title = name;
  return box;
}

function messageNode(m, prev, t) {
  const isGroup = Boolean(t && t.kind === 'group');
  const frag = document.createDocumentFragment();
  if (!prev || !sameDay(prev.sent_at, m.sent_at)) frag.appendChild(el('div', 'day', fmtDay(m.sent_at)));
  const recalled = m.kind === 'chat.undo';
  const md = recalled ? null : m.media;
  const media = md && MEDIA[md.kind];
  const styled = !recalled && !media && m.styles && m.styles.length > 0;
  const text = recalled || media || styled ? m.text : displayText(m.text);
  const urgency = recalled ? 0 : m.urgency || 0;
  let cls = `msg ${m.outgoing ? 'out' : 'in'}`;
  if (media) cls += ` ${media[1]}`;
  if (recalled) cls += ' recalled';
  else if (!media && !m.quote && !styled && !urgency && emojiOnly(text)) cls += ' emoji-only';
  if (urgency === 1) cls += ' important';
  if (urgency === 2) cls += ' urgent';
  const bubble = el('div', cls);
  bubble.dataset.seq = String(m.seq);
  const newSender = !prev || prev.outgoing || prev.sender_id !== m.sender_id || !sameDay(prev.sent_at, m.sent_at);
  if (isGroup && !m.outgoing && newSender) {
    bubble.appendChild(el('div', 'sender', senderLabel(m) || 'Thành viên'));
  }
  if (urgency) bubble.appendChild(el('div', 'urgency-tag', URGENCY_TEXT[urgency]));
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
  } else if (styled) {
    appendRich(bubble, m.text || '', m.styles, m.mentions);
  } else if (m.mentions && m.mentions.length) {
    appendMentioned(bubble, m.text || '', m.mentions);
  } else {
    appendText(bubble, text || '');
  }
  if (!recalled && m.reactions && m.reactions.length) bubble.appendChild(reactionSummary(m));
  bubble.appendChild(el('span', 'time', fmtClock(m.sent_at)));
  if (canReact(m)) {
    const button = el('button', 'react-btn', '👍');
    button.type = 'button';
    button.title = 'Thả cảm xúc';
    button.setAttribute('aria-label', 'Thả cảm xúc');
    button.addEventListener('click', (e) => {
      e.stopPropagation();
      openPicker(m, button);
    });
    bubble.appendChild(button);
  }
  if (m.outgoing) {
    frag.appendChild(bubble);
  } else {
    // Zalo shows the sender's picture once per run of their messages.
    const face = newSender ? senderAvatar(m, t) : el('div', 'avatar spacer');
    face.classList.add('msg-avatar');
    const line = el('div', 'msg-line');
    line.append(face, bubble);
    frag.appendChild(line);
  }
  return frag;
}

// ---------------------------------------------------------------- reactions

/** The six reactions Zalo offers, in its order: icon code and emoji. */
const REACTIONS = [
  ['/-strong', '👍', 'Thích'],
  ['/-heart', '❤️', 'Yêu thích'],
  [':>', '😆', 'Haha'],
  [':o', '😮', 'Ngạc nhiên'],
  [':-((', '😭', 'Buồn'],
  [':-h', '😡', 'Phẫn nộ'],
];

/** Messages Zalo has confirmed, and not recalled. */
function canReact(m) {
  return m.kind !== 'chat.undo' && !String(m.remote_id || '').startsWith('local:');
}

/** Up to three emoji and the number of reactions, under the message. */
function reactionSummary(m) {
  const list = m.reactions;
  const total = list.reduce((n, r) => n + r.count, 0);
  const row = el('div', 'reaction-row');
  const pill = el('button', list.some((r) => r.mine) ? 'reactions mine' : 'reactions');
  pill.type = 'button';
  pill.title = list.map((r) => `${r.emoji} ${r.count}`).join(' · ');
  pill.append(list.slice(0, 3).map((r) => r.emoji).join(''));
  if (total > 1) pill.append(el('span', 'count', String(total)));
  pill.addEventListener('click', (e) => {
    e.stopPropagation();
    if (canReact(m)) openPicker(m, pill);
  });
  row.append(pill);
  return row;
}

let picker = null;

function closePicker() {
  if (!picker) return;
  picker.remove();
  picker = null;
}

function menuButton(label, action, className) {
  const button = el('button', className || null, label);
  button.type = 'button';
  button.addEventListener('click', () => {
    closePicker();
    action();
  });
  return button;
}

/** Put a floating box next to `anchor`: above it, or below when there is no room. */
function placeNear(box, anchor) {
  const at = anchor.getBoundingClientRect();
  const left = Math.min(window.innerWidth - box.offsetWidth - 8, Math.max(8, at.left + at.width / 2 - box.offsetWidth / 2));
  let top = at.top - box.offsetHeight - 6;
  if (top < 8) top = at.bottom + 6;
  box.style.left = `${left}px`;
  box.style.top = `${top}px`;
}

/** A small menu next to `anchor`: `items` are `[label, action, className, hint]`. */
function openMenu(anchor, items, title) {
  closePicker();
  const box = el('div', 'react-picker composer-menu');
  box.setAttribute('role', 'menu');
  if (title) box.append(el('div', 'menu-title', title));
  const actions = el('div', 'menu-actions column');
  for (const [label, action, className, hint] of items) {
    const button = menuButton(label, action, className);
    button.setAttribute('role', 'menuitem');
    if (hint) button.title = hint;
    actions.append(button);
  }
  box.append(actions);
  document.body.append(box);
  placeNear(box, anchor);
  picker = box;
  box.querySelector('button').focus();
}

/** Reactions on top, then what can be done with the message. */
function openPicker(m, anchor) {
  closePicker();
  const box = el('div', 'react-picker');
  box.setAttribute('role', 'menu');
  const row = el('div', 'react-row');
  for (const [icon, emoji, label] of REACTIONS) {
    const button = menuButton(emoji, () => react(m, icon));
    button.title = label;
    button.setAttribute('aria-label', label);
    row.append(button);
  }
  if ((m.reactions || []).some((r) => r.mine)) {
    const remove = menuButton('Gỡ', () => react(m, ''), 'remove');
    remove.title = 'Gỡ cảm xúc của bạn';
    row.append(remove);
  }
  const actions = el('div', 'menu-actions');
  actions.append(menuButton('Trả lời', () => startReply(m)));
  if (canForward(m)) actions.append(menuButton('Chuyển tiếp', () => openForward(m)));
  if (!m.media && m.text) actions.append(menuButton('Sao chép', () => copyText(m.text)));
  if (m.outgoing) actions.append(menuButton('Thu hồi', () => recallMessage(m), 'danger'));
  const remove = menuButton('Xóa', () => deleteForMe(m), 'danger');
  remove.title = 'Xóa ở phía bạn (người khác vẫn thấy)';
  actions.append(remove);
  box.append(row, actions);
  document.body.append(box);
  placeNear(box, anchor);
  picker = box;
  box.querySelector('button').focus();
}

document.addEventListener('click', (e) => {
  if (picker && !picker.contains(e.target)) closePicker();
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if (!$('rename').hidden) closeRename();
  else if (!$('forward').hidden) closeForward();
  else if (!$('new-group').hidden) closeNewGroup();
  else if (!$('add-friend').hidden) closeAddFriend();
  else if (picker) closePicker();
  else if (panelKind) closePanel();
  else if (info.thread && $('viewer').hidden) closeInfo();
  else if (state.replyTo && document.activeElement === input) cancelReply();
});
$('messages').addEventListener('scroll', closePicker, { passive: true });

/** Answer `m` with the next message. */
function startReply(m) {
  state.replyTo = m;
  renderReply();
  input.focus();
}

function cancelReply() {
  state.replyTo = null;
  renderReply();
}

function renderReply() {
  const m = state.replyTo;
  const bar = $('reply-bar');
  bar.hidden = !m || m.thread_id !== state.current;
  if (bar.hidden) return;
  const name = m.outgoing ? 'chính mình' : m.sender_name || threadName(state.threads.get(m.thread_id) || {});
  $('reply-name').textContent = `Trả lời ${name}`;
  $('reply-text').textContent = m.text || '';
}

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Without HTTPS the clipboard API is missing: select and copy instead.
    const area = el('textarea');
    area.value = text;
    document.body.append(area);
    area.select();
    document.execCommand('copy');
    area.remove();
  }
  toast('Đã sao chép');
}

async function recallMessage(m) {
  if (!window.confirm('Thu hồi tin nhắn này? Mọi người trong cuộc trò chuyện sẽ không còn thấy nội dung.')) return;
  try {
    const message = await api(`/threads/${encodeURIComponent(m.thread_id)}/messages/${m.seq}/recall`, { method: 'POST' });
    onMessageUpdated({ message });
  } catch (err) {
    handleError(err, 'Không thu hồi được: ');
  }
}

$('reply-cancel').addEventListener('click', cancelReply);

// ------------------------------------------------- forwarding and deleting

/** Media the gateway can forward (texts and links through Zalo, the rest sent again). */
const FORWARDABLE = ['link', 'sticker', 'image', 'gif', 'video', 'audio', 'file'];

function canForward(m) {
  if (!canReact(m)) return false;
  return m.media ? FORWARDABLE.includes(m.media.kind) : m.kind === 'webchat';
}

const forwarding = { message: null, chosen: new Set() };
const FORWARD_MAX = 20;

/** Where a message can go: recent conversations, then friends, then groups. */
function forwardTargets() {
  const out = [];
  const seen = new Set();
  const add = (id, name, picture, sub) => {
    if (seen.has(id)) return;
    seen.add(id);
    out.push({ id, name, picture, sub });
  };
  const recent = [...state.threads.values()]
    .filter((t) => t.last_message_at > 0)
    .sort((a, b) => b.last_message_at - a.last_message_at);
  for (const t of recent) add(t.id, threadName(t), t.avatar, t.kind === 'group' ? membersText(t) : '');
  for (const p of friendList()) add(`zalo:u:${p.uid}`, personName(p), p.avatar, 'Bạn bè');
  for (const t of groupList()) add(t.id, threadName(t), t.avatar, membersText(t));
  return out;
}

function openForward(m) {
  forwarding.message = m;
  forwarding.chosen = new Set();
  $('forward-search').value = '';
  $('forward-preview').textContent = m.media ? m.text : displayText(m.text);
  renderForwardList();
  $('forward').hidden = false;
  $('forward-search').focus();
}

function closeForward() {
  $('forward').hidden = true;
  forwarding.message = null;
}

function renderForwardList() {
  const q = fold($('forward-search').value.trim());
  const ul = $('forward-list');
  ul.replaceChildren();
  const list = forwardTargets().filter((x) => !q || fold(x.name).includes(q)).slice(0, 300);
  if (!list.length) ul.append(el('li', 'list-empty', 'Không tìm thấy hội thoại'));
  const full = forwarding.chosen.size >= FORWARD_MAX;
  for (const x of list) {
    const chosen = forwarding.chosen.has(x.id);
    const li = el('li', chosen ? 'forward-item chosen' : 'forward-item');
    const box = el('input');
    box.type = 'checkbox';
    box.checked = chosen;
    box.disabled = full && !chosen;
    box.tabIndex = -1;
    box.setAttribute('aria-label', x.name);
    const body = el('div', 'body');
    body.append(el('div', 'name', x.name));
    if (x.sub) body.append(el('div', 'preview', x.sub));
    li.append(box, avatarBox(x.id, x.name, x.picture), body);
    li.addEventListener('click', () => {
      if (forwarding.chosen.has(x.id)) forwarding.chosen.delete(x.id);
      else if (!full) forwarding.chosen.add(x.id);
      renderForwardList();
    });
    ul.append(li);
  }
  const n = forwarding.chosen.size;
  const send = $('forward-send');
  send.disabled = !n;
  send.textContent = n ? `Chuyển tiếp (${n})` : 'Chuyển tiếp';
}

async function sendForward() {
  const m = forwarding.message;
  if (!m || !forwarding.chosen.size) return;
  const send = $('forward-send');
  send.disabled = true;
  try {
    const data = await api(`/threads/${encodeURIComponent(m.thread_id)}/messages/${m.seq}/forward`, {
      json: { to: [...forwarding.chosen] },
    });
    const results = data.results || [];
    for (const r of results) {
      if (r.message) onMessage({ message: r.message, thread: null });
      if (r.upload) adoptUpload(r.upload);
    }
    closeForward();
    const failed = results.filter((r) => r.error);
    if (!failed.length) toast(`Đã chuyển tiếp tới ${results.length} hội thoại`);
    else toast(`Chuyển tiếp được ${results.length - failed.length}/${results.length}: ${failed[0].error}`);
  } catch (err) {
    send.disabled = false;
    handleError(err, 'Không chuyển tiếp được: ');
  }
}

$('forward-search').addEventListener('input', renderForwardList);
$('forward-cancel').addEventListener('click', closeForward);
$('forward-send').addEventListener('click', sendForward);
$('forward').addEventListener('click', (e) => {
  if (e.target === $('forward')) closeForward();
});

async function deleteForMe(m) {
  if (!window.confirm('Xóa tin nhắn này ở phía bạn? Người khác trong cuộc trò chuyện vẫn thấy tin.')) return;
  try {
    await api(`/threads/${encodeURIComponent(m.thread_id)}/messages/${m.seq}`, { method: 'DELETE' });
    removeMessage(m.thread_id, m.seq, null);
  } catch (err) {
    handleError(err, 'Không xóa được: ');
  }
}

/** A message deleted for this account: gone from the list and the preview. */
function removeMessage(threadId, seq, thread) {
  if (thread) {
    const t = state.threads.get(threadId);
    state.threads.set(threadId, Object.assign(t || {}, thread));
  }
  if (threadId === state.current) {
    const i = state.messages.findIndex((x) => x.seq === seq);
    if (i >= 0) {
      state.messages.splice(i, 1);
      renderMessages(false);
    }
  }
  if (state.replyTo && state.replyTo.seq === seq) cancelReply();
  renderThreads();
}

async function react(m, icon) {
  try {
    const message = await api(`/threads/${encodeURIComponent(m.thread_id)}/messages/${m.seq}/reactions`, { json: { icon } });
    onMessageUpdated({ message });
  } catch (err) {
    handleError(err, 'Không thả được cảm xúc: ');
  }
}

/** Ask for the names and pictures of the senders shown in a group. */
function wantSenders(messages) {
  const t = state.threads.get(state.current);
  if (t && t.kind === 'group') wantPeople(messages.filter((m) => !m.outgoing).map((m) => m.sender_id));
}

const SENT_STATUS = ['Đã gửi', 'Đã nhận', 'Đã xem'];

/** How far the last message got, under it, when it is the account's own. */
function renderSentStatus() {
  const box = $('messages');
  for (const old of box.querySelectorAll('.sent-status')) old.remove();
  const last = state.messages[state.messages.length - 1];
  if (!last || !last.outgoing || last.kind === 'chat.undo') return;
  const local = String(last.remote_id || '').startsWith('local:');
  const text = local ? 'Đang gửi…' : SENT_STATUS[last.status || 0] || SENT_STATUS[0];
  box.appendChild(el('div', last.status === 2 ? 'sent-status seen' : 'sent-status', text));
}

/** Bring a message into view and make it blink once. */
function showMessage(seq) {
  const node = document.querySelector(`#messages .msg[data-seq="${seq}"]`);
  if (!node) return;
  node.scrollIntoView({ block: 'center' });
  node.classList.add('flash');
  setTimeout(() => node.classList.remove('flash'), 1800);
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
  const t = state.threads.get(state.current);
  let prev = null;
  for (const m of state.messages) {
    box.appendChild(messageNode(m, prev, t));
    prev = m;
  }
  renderSentStatus();
  box.scrollTop = scrollToEnd ? box.scrollHeight : box.scrollHeight - fromBottom;
  wantSenders(state.messages);
}

async function fetchMessages(id, before) {
  const query = before ? `&before=${before}` : '';
  return api(`/threads/${encodeURIComponent(id)}/messages?limit=50${query}`);
}

async function openThread(id, focusSeq) {
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
  if (presence.thread !== t.id) watchPresence(t);
  renderChatSub();
  $('messages').replaceChildren();
  renderThreads();
  renderUploads();
  if (state.replyTo && state.replyTo.thread_id !== id) state.replyTo = null;
  renderReply();
  if (panelKind === 'emoji') closePanel();
  if (info.thread && info.thread !== id) openInfo();
  state.mentions = [];
  closeMentionBox();
  try {
    let list = await fetchMessages(id);
    let full = list.length === 50;
    // A search result further back: older pages until it is there.
    while (focusSeq && full && list.length < 1000 && list[0].seq > focusSeq) {
      if (state.current !== id) return;
      const older = await fetchMessages(id, list[0].seq);
      full = older.length === 50;
      list = older.concat(list);
    }
    if (state.current !== id) return;
    state.messages = list;
    state.hasOlder = full;
    renderMessages(!focusSeq);
    if (focusSeq) showMessage(focusSeq);
  } catch (err) {
    handleError(err);
  }
  markRead(id);
  focusComposer();
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
  box.appendChild(messageNode(m, prev, state.threads.get(state.current)));
  renderSentStatus();
  if (nearBottom || m.outgoing) box.scrollTop = box.scrollHeight;
  wantSenders([m]);
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
  closeInfo();
  stopPresence();
  $('app').classList.remove('show-chat');
  state.current = null;
  renderThreads();
});

// -------------------------------------------------------- conversation info
//
// The panel next to a conversation: the group's members, and the photos,
// videos, files and links shared in it (from the gateway's store).

const ROLE_TEXT = { owner: 'Trưởng nhóm', admin: 'Phó nhóm' };

const info = {
  thread: null, // conversation shown, null when closed
  members: null, // { total, members } of a group, or { error }
  shared: {}, // type -> messages, missing while loading
  allMembers: false,
};

function openInfo() {
  const t = state.threads.get(state.current);
  if (!t) return;
  info.thread = t.id;
  info.members = null;
  info.shared = {};
  info.allMembers = false;
  $('app').classList.add('with-info');
  $('info').hidden = false;
  $('info-toggle').classList.add('on');
  renderInfo();
  loadInfo(t);
}

function closeInfo() {
  info.thread = null;
  $('app').classList.remove('with-info');
  $('info').hidden = true;
  $('info').replaceChildren();
  $('info-toggle').classList.remove('on');
}

async function loadInfo(t) {
  const id = t.id;
  const path = `/threads/${encodeURIComponent(id)}`;
  const jobs = ['media', 'file', 'link'].map(async (type) => {
    let list = [];
    try {
      list = await api(`${path}/shared?type=${type}&limit=${type === 'media' ? 12 : 6}`);
    } catch (err) {
      if (err instanceof Unauthorized) handleError(err);
    }
    if (info.thread !== id) return;
    info.shared[type] = list;
    renderInfo();
  });
  if (t.kind === 'group') {
    try {
      const data = await api(`${path}/members`);
      if (info.thread !== id) return;
      info.members = data;
      t.members = data.total;
      for (const m of data.members) {
        if (m.name || m.avatar) {
          state.people.set(m.uid, { ...(state.people.get(m.uid) || {}), uid: m.uid, name: m.name, avatar: m.avatar, friend: m.friend });
        }
      }
      if (id === state.current) renderChatSub();
    } catch (err) {
      if (info.thread === id) info.members = { error: err.message || 'lỗi không rõ' };
    }
    if (info.thread === id) renderInfo();
  }
  await Promise.all(jobs);
}

function infoSection(title, count) {
  const section = el('section', 'info-section');
  const h = el('h3', 'info-section-title', title);
  if (count) h.append(el('span', 'muted', ` · ${count}`));
  section.append(h);
  return section;
}

function renderInfo() {
  const box = $('info');
  const t = state.threads.get(info.thread);
  if (!t) return;
  const scroll = box.scrollTop;
  box.replaceChildren();
  const head = el('div', 'info-head');
  const close = el('button', 'upload-cancel', '×');
  close.type = 'button';
  close.title = 'Đóng';
  close.setAttribute('aria-label', 'Đóng');
  close.addEventListener('click', closeInfo);
  head.append(el('div', 'info-title', t.kind === 'group' ? 'Thông tin nhóm' : 'Thông tin hội thoại'), close);
  const profile = el('div', 'info-profile');
  const picture = avatar(t);
  picture.classList.add('info-avatar');
  const nameRow = el('div', 'info-name-row');
  const edit = el('button', 'icon-mini');
  edit.type = 'button';
  edit.title = t.kind === 'group' ? 'Đổi tên nhóm' : 'Đặt tên gợi nhớ';
  edit.setAttribute('aria-label', edit.title);
  edit.innerHTML = '<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"><path d="M4 20h4l10.5-10.5a2.1 2.1 0 0 0-3-3L5 17v3z" fill="none" stroke="currentColor" stroke-width="2" stroke-linejoin="round"/></svg>';
  edit.addEventListener('click', () => openRename(t.id));
  nameRow.append(el('div', 'info-name', threadName(t)), edit);
  profile.append(picture, nameRow);
  if (t.kind === 'group') profile.append(el('div', 'muted small', membersText(t)));
  else if (t.alias && t.name && t.alias !== t.name) profile.append(el('div', 'muted small', `Tên Zalo: ${t.name}`));
  box.append(head, profile);
  if (t.kind === 'group') box.append(membersSection());
  box.append(sharedMediaSection(), sharedListSection('file', 'Tệp', 'Chưa có tệp nào.'), sharedListSection('link', 'Liên kết', 'Chưa có liên kết nào.'));
  box.scrollTop = scroll;
}

function membersSection() {
  const data = info.members;
  const section = infoSection('Thành viên', data && !data.error ? data.total : 0);
  if (!data) {
    section.append(el('div', 'list-note', 'Đang tải…'));
    return section;
  }
  if (data.error) {
    section.append(el('div', 'list-note', `Không tải được: ${data.error}`));
    return section;
  }
  const me = state.status && state.status.uid;
  const ul = el('ul', 'member-list');
  for (const m of info.allMembers ? data.members : data.members.slice(0, 20)) {
    const li = el('li', 'member');
    const body = el('div', 'body');
    const name = m.name || `Người dùng ${m.uid}`;
    body.append(el('div', 'name', m.uid === me ? `${name} (bạn)` : name));
    if (ROLE_TEXT[m.role]) body.append(el('div', 'member-role', ROLE_TEXT[m.role]));
    li.append(avatarBox(`zalo:u:${m.uid}`, m.name, m.avatar), body);
    if (m.uid !== me) {
      li.classList.add('clickable');
      li.title = `Nhắn tin cho ${name}`;
      li.addEventListener('click', () => {
        closeInfo();
        openPerson({ uid: m.uid, name: m.name, avatar: m.avatar });
      });
    }
    ul.append(li);
  }
  section.append(ul);
  if (!info.allMembers && data.members.length > 20) {
    const more = el('button', 'btn secondary info-more', `Xem cả ${data.members.length} thành viên`);
    more.type = 'button';
    more.addEventListener('click', () => {
      info.allMembers = true;
      renderInfo();
    });
    section.append(more);
  }
  if (data.total > data.members.length) {
    section.append(el('div', 'list-note', `Zalo liệt kê ${data.members.length} trong ${data.total} thành viên.`));
  }
  return section;
}

/** Bring a shared message into view (from the info panel). */
function jumpTo(m) {
  if (window.matchMedia('(max-width: 760px)').matches) closeInfo();
  if (document.querySelector(`#messages .msg[data-seq="${m.seq}"]`)) showMessage(m.seq);
  else openThread(m.thread_id, m.seq);
}

function sharedMediaSection() {
  const list = info.shared.media;
  const section = infoSection('Ảnh, video');
  if (!list) {
    section.append(el('div', 'list-note', 'Đang tải…'));
    return section;
  }
  if (!list.length) {
    section.append(el('div', 'list-note', 'Chưa có ảnh hay video nào.'));
    return section;
  }
  const grid = el('div', 'shared-grid');
  for (const m of list) {
    const md = m.media || {};
    const cell = el('button', 'shared-cell');
    cell.type = 'button';
    cell.title = fmtListTime(m.sent_at);
    const src = md.thumb || (md.kind === 'image' ? md.url : '');
    if (src) {
      const img = new Image();
      img.alt = '';
      img.loading = 'lazy';
      img.src = mediaSrc(src);
      img.addEventListener('error', () => img.remove());
      cell.append(img);
    }
    if (md.kind === 'video') cell.append(el('span', 'shared-badge', md.duration_ms ? fmtDuration(md.duration_ms) : '▶'));
    cell.addEventListener('click', () => (md.kind === 'image' ? openViewer(md) : jumpTo(m)));
    grid.append(cell);
  }
  section.append(grid);
  return section;
}

function sharedListSection(type, title, emptyText) {
  const list = info.shared[type];
  const section = infoSection(title);
  if (!list) {
    section.append(el('div', 'list-note', 'Đang tải…'));
    return section;
  }
  if (!list.length) {
    section.append(el('div', 'list-note', emptyText));
    return section;
  }
  const ul = el('ul', 'shared-list');
  for (const m of list) {
    const md = m.media || {};
    const a = el('a');
    const lines = el('div', 'shared-lines');
    if (type === 'file') {
      if (md.size > GATEWAY_MAX_FILE && isWebUrl(md.url)) {
        a.href = md.url;
        a.target = '_blank';
        a.rel = 'noopener noreferrer';
      } else {
        a.href = mediaSrc(md.url);
        a.download = md.name || '';
      }
      const size = fmtSize(md.size);
      lines.append(el('div', 'file-name', md.name || 'Tập tin'), el('div', 'file-size', [size, fmtListTime(m.sent_at)].filter(Boolean).join(' · ')));
      a.append(el('div', 'file-icon', extOf(md.name) || 'TỆP'), lines);
    } else {
      if (!isWebUrl(md.url)) continue;
      a.href = md.url;
      a.target = '_blank';
      a.rel = 'noopener noreferrer';
      lines.append(el('div', 'file-name', md.name || md.url), el('div', 'file-size', hostOf(md.url)));
      a.append(el('div', 'file-icon link-icon', '🔗'), lines);
    }
    const li = el('li', 'shared-item');
    li.append(a);
    ul.append(li);
  }
  section.append(ul);
  return section;
}

$('info-toggle').addEventListener('click', () => (info.thread ? closeInfo() : openInfo()));
document.querySelector('.chat-head').addEventListener('click', (e) => {
  if (e.target.closest('#chat-avatar, .chat-title') && !info.thread) openInfo();
});

// ------------------------------------------------------------------- typing

/** Zalo shows "typing" for a few seconds after each announcement. */
const TYPING_SHOWN_MS = 6000;
/** How often to tell the gateway that we are typing. */
const TYPING_SEND_MS = 4000;

function typingText(t, typing) {
  if (t.kind === 'group' && typing.name) return `${typing.name} đang soạn tin…`;
  return 'Đang soạn tin…';
}

// When the other side of a 1-1 chat was last on Zalo, asked again every minute.
const presence = { thread: null, seen: null, timer: 0 };

/** "Vừa truy cập", "Truy cập 5 phút trước"… as Zalo words it; empty if hidden. */
function presenceText(seen) {
  if (!seen || !seen.visible || !seen.at) return '';
  const secs = Math.max(0, Date.now() - seen.at) / 1000;
  if (secs <= 300) return 'Vừa truy cập';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `Truy cập ${mins} phút trước`;
  const hours = Math.floor(secs / 3600);
  if (hours < 24) return `Truy cập ${hours} giờ trước`;
  const days = Math.floor(secs / 86400);
  return days <= 7 ? `Truy cập ${days} ngày trước` : 'Truy cập hơn một tuần trước';
}

function stopPresence() {
  clearInterval(presence.timer);
  presence.thread = null;
  presence.seen = null;
}

function watchPresence(t) {
  stopPresence();
  if (t.kind !== 'user') return;
  const id = t.id;
  presence.thread = id;
  const ask = async () => {
    try {
      const seen = await api(`/threads/${encodeURIComponent(id)}/presence`);
      if (presence.thread !== id) return;
      presence.seen = seen;
    } catch {
      // A nicety: without it the line stays empty.
    }
    if (presence.thread === id) renderChatSub();
  };
  ask();
  presence.timer = setInterval(() => {
    if (document.visibilityState === 'visible') ask();
  }, 60000);
}

/** The line under the open conversation's name. */
function renderChatSub() {
  const t = state.threads.get(state.current);
  const sub = $('chat-sub');
  if (!t) return;
  const typing = state.typing.get(t.id);
  const seen = presence.thread === t.id ? presence.seen : null;
  const text = presenceText(seen);
  sub.classList.toggle('typing', Boolean(typing));
  sub.classList.toggle('online', !typing && text === 'Vừa truy cập');
  if (typing) sub.textContent = typingText(t, typing);
  else sub.textContent = t.kind === 'group' ? membersText(t) : text;
}

function stopTyping(threadId) {
  const typing = state.typing.get(threadId);
  if (!typing) return;
  clearTimeout(typing.timer);
  state.typing.delete(threadId);
  renderThreads();
  if (threadId === state.current) renderChatSub();
}

/** A `typing` event: show it for a few seconds. */
function onTyping(p) {
  if (!p || !p.thread_id) return;
  const old = state.typing.get(p.thread_id);
  if (old) clearTimeout(old.timer);
  const timer = setTimeout(() => stopTyping(p.thread_id), TYPING_SHOWN_MS);
  state.typing.set(p.thread_id, { name: p.name || '', user: p.user_id, timer });
  renderThreads();
  if (p.thread_id === state.current) renderChatSub();
}

/** Tell the gateway we are typing, at most every few seconds per thread. */
function sendTyping() {
  const id = state.current;
  if (!id || !composerText().trim()) return;
  const now = Date.now();
  if (now - (state.typingSent.get(id) || 0) < TYPING_SEND_MS) return;
  state.typingSent.set(id, now);
  api(`/threads/${encodeURIComponent(id)}/typing`, { method: 'POST' }).catch(() => {});
}

// ----------------------------------------------------------------- composer

const input = $('input');

function autosize() {
  input.style.height = 'auto';
  input.style.height = `${Math.min(input.scrollHeight, 160)}px`;
  // A scrollbar only once the text is taller than the box can grow.
  input.style.overflowY = input.scrollHeight > 160 ? 'auto' : 'hidden';
}

// ------------------------------------------------------------------ mentions
//
// In a group, "@" followed by a few letters lists the members; picking one
// writes "@Name" and remembers who it is, so the message names them.

const mention = { members: new Map(), shown: [], active: 0 };

/** The "@…" being typed right before the cursor, in a group. */
function mentionQuery() {
  const t = state.threads.get(state.current);
  if (!t || t.kind !== 'group' || input.selectionStart !== input.selectionEnd) return null;
  const before = input.value.slice(0, input.selectionStart);
  const found = /(^|\s)@([^\s@]{0,30})$/.exec(before);
  if (!found) return null;
  return { start: input.selectionStart - found[2].length - 1, text: found[2] };
}

/** Members of a group, asked once per page load. */
function groupMembers(id) {
  if (!mention.members.has(id)) {
    const asked = api(`/threads/${encodeURIComponent(id)}/members`)
      .then((data) => data.members || [])
      .catch(() => {
        mention.members.delete(id);
        return [];
      });
    mention.members.set(id, asked);
  }
  return mention.members.get(id);
}

function closeMentionBox() {
  $('mention-box').hidden = true;
  mention.shown = [];
}

async function updateMentionBox() {
  const q = mentionQuery();
  if (!q) {
    closeMentionBox();
    return;
  }
  const id = state.current;
  const members = await groupMembers(id);
  const now = mentionQuery();
  if (state.current !== id || !now || now.text !== q.text) return;
  const me = state.status && state.status.uid;
  const wanted = fold(q.text);
  const list = members
    .filter((m) => m.uid !== me && (m.name || '').trim() && fold(m.name).includes(wanted))
    .slice(0, 8)
    .map((m) => ({ uid: m.uid, name: m.name.trim(), avatar: m.avatar }));
  if (fold('tat ca all').includes(wanted)) list.unshift({ uid: '-1', name: 'Tất cả', avatar: '', note: 'Nhắc cả nhóm' });
  mention.shown = list;
  mention.active = 0;
  renderMentionBox();
}

function renderMentionBox() {
  const box = $('mention-box');
  box.replaceChildren();
  if (!mention.shown.length) {
    box.hidden = true;
    return;
  }
  mention.shown.forEach((m, i) => {
    const item = el('div', i === mention.active ? 'mention-item active' : 'mention-item');
    item.setAttribute('role', 'option');
    const picture = m.uid === '-1' ? el('div', 'avatar', '@') : avatarBox(`zalo:u:${m.uid}`, m.name, m.avatar);
    item.append(picture, el('span', 'name', m.name));
    if (m.note) item.append(el('span', 'note', m.note));
    // mousedown, so the box does not lose the cursor before the click lands.
    item.addEventListener('mousedown', (e) => {
      e.preventDefault();
      pickMention(m);
    });
    box.append(item);
  });
  box.hidden = false;
}

function pickMention(m) {
  const q = mentionQuery();
  if (!q) return;
  const label = `@${m.name}`;
  input.setRangeText(`${label} `, q.start, input.selectionStart, 'end');
  state.mentions.push({ uid: m.uid, label });
  closeMentionBox();
  autosize();
  updateSendButton();
  input.focus();
}

/** Keys while the list is open; true when the key was for the list. */
function mentionKey(e) {
  if ($('mention-box').hidden || !mention.shown.length) return false;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    const n = mention.shown.length;
    mention.active = (mention.active + (e.key === 'ArrowDown' ? 1 : n - 1)) % n;
    renderMentionBox();
  } else if (e.key === 'Enter' || e.key === 'Tab') {
    pickMention(mention.shown[mention.active]);
  } else if (e.key === 'Escape') {
    closeMentionBox();
  } else {
    return false;
  }
  e.preventDefault();
  e.stopPropagation();
  return true;
}

/** Where the chosen names stand in the text to send (each "@Name" once). */
function draftMentions(text) {
  const taken = [];
  const out = [];
  for (const m of state.mentions) {
    let at = text.indexOf(m.label);
    while (at !== -1 && taken.some(([s, e]) => at < e && at + m.label.length > s)) {
      at = text.indexOf(m.label, at + 1);
    }
    if (at === -1) continue;
    taken.push([at, at + m.label.length]);
    out.push({ uid: m.uid, pos: at, len: m.label.length });
  }
  return out.sort((a, b) => a.pos - b.pos);
}

// ------------------------------------------------------------- formatting
//
// "Aa" turns the box into a rich editor with a toolbar, as Zalo PC does:
// bold, italic, underline, strike, Zalo's four colours, two sizes, lists.
// What is sent is the plain text and its runs (`styles`, UTF-16 units).

const rich = $('rich');
const format = { on: false, range: null };
const BLOCK_TAGS = new Set(['DIV', 'P', 'LI', 'UL', 'OL', 'BLOCKQUOTE', 'H1', 'H2', 'H3', 'H4', 'H5', 'H6', 'PRE']);

/** `#db342e`, `rgb(219, 52, 46)` → `db342e`; '' when not a colour. */
function hexColor(css) {
  const c = (css || '').trim().toLowerCase();
  let m = /^#([0-9a-f]{6})$/.exec(c);
  if (m) return m[1];
  m = /^#([0-9a-f]{3})$/.exec(c);
  if (m) return [...m[1]].map((x) => x + x).join('');
  m = /^rgba?\((\d+),\s*(\d+),\s*(\d+)/.exec(c);
  return m ? m.slice(1, 4).map((n) => Number(n).toString(16).padStart(2, '0')).join('') : '';
}

/** The formats an element adds to those it is inside. */
function formatsOf(node, outer) {
  const f = { ...outer };
  const tag = node.tagName;
  if (tag === 'B' || tag === 'STRONG') f.b = true;
  if (tag === 'I' || tag === 'EM') f.i = true;
  if (tag === 'U') f.u = true;
  if (tag === 'S' || tag === 'STRIKE' || tag === 'DEL') f.s = true;
  if (tag === 'FONT') {
    if (node.hasAttribute('color')) f.color = hexColor(node.getAttribute('color'));
    const size = Number(node.getAttribute('size'));
    if (size) f.size = size <= 2 ? 'f_13' : size >= 4 ? 'f_18' : '';
  }
  const st = node.style;
  if (st) {
    if (st.fontWeight === 'bold' || Number(st.fontWeight) >= 600) f.b = true;
    if (st.fontStyle === 'italic') f.i = true;
    const deco = `${st.textDecorationLine || ''} ${st.textDecoration || ''}`;
    if (deco.includes('underline')) f.u = true;
    if (deco.includes('line-through')) f.s = true;
    if (st.color) f.color = hexColor(st.color);
  }
  return f;
}

/** Merge runs of the same code that touch or overlap. */
function mergeRuns(runs) {
  const out = [];
  const byCode = new Map();
  for (const r of runs) {
    if (!byCode.has(r.st)) byCode.set(r.st, []);
    byCode.get(r.st).push(r);
  }
  for (const [st, list] of byCode) {
    list.sort((a, b) => a.start - b.start);
    let cur = null;
    for (const r of list) {
      if (cur && r.start <= cur.start + cur.len) {
        cur.len = Math.max(cur.len, r.start + r.len - cur.start);
      } else {
        if (cur) out.push(cur);
        cur = { start: r.start, len: r.len, st };
      }
    }
    if (cur) out.push(cur);
  }
  return out.sort((a, b) => a.start - b.start || a.st.localeCompare(b.st));
}

/**
 * The rich box as Zalo takes it: `{ text, styles }`, the text trimmed and
 * the runs moved and cut to match. Colours outside Zalo's four are left out
 * (the "default colour" button paints the text's own colour).
 */
function richValue() {
  let text = '';
  const runs = [];
  const add = (st, start, len) => { if (len > 0) runs.push({ start, len, st }); };
  const newline = () => { if (text && !text.endsWith('\n')) text += '\n'; };
  const walk = (node, f, list) => {
    for (const child of node.childNodes) {
      if (child.nodeType === Node.TEXT_NODE) {
        const part = child.nodeValue.replace(/\u00a0/g, ' ').replace(/\u200b/g, '');
        if (!part) continue;
        const start = text.length;
        text += part;
        for (const key of ['b', 'i', 'u', 's']) if (f[key]) add(key, start, part.length);
        if (f.color && PALETTE.has(f.color)) add(`c_${f.color}`, start, part.length);
        if (f.size) add(f.size, start, part.length);
      } else if (child.nodeType === Node.ELEMENT_NODE) {
        if (child.tagName === 'BR') {
          text += '\n';
          continue;
        }
        const block = BLOCK_TAGS.has(child.tagName);
        const inList = child.tagName === 'UL' ? 'lst_1' : child.tagName === 'OL' ? 'lst_2' : list;
        if (block) newline();
        const start = text.length;
        walk(child, formatsOf(child, f), inList);
        if (child.tagName === 'LI' && inList) add(inList, start, text.replace(/\n+$/, '').length - start);
        if (block) newline();
      }
    }
  };
  walk(rich, {}, null);
  const lead = text.length - text.trimStart().length;
  const kept = text.trim();
  const styles = mergeRuns(runs
    .map((r) => {
      const start = Math.max(0, r.start - lead);
      const end = Math.min(kept.length, r.start + r.len - lead);
      return { start, len: end - start, st: r.st };
    })
    .filter((r) => r.len > 0));
  return { text: kept, styles };
}

/** What the box holds now, whichever box is in use. */
function composerText() {
  return format.on ? richValue().text : input.value;
}

function focusComposer() {
  if (!format.on) {
    input.focus();
    return;
  }
  // Keep the selection when it is in the box already (a toolbar click), else
  // go back to where it last was, or to the end.
  const sel = getSelection();
  const current = sel.rangeCount && rich.contains(sel.anchorNode) ? sel.getRangeAt(0).cloneRange() : null;
  rich.focus();
  let range = current;
  if (!range && format.range && rich.contains(format.range.startContainer)) range = format.range;
  if (!range) {
    range = document.createRange();
    range.selectNodeContents(rich);
    range.collapse(false);
  }
  sel.removeAllRanges();
  sel.addRange(range);
}

/** Turn the rich box on (the text so far moves in) or off (back to plain text). */
function setFormat(on) {
  if (on === format.on) return;
  closeMentionBox();
  const btn = $('format-btn');
  btn.classList.toggle('on', on);
  btn.setAttribute('aria-pressed', String(on));
  $('format-bar').hidden = !on;
  if (on) {
    rich.textContent = input.value;
    input.value = '';
    autosize();
    input.hidden = true;
    rich.hidden = false;
    format.on = true;
    format.range = null;
    try { document.execCommand('styleWithCSS', false, false); } catch (_) { /* old engines */ }
  } else {
    const { text } = richValue();
    format.on = false;
    rich.replaceChildren();
    rich.hidden = true;
    input.hidden = false;
    input.value = text;
    autosize();
  }
  updateSendButton();
  focusComposer();
}

/** The selection in the box as character counts from its start. */
function caretOffsets() {
  const sel = getSelection();
  if (!sel.rangeCount || !rich.contains(sel.anchorNode)) return null;
  const r = sel.getRangeAt(0);
  const upTo = (node, offset) => {
    const before = document.createRange();
    before.selectNodeContents(rich);
    before.setEnd(node, offset);
    return before.toString().length;
  };
  return [upTo(r.startContainer, r.startOffset), upTo(r.endContainer, r.endOffset)];
}

/** Put the selection back at character counts from `caretOffsets`. */
function setCaretOffsets(at) {
  if (!at) return;
  const walker = document.createTreeWalker(rich, NodeFilter.SHOW_TEXT);
  let pos = 0;
  let start = null;
  let end = null;
  for (let n = walker.nextNode(); n; n = walker.nextNode()) {
    const len = n.nodeValue.length;
    if (!start && at[0] <= pos + len) start = [n, at[0] - pos];
    if (!end && at[1] <= pos + len) end = [n, at[1] - pos];
    if (start && end) break;
    pos += len;
  }
  if (!start || !end) return;
  const range = document.createRange();
  range.setStart(...start);
  range.setEnd(...end);
  const sel = getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
}

/** Light up the toolbar buttons that apply where the cursor is. */
function renderFormatState() {
  if (!format.on) return;
  for (const b of $('format-bar').querySelectorAll('button[data-cmd]')) {
    let on = false;
    try { on = document.queryCommandState(b.dataset.cmd); } catch (_) { on = false; }
    b.classList.toggle('on', on);
  }
}

$('format-btn').addEventListener('click', () => setFormat(!format.on));
// Pressing a toolbar button must not take the cursor out of the box.
$('format-bar').addEventListener('mousedown', (e) => {
  if (e.target.closest('button')) e.preventDefault();
});
$('format-bar').addEventListener('click', (e) => {
  const b = e.target.closest('button');
  if (!b || !format.on) return;
  focusComposer();
  if (b.dataset.cmd) {
    // Making a list puts the cursor at the start of the line; keep it where it was.
    const at = b.dataset.cmd.startsWith('insert') ? caretOffsets() : null;
    document.execCommand(b.dataset.cmd, false, null);
    setCaretOffsets(at);
  } else if ('color' in b.dataset) {
    document.execCommand('foreColor', false, b.dataset.color || getComputedStyle(rich).color);
  } else if (b.dataset.size) {
    document.execCommand('fontSize', false, b.dataset.size);
  }
  renderFormatState();
  updateSendButton();
});
document.addEventListener('selectionchange', () => {
  if (!format.on) return;
  const sel = getSelection();
  if (sel.rangeCount && rich.contains(sel.anchorNode)) {
    format.range = sel.getRangeAt(0).cloneRange();
    renderFormatState();
  }
});
rich.addEventListener('input', () => {
  if (!rich.textContent && rich.querySelector('br') && rich.childNodes.length === 1) rich.replaceChildren();
  updateSendButton();
  sendTyping();
});
rich.addEventListener('keydown', (e) => {
  // Enter starts a new line here (lists need it); Ctrl+Enter sends.
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey) && !e.isComposing) {
    e.preventDefault();
    $('composer').requestSubmit();
  }
});
// Only plain text comes in from outside, so what is sent is what Zalo can draw.
rich.addEventListener('paste', (e) => {
  const files = [...((e.clipboardData && e.clipboardData.files) || [])];
  e.preventDefault();
  if (files.length) {
    sendFiles(files);
    return;
  }
  const text = e.clipboardData ? e.clipboardData.getData('text/plain') : '';
  if (text) document.execCommand('insertText', false, text);
});
rich.addEventListener('drop', (e) => {
  if (carriesFiles(e)) return;
  e.preventDefault();
  const text = e.dataTransfer ? e.dataTransfer.getData('text/plain') : '';
  if (text) document.execCommand('insertText', false, text);
});

// --------------------------------------------------------------- marks
//
// "!" marks the next message as important or urgent ("Đánh dấu tin nhắn").

const mark = { level: 0 };

function setMark(level) {
  mark.level = level;
  const btn = $('mark-btn');
  btn.dataset.level = String(level);
  btn.classList.toggle('on', level > 0);
  const chip = $('mark-chip');
  chip.hidden = !level;
  chip.textContent = level ? `${URGENCY_TEXT[level]} ×` : '';
  chip.classList.toggle('urgent', level === 2);
}

$('mark-btn').addEventListener('click', (e) => {
  e.stopPropagation();
  openMenu($('mark-btn'), [
    ['Thường', () => setMark(0)],
    ['Quan trọng', () => setMark(1), null, 'Người nhận thấy nhãn "Quan trọng"'],
    ['Khẩn cấp', () => setMark(2), 'danger', 'Người nhận thấy nhãn "Khẩn cấp"'],
  ], 'Đánh dấu tin nhắn');
});
$('mark-chip').addEventListener('click', () => {
  setMark(0);
  focusComposer();
});

/** An empty box turns the send button into a quick 👍, as in Zalo. */
function updateSendButton() {
  const empty = !composerText().trim();
  const send = $('send');
  send.classList.toggle('like', empty);
  send.textContent = empty ? '👍' : 'Gửi';
  send.title = empty ? 'Gửi 👍' : 'Gửi';
}

input.addEventListener('input', () => {
  autosize();
  updateSendButton();
  sendTyping();
  updateMentionBox();
});
input.addEventListener('blur', () => setTimeout(closeMentionBox, 150));
input.addEventListener('keydown', (e) => {
  if (mentionKey(e)) return;
  // Enter sends; Shift+Enter is a new line; never send while an IME is composing.
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing && e.keyCode !== 229) {
    e.preventDefault();
    $('composer').requestSubmit();
  }
});

$('composer').addEventListener('submit', async (e) => {
  e.preventDefault();
  const formatted = format.on;
  const drawn = formatted ? richValue() : { text: input.value.trim(), styles: [] };
  let text = drawn.text;
  // The 👍 button sends a thumbs up; Enter in an empty box sends nothing.
  if (!text && e.submitter === $('send')) text = '👍';
  const id = state.current;
  if (!text || !id) return;
  // Kept to put back if the send fails.
  const saved = formatted ? [...rich.childNodes].map((n) => n.cloneNode(true)) : null;
  if (formatted) rich.replaceChildren();
  else input.value = '';
  autosize();
  updateSendButton();
  $('send').disabled = true;
  const body = { text };
  const replyTo = state.replyTo;
  if (replyTo && replyTo.thread_id === id) body.reply_to = replyTo.seq;
  const named = draftMentions(text);
  if (named.length) body.mentions = named;
  if (drawn.styles.length && text === drawn.text) body.styles = drawn.styles;
  const level = mark.level;
  if (level) body.urgency = level;
  closeMentionBox();
  try {
    const message = await api(`/threads/${encodeURIComponent(id)}/messages`, { json: body });
    state.mentions = [];
    if (state.replyTo === replyTo) cancelReply();
    if (mark.level === level) setMark(0);
    onMessage({ message, thread: null });
  } catch (err) {
    if (formatted) {
      if (!rich.childNodes.length && format.on) rich.replaceChildren(...saved);
    } else if (text !== '👍' || input.value) {
      input.value = text;
    }
    autosize();
    updateSendButton();
    handleError(err, 'Không gửi được: ');
  } finally {
    $('send').disabled = false;
    focusComposer();
  }
});

// ------------------------------------------------------- emoji and stickers
//
// One panel above the composer shows either emoji (put into the text) or
// Zalo's stickers (sent at once). Recently used ones are kept on this device.

const EMOJI_GROUPS = [
  ['🕘', 'Dùng gần đây', ''],
  ['😀', 'Mặt cười', '😀 😃 😄 😁 😆 😅 🤣 😂 🙂 🙃 😉 😊 😇 🥰 😍 🤩 😘 😗 😚 😋 😛 😜 🤪 😝 🤑 🤗 🤭 🤫 🤔 🤐 🤨 😐 😑 😶 😏 😒 🙄 😬 😌 😔 😪 🤤 😴 😷 🤒 🤕 🤢 🤮 🥵 🥶 🥴 😵 🤯 🤠 🥳 😎 🤓 🧐 😕 😟 🙁 😮 😯 😲 😳 🥺 😦 😧 😨 😰 😥 😢 😭 😱 😖 😣 😞 😓 😩 😫 🥱 😤 😡 😠 🤬 😈 👿 💀 💩 🤡 👻 👽 🤖 😺 😸 😹 😻 😼 😽 🙀 😿 😾'],
  ['👋', 'Cử chỉ', '👍 👎 👌 🤌 🤏 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ ✋ 🤚 🖐️ 🖖 👋 👏 🙌 👐 🤲 🤝 🙏 ✍️ 💪 🫶 🙋 🙆 🙅 🤷 🤦 🙇 💁 🧑‍💻 👨‍👩‍👧 👶 🧒 👦 👧 🧑 👨 👩 🧓 👴 👵'],
  ['❤️', 'Trái tim', '❤️ 🧡 💛 💚 💙 💜 🤎 🖤 🤍 💔 ❣️ 💕 💞 💓 💗 💖 💘 💝 💟 💋 💯 💢 💥 💫 💦 💨 🔥 ✨ ⭐ 🌟 💤 ❗ ❓ ✅ ❌ 🆗 🆒 🆕 🔴 🟢 🔵'],
  ['🐶', 'Động vật và thiên nhiên', '🐶 🐱 🐭 🐹 🐰 🦊 🐻 🐼 🐨 🐯 🦁 🐮 🐷 🐸 🐵 🙈 🙉 🙊 🐔 🐧 🐦 🐤 🦆 🦉 🐴 🦄 🐝 🦋 🐌 🐞 🐢 🐍 🐙 🐠 🐬 🐳 🦈 🌸 🌹 🌺 🌻 🌼 🌷 🌱 🌿 🍀 🍁 🌈 ☀️ 🌙 ⛅ 🌧️ ⛄ 🌊'],
  ['🍜', 'Đồ ăn', '🍎 🍊 🍋 🍌 🍉 🍇 🍓 🍒 🍑 🥭 🍍 🥥 🥑 🍅 🌶️ 🌽 🥕 🍞 🧀 🍳 🍗 🍖 🍔 🍟 🍕 🌭 🥪 🌮 🍜 🍲 🍛 🍣 🍱 🥟 🍤 🍙 🍚 🍰 🎂 🍩 🍪 🍫 🍬 🍭 🍦 ☕ 🍵 🧋 🥤 🍺 🍻 🥂 🍷'],
  ['⚽', 'Hoạt động và đồ vật', '⚽ 🏀 🏐 🏓 🏸 🎾 🏆 🥇 🎮 🎲 🎯 🎤 🎧 🎸 🎹 🎉 🎊 🎈 🎁 🧧 🏮 🎄 🎆 📱 💻 📷 🎥 📞 ⏰ 💡 📚 ✏️ 📌 📎 ✂️ 🔑 🏠 🚗 🏍️ 🚲 ✈️ 🚀 💰 💵 💎 🛒 🏖️ ⛺'],
];

const RECENT_EMOJI_KEY = 'zetachat.recentEmoji';
const RECENT_STICKERS_KEY = 'zetachat.recentStickers';

function loadList(key) {
  try {
    const list = JSON.parse(localStorage.getItem(key) || '[]');
    return Array.isArray(list) ? list : [];
  } catch {
    return [];
  }
}

function saveList(key, list) {
  try {
    localStorage.setItem(key, JSON.stringify(list));
  } catch {
    // Private windows may refuse: recent items are a convenience.
  }
}

function rememberEmoji(emoji) {
  const list = loadList(RECENT_EMOJI_KEY).filter((e) => e !== emoji);
  list.unshift(emoji);
  saveList(RECENT_EMOJI_KEY, list.slice(0, 32));
}

function rememberSticker(s) {
  const list = loadList(RECENT_STICKERS_KEY).filter((x) => x.id !== s.id);
  list.unshift({ id: s.id, cate_id: s.cate_id, type: s.type, url: s.url });
  saveList(RECENT_STICKERS_KEY, list.slice(0, 24));
}

let panelKind = null; // 'emoji' | 'stickers' | null

/** Change the layout under the messages, staying at the newest one if there. */
function keepBottom(change) {
  const box = $('messages');
  const atBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 80;
  change();
  if (atBottom) box.scrollTop = box.scrollHeight;
}

function closePanel() {
  panelKind = null;
  $('panel').hidden = true;
  $('panel').replaceChildren();
  $('open-emoji').classList.remove('on');
  $('open-stickers').classList.remove('on');
}

function openPanel(kind) {
  panelKind = kind;
  keepBottom(() => {
    $('panel').hidden = false;
  });
  $('open-emoji').classList.toggle('on', kind === 'emoji');
  $('open-stickers').classList.toggle('on', kind === 'stickers');
  if (kind === 'emoji') renderEmoji(loadList(RECENT_EMOJI_KEY).length ? 0 : 1);
  else renderStickerPanel();
}

function togglePanel(kind) {
  if (panelKind === kind) closePanel();
  else openPanel(kind);
}

/** Put text where the cursor is in the message box. */
function insertAtCursor(text) {
  if (format.on) {
    focusComposer();
    document.execCommand('insertText', false, text);
    updateSendButton();
    return;
  }
  const start = input.selectionStart ?? input.value.length;
  const end = input.selectionEnd ?? input.value.length;
  input.setRangeText(text, start, end, 'end');
  autosize();
  updateSendButton();
}

function renderEmoji(group) {
  const box = $('panel');
  box.replaceChildren();
  const tabs = el('div', 'panel-tabs');
  EMOJI_GROUPS.forEach(([icon, label], i) => {
    const tab = el('button', i === group ? 'panel-tab on' : 'panel-tab', icon);
    tab.type = 'button';
    tab.title = label;
    tab.setAttribute('aria-label', label);
    tab.addEventListener('click', () => renderEmoji(i));
    tabs.append(tab);
  });
  const grid = el('div', 'emoji-grid');
  const list = group === 0 ? loadList(RECENT_EMOJI_KEY) : EMOJI_GROUPS[group][2].split(' ');
  if (!list.length) grid.append(el('div', 'panel-empty', 'Chưa dùng biểu tượng nào'));
  for (const emoji of list) {
    const cell = el('button', 'emoji-cell', emoji);
    cell.type = 'button';
    cell.addEventListener('click', () => {
      insertAtCursor(emoji);
      rememberEmoji(emoji);
    });
    grid.append(cell);
  }
  box.append(tabs, grid);
}

const stickers = {
  packs: null, // the store's packs, once loaded
  pick: 'recent', // pack shown: 'recent' or a pack id
  cache: new Map(), // pack id -> its stickers
  query: '',
  timer: 0,
};

function renderStickerPanel() {
  const box = $('panel');
  box.replaceChildren();
  const search = el('input', 'field panel-search');
  search.type = 'search';
  search.placeholder = 'Tìm nhãn dán: haha, cảm ơn, yêu…';
  search.value = stickers.query;
  search.addEventListener('input', () => {
    stickers.query = search.value;
    clearTimeout(stickers.timer);
    const q = search.value.trim();
    if (!q) {
      showPick();
      return;
    }
    stickers.timer = setTimeout(() => searchStickers(q), 300);
  });
  const grid = el('div', 'sticker-grid');
  grid.id = 'sticker-grid';
  const strip = el('div', 'pack-strip');
  strip.id = 'pack-strip';
  box.append(search, grid, strip);
  renderPackStrip();
  if (stickers.query.trim()) searchStickers(stickers.query.trim());
  else showPick();
  if (!stickers.packs) loadPacks();
}

async function loadPacks() {
  try {
    stickers.packs = await api('/stickers/packs');
  } catch (err) {
    stickers.packs = [];
    handleError(err, 'Không tải được kho nhãn dán: ');
  }
  if (panelKind === 'stickers') {
    renderPackStrip();
    if (stickers.pick === 'recent' && !loadList(RECENT_STICKERS_KEY).length && stickers.packs.length) {
      stickers.pick = stickers.packs[0].id;
      showPick();
    }
  }
}

function renderPackStrip() {
  const strip = $('pack-strip');
  if (!strip) return;
  strip.replaceChildren();
  const recent = el('button', 'pack-btn', '🕘');
  recent.type = 'button';
  recent.title = 'Dùng gần đây';
  recent.dataset.pick = 'recent';
  recent.addEventListener('click', () => choosePack('recent'));
  strip.append(recent);
  for (const p of stickers.packs || []) {
    const button = el('button', 'pack-btn');
    button.type = 'button';
    button.title = p.name;
    button.dataset.pick = String(p.id);
    if (p.icon) {
      const img = new Image();
      img.alt = p.name;
      img.loading = 'lazy';
      img.src = mediaSrc(p.icon);
      img.addEventListener('error', () => img.replaceWith(initials(p.name)));
      button.append(img);
    } else {
      button.append(initials(p.name));
    }
    button.addEventListener('click', () => choosePack(p.id));
    strip.append(button);
  }
  markPick();
}

function markPick() {
  for (const b of document.querySelectorAll('#pack-strip .pack-btn')) {
    b.classList.toggle('on', !stickers.query.trim() && b.dataset.pick === String(stickers.pick));
  }
}

function choosePack(pick) {
  stickers.pick = pick;
  stickers.query = '';
  const search = document.querySelector('#panel .panel-search');
  if (search) search.value = '';
  showPick();
}

async function showPick() {
  markPick();
  const pick = stickers.pick;
  if (pick === 'recent') {
    showStickers(loadList(RECENT_STICKERS_KEY), 'Chưa gửi nhãn dán nào: chọn một gói bên dưới hoặc tìm theo từ khóa.');
    return;
  }
  let list = stickers.cache.get(pick);
  if (!list) {
    showStickers(null);
    try {
      list = await api(`/stickers/packs/${pick}`);
      stickers.cache.set(pick, list);
    } catch (err) {
      if (stickers.pick === pick) showStickers([], 'Không tải được gói này.');
      handleError(err, 'Không tải được nhãn dán: ');
      return;
    }
  }
  if (stickers.pick === pick && !stickers.query.trim()) showStickers(list, 'Gói này trống.');
}

async function searchStickers(q) {
  markPick();
  showStickers(null);
  try {
    const list = await api(`/stickers/search?q=${encodeURIComponent(q)}`);
    if (stickers.query.trim() === q) showStickers(list, `Không có nhãn dán cho “${q}”.`);
  } catch (err) {
    if (stickers.query.trim() === q) showStickers([], 'Không tìm được nhãn dán.');
    handleError(err, 'Không tìm được nhãn dán: ');
  }
}

/** Fill the grid: a list of stickers, or "loading" for null. */
function showStickers(list, emptyText) {
  const grid = $('sticker-grid');
  if (!grid) return;
  grid.replaceChildren();
  grid.scrollTop = 0;
  if (!list) {
    grid.append(el('div', 'panel-empty', 'Đang tải…'));
    return;
  }
  if (!list.length) {
    grid.append(el('div', 'panel-empty', emptyText));
    return;
  }
  for (const s of list) {
    const cell = el('button', 'sticker-cell');
    cell.type = 'button';
    cell.title = 'Gửi nhãn dán';
    const img = new Image();
    img.className = 'sticker-pick';
    img.alt = 'Nhãn dán';
    img.loading = 'lazy';
    img.src = mediaSrc(s.url);
    img.addEventListener('error', () => cell.remove());
    // In the picker a sticker moves while the pointer is on it.
    cell.addEventListener('mouseenter', () => {
      const still = cell.querySelector('img.sticker-pick');
      if (still) stickerMotion(Number(s.id), (m) => animateSticker(still, m));
    }, { once: true });
    cell.append(img);
    cell.addEventListener('click', () => sendSticker(s));
    grid.append(cell);
  }
}

async function sendSticker(s) {
  const id = state.current;
  if (!id) return;
  rememberSticker(s);
  try {
    const message = await api(`/threads/${encodeURIComponent(id)}/stickers`, {
      json: { id: s.id, cate_id: s.cate_id, type: s.type },
    });
    onMessage({ message, thread: null });
  } catch (err) {
    handleError(err, 'Không gửi được nhãn dán: ');
  }
}

$('open-emoji').addEventListener('click', () => togglePanel('emoji'));
$('open-stickers').addEventListener('click', () => togglePanel('stickers'));

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

/** Follow an upload this page did not start (a forwarded file, another tab). */
function adoptUpload(u) {
  serverUploads.set(u.id, newerUpload(serverUploads.get(u.id), u));
  const tracked = [...uploads.values()].some((entry) => entry.id === u.id);
  if (!tracked && (UPLOAD_RANK[u.state] ?? 3) < 3) {
    uploads.set(u.id, {
      key: u.id, id: u.id, thread_id: u.thread_id, name: u.name, size: u.size,
      phase: 'server', loaded: u.size, xhr: null, localError: '',
    });
  }
  scheduleUploads();
}

/** An `upload` event from the gateway. */
function onUpload(payload) {
  const u = payload && payload.upload;
  if (!u) return;
  if (![...uploads.values()].some((entry) => entry.id === u.id)) adoptUpload(u);
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
  let title = t.kind === 'group' ? `${threadName(t)} · ${m.sender_name || 'Thành viên'}` : threadName(t);
  if (mentionsMe(m)) title += ' nhắc đến bạn';
  if (m.urgency) title = `[${URGENCY_TEXT[m.urgency]}] ${title}`;
  const n = new Notification(title, { body: displayText(m.text), tag: m.thread_id, icon: '/icon.svg' });
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
    members: 0,
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
  if (!m.outgoing) stopTyping(m.thread_id);
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
  if (!wasValid && s.session === 'valid') {
    loadThreads().catch(handleError);
    loadFriends();
  }
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
    loadFriends();
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
  es.addEventListener('message_deleted', (e) => {
    track(e);
    const p = JSON.parse(e.data);
    removeMessage(p.thread_id, p.seq, p.thread);
  });
  es.addEventListener('status', (e) => { track(e); onStatus(JSON.parse(e.data)); });
  // Upload progress and typing: not in the event log, so no id to track.
  es.addEventListener('upload', (e) => onUpload(JSON.parse(e.data)));
  es.addEventListener('typing', (e) => onTyping(JSON.parse(e.data)));
  // History was loaded on the gateway: reload lists instead of replaying messages.
  es.addEventListener('threads_changed', (e) => { track(e); resync(); });
  es.addEventListener('thread_read', (e) => {
    track(e);
    const p = JSON.parse(e.data);
    const t = state.threads.get(p.thread_id);
    if (t) {
      // Read here or on another device; some may still be unread there.
      t.unread = typeof p.unread === 'number' ? p.unread : 0;
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

/** The account's picture at the top of the list. */
function renderMe(s) {
  const key = `${s.uid}|${s.avatar}|${s.display_name}`;
  if (key === state.meKey) return;
  state.meKey = key;
  const box = avatarBox(`zalo:u:${s.uid || 'me'}`, s.display_name || 'Tôi', s.avatar);
  box.id = 'me-avatar';
  box.classList.add('me-avatar');
  box.title = s.display_name ? `Tài khoản Zalo: ${s.display_name}` : 'Tài khoản Zalo';
  $('me-avatar').replaceWith(box);
}

function renderStatus(s) {
  state.status = s;
  renderMe(s);
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
  loadFriends();
  loadRequests();
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
