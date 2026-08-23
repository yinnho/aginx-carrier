// aginx webui 前端——移植自 opencarrier 桌面客户端（vanilla JS、DOM 直写、无框架）。
// 与源实现的两处差异：数据源从 Tauri invoke 换成同源 fetch；聊天流从
// Tauri 事件（chat://delta…）换成直接读 fetch SSE 流。
// DOM 即真相：流式增量只改 bubble.textContent，没有响应式层，没有代理陷阱。

const state = {
  agents: [],
  current: null,
  history: new Map(), // agent name -> [{role:'user'|'agent', text}]
  streaming: false,
  brain: null,
  senderId:
    localStorage.getItem('aginx_sender') ||
    'w' + Math.random().toString(36).slice(2, 10),
};
localStorage.setItem('aginx_sender', state.senderId);

let editMods = {}; // 设置弹窗打开期间的模态编辑副本

const $ = (id) => document.getElementById(id);
const contactList = $('contact-list');

const escapeHtml = (s) =>
  String(s).replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]));

async function apiGet(path) {
  const r = await fetch(path);
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  return r.json();
}
async function apiSend(path, method, body) {
  const r = await fetch(path, {
    method,
    headers: { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await r.json().catch(() => ({}));
  if (!r.ok) throw new Error(data.error || `HTTP ${r.status}`);
  return data;
}

function setConn(ok) {
  $('conn-dot').classList.toggle('off', !ok);
}

// ---------- 分身列表 ----------

async function loadAgents() {
  try {
    const data = await apiGet('/api/agents');
    state.agents = data.agents || [];
    setConn(true);
  } catch (e) {
    setConn(false);
  }
  renderContacts();
}

function previewText(a) {
  const h = state.history.get(a.name);
  const last = h && h.length ? h[h.length - 1] : null;
  if (last) return (last.role === 'user' ? '我：' : '') + last.text;
  return a.description || `${a.name} · ${a.model || ''}`;
}

function renderContacts() {
  contactList.innerHTML = '';
  const q = ($('search-input').value || '').trim().toLowerCase();
  const sorted = [...state.agents].sort((a, b) => {
    const ra = a.state === 'Running' ? 1 : 0;
    const rb = b.state === 'Running' ? 1 : 0;
    if (ra !== rb) return rb - ra;
    return (b.last_active || '').localeCompare(a.last_active || '');
  });
  for (const a of sorted) {
    const hay = `${a.display_name} ${a.name} ${a.description}`.toLowerCase();
    if (q && !hay.includes(q)) continue;
    const el = document.createElement('div');
    el.className = 'chat_item' + (state.current && state.current.id === a.id ? ' active' : '');
    el.innerHTML =
      `<div class="avatar">${escapeHtml(a.emoji || (a.display_name || a.name).slice(0, 1))}` +
      `<span class="presence${a.state === 'Running' ? ' on' : ''}"></span></div>` +
      `<div class="meta"><div class="name">${escapeHtml(a.display_name || a.name)}</div>` +
      `<div class="msg">${escapeHtml(previewText(a).slice(0, 40))}</div></div>`;
    el.onclick = () => selectAgent(a);
    contactList.appendChild(el);
  }
  if (!contactList.children.length) {
    contactList.innerHTML = `<div class="empty-list">${q ? '无匹配分身' : '还没有分身<br>点 ⟳ 刷新或用克隆大师创建'}</div>`;
  }
}

// ---------- 会话 ----------

async function selectAgent(a) {
  if (state.streaming) return;
  state.current = a;
  renderContacts();
  $('chat-empty').classList.add('hidden');
  $('chat-active').classList.remove('hidden');
  $('chat-avatar').textContent = a.emoji || (a.display_name || a.name).slice(0, 1);
  $('chat-name').textContent = a.display_name || a.name;
  $('chat-sub').textContent = `${a.name} · ${a.model || '?'} · ${a.state === 'Running' ? '在线' : '离线'}`;
  setStatus('', false);
  $('chat-body').innerHTML = '';
  state.history.delete(a.name);
  try {
    const data = await apiGet(
      `/api/history?agent=${encodeURIComponent(a.name)}&sender=${encodeURIComponent(state.senderId)}`
    );
    const msgs = (data.messages || []).map((m) => ({
      role: m.role === 'user' ? 'user' : 'agent',
      text: m.text || '',
    }));
    state.history.set(a.name, msgs);
    for (const m of msgs) appendBubble(m.role, m.text);
  } catch (e) {
    /* 历史拉取失败不阻塞聊天 */
  }
  scrollChat();
  $('msg-input').focus();
}

function pushHist(name, m) {
  if (!state.history.has(name)) state.history.set(name, []);
  state.history.get(name).push(m);
}

// ---------- 消息渲染（DOM 直写） ----------

function scrollChat() {
  const b = $('chat-body');
  b.scrollTop = b.scrollHeight;
}

function appendBubble(role, text) {
  const row = document.createElement('div');
  row.className = `row ${role}`;
  const avatar = document.createElement('div');
  avatar.className = 'avatar';
  avatar.textContent = role === 'user' ? '我' : (state.current && state.current.emoji) || '🧬';
  const bubble = document.createElement('div');
  bubble.className = 'bubble';
  bubble.textContent = text || '';
  row.appendChild(avatar);
  row.appendChild(bubble);
  $('chat-body').appendChild(row);
  scrollChat();
  return bubble;
}

function appendThink() {
  const d = document.createElement('details');
  d.className = 'think';
  d.innerHTML = '<summary>💭 思考过程</summary><pre></pre>';
  $('chat-body').appendChild(d);
  return d.querySelector('pre');
}

function appendToolChip(name, preview, isError) {
  const el = document.createElement('div');
  el.className = 'tool-chip' + (isError ? ' is_error' : '');
  el.textContent = `🔧 ${name} ${isError ? '失败' : '完成'}`;
  if (preview) {
    const s = document.createElement('span');
    s.className = 'tool-preview';
    s.textContent = preview.slice(0, 120);
    el.appendChild(s);
  }
  $('chat-body').appendChild(el);
  scrollChat();
}

function setStatus(text, spin) {
  $('status-text').textContent = text;
  $('spinner').classList.toggle('hidden', !spin);
}

// ---------- 发送（SSE 直读） ----------

async function sendMessage() {
  const input = $('msg-input');
  const text = input.value.trim();
  if (!text || !state.current || state.streaming) return;
  state.streaming = true;
  $('send-btn').disabled = true;
  input.value = '';
  const agent = state.current;

  pushHist(agent.name, { role: 'user', text });
  appendBubble('user', text);
  renderContacts();

  const bubble = appendBubble('agent', '');
  bubble.classList.add('streaming');
  let thinkPre = null;
  let acc = '';
  setStatus('思考中…', true);

  try {
    const resp = await fetch(`/api/chat/${encodeURIComponent(agent.name)}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ message: text, sender_id: state.senderId }),
    });
    if (!resp.ok || !resp.body) {
      const t = await resp.text().catch(() => '');
      throw new Error(`HTTP ${resp.status} ${t}`.slice(0, 200));
    }
    const reader = resp.body.getReader();
    const dec = new TextDecoder();
    let buf = '';
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i;
      while ((i = buf.indexOf('\n\n')) >= 0) {
        const chunk = buf.slice(0, i);
        buf = buf.slice(i + 2);
        for (const line of chunk.split('\n')) {
          if (!line.startsWith('data:')) continue;
          let ev;
          try { ev = JSON.parse(line.slice(5)); } catch { continue; }
          if (ev.type === 'delta') {
            acc += ev.text || '';
            bubble.textContent = acc;
            scrollChat();
          } else if (ev.type === 'thinking') {
            if (!thinkPre) thinkPre = appendThink();
            thinkPre.textContent += ev.text || '';
          } else if (ev.type === 'tool') {
            appendToolChip(ev.name, ev.preview, ev.is_error);
          } else if (ev.type === 'done') {
            if (!acc && ev.response) { acc = ev.response; bubble.textContent = acc; }
            setStatus(`完成 · ${ev.tokens ?? '?'} tok · ${ev.iterations ?? '?'} 轮`, false);
          } else if (ev.type === 'error') {
            throw new Error(ev.message || 'agent 轮失败');
          }
        }
      }
    }
    bubble.classList.remove('streaming');
    if (!acc) {
      // 整轮无文本（无声轮）——不占一个空气泡
      bubble.parentElement.remove();
      setStatus('完成（无文本回复）', false);
    } else {
      pushHist(agent.name, { role: 'agent', text: acc });
    }
  } catch (e) {
    bubble.classList.remove('streaming');
    bubble.classList.add('bubble-error');
    bubble.textContent = `⚠ ${e.message}`;
    setStatus('失败', false);
  }
  state.streaming = false;
  $('send-btn').disabled = false;
  $('msg-input').focus();
  renderContacts();
}

// ---------- 设置（大脑） ----------

function brainMissing() {
  return !state.brain || !state.brain.base_url;
}

async function loadBrain() {
  try {
    state.brain = await apiGet('/api/brain');
  } catch (e) {
    state.brain = null;
  }
  $('banner').classList.toggle('hidden', !brainMissing());
}

function openSettings() {
  const b = state.brain || { base_url: '', api_key_env: 'AGINXBRAIN_API_KEY', default_modality: 'chat', modalities: {} };
  $('cfg-base-url').value = b.base_url || '';
  $('cfg-key-env').value = b.api_key_env || '';
  $('cfg-key-value').value = '';
  $('cfg-default-modality').value = b.default_modality || '';
  editMods = JSON.parse(JSON.stringify(b.modalities || {}));
  renderMods();
  setHint('', '');
  $('settings-dialog').showModal();
}

function renderMods() {
  const box = $('mod-list');
  box.innerHTML = '';
  for (const [name, m] of Object.entries(editMods)) {
    const row = document.createElement('div');
    row.className = 'mod-row';
    row.innerHTML =
      `<span class="mod-name" title="${escapeHtml(name)}">${escapeHtml(name)}</span>` +
      `<input type="text" data-name="${escapeHtml(name)}" value="${escapeHtml((m && m.description) || '')}">` +
      `<button type="button" class="mod-del" data-name="${escapeHtml(name)}" title="删除">✕</button>`;
    box.appendChild(row);
  }
  box.querySelectorAll('.mod-row input').forEach((inp) => {
    inp.oninput = () => { editMods[inp.dataset.name].description = inp.value; };
  });
  box.querySelectorAll('.mod-del').forEach((btn) => {
    btn.onclick = () => { delete editMods[btn.dataset.name]; renderMods(); };
  });
}

function setHint(text, cls) {
  const h = $('settings-hint');
  h.textContent = text;
  h.className = 'hint' + (cls ? ' ' + cls : '');
}

async function saveSettings() {
  const cfg = {
    base_url: $('cfg-base-url').value.trim(),
    api_key_env: $('cfg-key-env').value.trim() || 'AGINXBRAIN_API_KEY',
    default_modality: $('cfg-default-modality').value.trim() || 'chat',
    modalities: editMods,
  };
  const keyValue = $('cfg-key-value').value.trim();
  setHint('保存中…', '');
  try {
    await apiSend('/api/brain', 'PUT', cfg);
    if (keyValue) {
      await apiSend('/api/key', 'POST', { name: cfg.api_key_env, value: keyValue });
    }
    state.brain = cfg;
    $('banner').classList.add('hidden');
    setHint('已保存' + (keyValue ? '，Key 已写入 .env' : ''), 'ok');
    setTimeout(() => $('settings-dialog').close(), 500);
  } catch (e) {
    setHint(`保存失败：${e.message}`, 'err');
  }
}

// ---------- 事件接线 + 启动 ----------

function wireEvents() {
  $('send-btn').onclick = sendMessage;
  $('msg-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      sendMessage();
    }
  });
  $('refresh-btn').onclick = loadAgents;
  $('settings-btn').onclick = openSettings;
  $('banner').onclick = openSettings;
  $('search-input').addEventListener('input', renderContacts);
  $('settings-save').onclick = saveSettings;
  $('settings-cancel').onclick = () => $('settings-dialog').close();
  $('mod-add-btn').onclick = () => {
    const name = $('mod-add-name').value.trim();
    if (!name || editMods[name]) return;
    editMods[name] = { description: $('mod-add-desc').value.trim() };
    $('mod-add-name').value = '';
    $('mod-add-desc').value = '';
    renderMods();
  };
}

async function boot() {
  wireEvents();
  await Promise.all([loadAgents(), loadBrain()]);
  if (brainMissing()) openSettings();
}

boot();
