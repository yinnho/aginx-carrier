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
  view: 'chat', // 'chat' | 'market' | 'detail' | 'tools'
  market: { q: '', page: 1, templates: [], hasMore: false, keyOk: true, hubEnv: '', hubUrl: '' },
  installing: false,
  tools: [], // 本机网关 agent（/api/tools）；网关不可达时只含已添加的 stale 项
  remoteContacts: [], // 远程联系人（/api/tools remote——store 元数据，零网络）
  gateways: [], // 远程网关地址簿（/api/gateways，bound=已配对绑定）
  remoteAgents: {}, // target -> 对方网关 agent 列表（点开网关时拉取）
  gwNeedsBind: {}, // target -> true：私有网关待配对（探活/listAgents 被拒时置位）
  activeGateway: null,
  localGateway: null, // 本机网关 {target,url}（出码端用；null=未配置藏分享钮）
  pendingScanAgent: null, // {target,agent}：扫码/手输带了 /分身名 后缀，绑定后自动加联系人直达会话
  // 第八刀同意流：访客申请轮询 / 主人访客面板轮询
  consent: null, // {target, requestId, agent, name, timer}
  visitorsTarget: null,
  visitorsTimer: null,
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
  // 首启默认落在「我」——系统身份，开箱即聊
  if (!state.current && state.agents.length) {
    const me = state.agents.find(a => a.name === 'me');
    if (me) selectAgent(me);
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
  // 「我」（系统身份）永远置顶，其余按 Running + 最近活跃排
  const sorted = [...state.agents].sort((a, b) => {
    if (a.name === 'me') return -1;
    if (b.name === 'me') return 1;
    const ra = a.state === 'Running' ? 1 : 0;
    const rb = b.state === 'Running' ? 1 : 0;
    if (ra !== rb) return rb - ra;
    return (b.last_active || '').localeCompare(a.last_active || '');
  });
  for (const a of sorted) {
    const hay = `${a.display_name} ${a.name} ${a.description}`.toLowerCase();
    if (q && !hay.includes(q)) continue;
    const isMe = a.name === 'me';
    const el = document.createElement('div');
    el.className = 'chat_item' + (state.current && state.current.id === a.id ? ' active' : '');
    el.innerHTML =
      `<div class="avatar">${escapeHtml(a.emoji || (isMe ? '👤' : (a.display_name || a.name).slice(0, 1)))}` +
      `<span class="presence${a.state === 'Running' ? ' on' : ''}"></span></div>` +
      `<div class="meta"><div class="name">${escapeHtml(a.display_name || a.name)}${isMe ? ' <span style="font-size:11px;color:var(--muted,#888)">系统身份</span>' : ''}</div>` +
      `<div class="msg">${escapeHtml(previewText(a).slice(0, 40))}</div></div>`;
    el.onclick = () => selectAgent(a);
    contactList.appendChild(el);
  }
  // 网关工具（第三刀）：已添加的追加在分身后，🖥 徽标
  for (const t of state.tools) {
    if (!t.added) continue;
    const hay = `${t.name} ${t.id} ${t.description}`.toLowerCase();
    if (q && !hay.includes(q)) continue;
    const isCur = state.current && state.current.kind === 'gateway' && state.current.id === t.id;
    const el = document.createElement('div');
    el.className = 'chat_item' + (isCur ? ' active' : '');
    const prev = previewText({ name: t.id, description: `网关 · ${t.agent_type || 'agent'}`, model: '' });
    el.innerHTML =
      `<div class="avatar">🖥<span class="presence on"></span></div>` +
      `<div class="meta"><div class="name">${escapeHtml(t.name || t.id)}</div>` +
      `<div class="msg">${escapeHtml(prev.slice(0, 40))}</div></div>`;
    el.onclick = () => selectTool(t);
    contactList.appendChild(el);
  }
  // 远程联系人（第五刀）：别人家网关的分身，👤 徽标
  for (const c of state.remoteContacts) {
    const hay = `${c.name} ${c.id} ${c.description} ${c.gateway}`.toLowerCase();
    if (q && !hay.includes(q)) continue;
    const isCur = state.current && state.current.kind === 'gateway' && state.current.id === c.id;
    const el = document.createElement('div');
    el.className = 'chat_item' + (isCur ? ' active' : '');
    const prev = previewText({ name: c.id, description: `远程 · ${c.gateway}`, model: '' });
    el.innerHTML =
      `<div class="avatar">👤<span class="presence on"></span></div>` +
      `<div class="meta"><div class="name">${escapeHtml(c.name || c.id)}</div>` +
      `<div class="msg">${escapeHtml(prev.slice(0, 40))}</div></div>`;
    el.onclick = () => selectTool(c);
    contactList.appendChild(el);
  }
  if (!contactList.children.length) {
    contactList.innerHTML = `<div class="empty-list">${q ? '无匹配联系人' : '还没有联系人<br>点 ⟳ 刷新 · ＋ 添加（制作/扫码/市场）'}</div>`;
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
  $('sessions-btn').classList.add('hidden');
  updateShareBtn();
  setStatus('', false);
  await applyHistory(a.name);
  scrollChat();
  $('msg-input').focus();
}

/// 拉取并渲染一个联系人的历史（分身按 name，网关工具按 id——本地流水，
/// 会话列表另走网关台账）。
async function applyHistory(key) {
  $('chat-body').innerHTML = '';
  state.history.delete(key);
  try {
    const url = `/api/history?agent=${encodeURIComponent(key)}&sender=${encodeURIComponent(state.senderId)}`;
    const data = await apiGet(url);
    const msgs = (data.messages || []).map((m) => ({
      role: m.role === 'user' ? 'user' : 'agent',
      text: m.text || '',
    }));
    state.history.set(key, msgs);
    for (const m of msgs) appendBubble(m.role, m.text);
  } catch (e) {
    /* 历史拉取失败不阻塞聊天 */
  }
}

// ---------- 网关工具会话（第三刀批2：历史=网关台账，续接=回喂 sessionId） ----------

async function openSessions() {
  const cur = state.current;
  if (!cur || cur.kind !== 'gateway') return;
  await renderSessions();
  $('sessions-dialog').showModal();
}

async function renderSessions() {
  const cur = state.current;
  const list = $('sessions-list');
  let data;
  try {
    data = await apiGet(
      `/api/tool-sessions?agent=${encodeURIComponent(cur.id)}&sender=${encodeURIComponent(state.senderId)}`
    );
  } catch (e) {
    list.innerHTML = `<div class="cwd-row-item cwd-dim">加载失败：${escapeHtml(e.message)}</div>`;
    return;
  }
  const active = data.active_session_id || '';
  list.innerHTML = '';
  if (data.gateway_error) {
    list.innerHTML = `<div class="cwd-row-item cwd-dim">网关不可达：${escapeHtml(data.gateway_error)}</div>`;
    return;
  }
  for (const s of data.sessions || []) {
    const el = document.createElement('div');
    el.className = 'sess-item' + (s.sessionId === active ? ' sess-current' : '');
    const time = (s.lastTs || '').slice(0, 19).replace('T', ' ');
    el.innerHTML =
      `<div class="sess-cwd">${escapeHtml(s.title || s.sessionId)}${s.sessionId === active ? ' <span class="sess-mark">当前</span>' : ''}</div>` +
      `<div class="sess-meta">${s.turns ?? '?'} 轮 · ${escapeHtml(time)}</div>`;
    el.onclick = () => pickSession(s.sessionId);
    list.appendChild(el);
  }
  if (!list.children.length) {
    list.innerHTML = '<div class="cwd-row-item cwd-dim">（还没有历史会话——发过消息就会出现在这里）</div>';
  }
}

// 点选历史会话 → 通知后端切续接 id，下一轮 prompt 回喂该 id
async function pickSession(sessionId) {
  const cur = state.current;
  if (!cur || cur.kind !== 'gateway' || !sessionId) return;
  try {
    await apiSend(`/api/tools/${encodeURIComponent(cur.id)}/session`, 'POST', {
      sender_id: state.senderId,
      session_id: sessionId,
    });
    $('sessions-dialog').close();
    setStatus('已切到该会话，下一轮续接它', false);
  } catch (e) {
    alert(`切换失败：${e.message}`);
  }
}

// 新会话：清续接 id，下一轮不带 --resume 从头开
async function newSession() {
  const cur = state.current;
  if (!cur || cur.kind !== 'gateway') return;
  try {
    await apiSend(`/api/tools/${encodeURIComponent(cur.id)}/session`, 'POST', {
      sender_id: state.senderId,
      session_id: null,
    });
    $('sessions-dialog').close();
    setStatus('新会话已就绪，下一轮从头开始', false);
  } catch (e) {
    alert(`新建失败：${e.message}`);
  }
}

async function selectTool(t) {
  if (state.streaming) return;
  const emoji = t.gateway ? '👤' : '🖥';
  state.current = { kind: 'gateway', id: t.id, name: t.name || t.id, emoji };
  renderContacts();
  $('chat-empty').classList.add('hidden');
  $('chat-active').classList.remove('hidden');
  $('chat-avatar').textContent = emoji;
  $('chat-name').textContent = t.name || t.id;
  $('chat-sub').textContent = t.gateway
    ? `远程 · ${t.gateway} · ${t.agent_type || 'agent'}`
    : `网关 · ${t.agent_type || 'agent'} · ${t.id}`;
  $('sessions-btn').classList.remove('hidden');
  updateShareBtn();
  setStatus('', false);
  await applyHistory(t.id);
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
  const isTool = agent.kind === 'gateway';
  const key = isTool ? agent.id : agent.name; // 网关工具按 id 路由，分身按 name
  // 轮前快照：轮末 diff 出这轮新装的分身（制作分身闭环）
  const knownAgentNames = new Set(state.agents.map((a) => a.name));

  pushHist(key, { role: 'user', text });
  appendBubble('user', text);
  renderContacts();

  const bubble = appendBubble('agent', '');
  bubble.classList.add('streaming');
  let thinkPre = null;
  let acc = '';
  setStatus('思考中…', true);

  try {
    const resp = await fetch(`/api/chat/${encodeURIComponent(key)}`, {
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
            if (ev.cost_usd != null) {
              // claude 等带真金白银计量的轮（cost/duration）
              const dur = ev.duration_ms != null ? `${(ev.duration_ms / 1000).toFixed(1)}s` : '?';
              setStatus(`完成 · $${Number(ev.cost_usd).toFixed(4)} · ${dur}`, false);
            } else if (ev.duration_ms != null) {
              // 网关路径 tokens 字段=轮数（批2）；carrier 方言无定价 → 秒+轮数
              const dur = `${(ev.duration_ms / 1000).toFixed(1)}s`;
              setStatus(`完成 · ${dur} · ${ev.tokens ?? '?'} 轮`, false);
            } else {
              setStatus(`完成 · ${ev.tokens ?? '?'} tok · ${ev.iterations ?? '?'} 轮`, false);
            }
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
      pushHist(key, { role: 'agent', text: acc });
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
  // 制作分身闭环（补第七刀）：轮末重拉 /api/agents——clone-creator 用 clone_install
  // 装好的新分身（spawn_agent 进程内即时注册，无 DupHub 往返）直接出现在侧栏，免手动 ⟳；
  // 聊 clone-creator 时自动跳进新分身会话（对齐市场安装 installClone 的体验）。
  await loadAgents();
  if (!isTool && key === 'clone-creator') {
    const fresh = state.agents
      .filter((a) => !knownAgentNames.has(a.name))
      .sort((x, y) => (y.last_active || '').localeCompare(x.last_active || ''))[0];
    if (fresh) await selectAgent(fresh);
  }
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

// ---------- 装分身页（市场 / 权限预览 / 一键安装） ----------

function showView(v) {
  state.view = v;
  $('chat-pane').classList.toggle('hidden', v !== 'chat');
  $('market-pane').classList.toggle('hidden', !(v === 'market' || v === 'detail'));
  $('mkt-list').classList.toggle('hidden', v !== 'market');
  $('mkt-detail').classList.toggle('hidden', v !== 'detail');
  $('tools-pane').classList.toggle('hidden', v !== 'tools');
}

async function loadMarket(reset) {
  const m = state.market;
  if (reset) { m.page = 1; m.templates = []; }
  m.q = ($('mkt-search').value || '').trim();
  $('mkt-grid').innerHTML = '<div class="mkt-loading">加载中…</div>';
  try {
    const data = await apiGet(
      `/api/market?q=${encodeURIComponent(m.q)}&page=${m.page}`
    );
    m.templates = m.page === 1 ? data.templates || [] : m.templates.concat(data.templates || []);
    m.hasMore = !!data.has_more;
    m.keyOk = !!data.key_configured;
    m.hubEnv = data.api_key_env || 'OPENCLONE_HUB_KEY';
    m.hubUrl = data.hub_url || '';
    $('mkt-key-banner').classList.toggle('hidden', m.keyOk);
    $('mkt-key-banner').innerHTML = m.keyOk
      ? ''
      : `未配置 DupHub API key——列表可浏览，<a id="mkt-key-link" href="#">点此填写 key</a> 后才能预览与安装`;
    const link = $('mkt-key-link');
    if (link) link.onclick = openHubKey;
    renderMarket();
  } catch (e) {
    $('mkt-grid').innerHTML = `<div class="mkt-loading mkt-err">市场加载失败：${escapeHtml(e.message)}</div>`;
  }
}

function fmtSize(n) {
  if (!n || n <= 0) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

// hub 的 tags 可能是数组、JSON 字符串（实测形态）或逗号串——统一成数组
function toTags(v) {
  if (Array.isArray(v)) return v;
  if (typeof v === 'string' && v.trim()) {
    try {
      const parsed = JSON.parse(v);
      if (Array.isArray(parsed)) return parsed;
    } catch { /* 按逗号串处理 */ }
    return v.split(/[,，、]/).map((s) => s.trim()).filter(Boolean);
  }
  return [];
}

function renderMarket() {
  const grid = $('mkt-grid');
  grid.innerHTML = '';
  if (!state.market.templates.length) {
    grid.innerHTML = '<div class="mkt-loading">市场里没有匹配的分身</div>';
  }
  for (const t of state.market.templates) {
    const name = t.name || '';
    const card = document.createElement('div');
    card.className = 'mkt-card';
    const badges = [];
    if (t.price > 0) badges.push(`<span class="mkt-badge paid">付费</span>`);
    if (t.visibility === 'private') badges.push(`<span class="mkt-badge priv">私有</span>`);
    const tags = toTags(t.tags).slice(0, 4)
      .map((x) => `<span class="mkt-chip">${escapeHtml(String(x))}</span>`).join('');
    card.innerHTML =
      `<div class="mkt-card-name">${escapeHtml(t.display_name || name)}${badges.join('')}</div>` +
      `<div class="mkt-card-desc">${escapeHtml((t.description || '').slice(0, 90) || '（无描述）')}</div>` +
      `<div class="mkt-card-meta"><span class="mkt-card-id">${escapeHtml(name)}</span>` +
      `${t.author ? `<span>· ${escapeHtml(String(t.author))}</span>` : ''}` +
      `${t.download_count != null ? `<span>· ${escapeHtml(String(t.download_count))} 次安装</span>` : ''}</div>` +
      (tags ? `<div class="mkt-card-tags">${tags}</div>` : '');
    card.onclick = () => openMarketDetail(name);
    grid.appendChild(card);
  }
  $('mkt-more-btn').classList.toggle('hidden', !state.market.hasMore);
}

async function openMarketDetail(name) {
  showView('detail');
  const box = $('mkt-detail');
  box.innerHTML = '<div class="mkt-loading">拉取权限清单…</div>';
  let d;
  try {
    d = await apiGet(`/api/market/${encodeURIComponent(name)}/preview`);
  } catch (e) {
    box.innerHTML =
      `<header id="mkt-d-head"><button id="mkt-back" class="btn-ghost">← 返回市场</button></header>` +
      `<div class="mkt-loading mkt-err">拉取失败：${escapeHtml(e.message)}</div>`;
    $('mkt-back').onclick = () => { showView('market'); };
    return;
  }
  renderMarketDetail(d);
}

// 工具徽章按 max_level 着色：readonly 蓝 / write 橙 / execute 红 / dangerous 深红
const LEVEL_COLOR = {
  none: '#8a8f99', readonly: '#576b95', write: '#b26a00',
  execute: '#c0392b', dangerous: '#8e1b1b',
};

function levelChip(level) {
  const c = LEVEL_COLOR[level] || LEVEL_COLOR.none;
  return `<span class="mkt-badge" style="color:${c};border-color:${c}">${escapeHtml(level)}</span>`;
}

function renderMarketDetail(d) {
  const box = $('mkt-detail');
  const flows = d.flows || [];
  const errs = d.format_errors || [];
  const blocks = [];

  blocks.push(`<header id="mkt-d-head">
    <button id="mkt-back" class="btn-ghost">← 返回市场</button>
    <div class="mkt-title">${escapeHtml(d.display_name || d.name)} <span class="mkt-card-id">${escapeHtml(d.name)}</span></div>
  </header>`);

  const badges = [];
  if (d.price > 0) badges.push(`<span class="mkt-badge paid">付费${d.purchased ? '·已购' : ''}</span>`);
  if (d.visibility === 'private') badges.push(`<span class="mkt-badge priv">私有</span>`);
  if (d.installed) badges.push(`<span class="mkt-badge ok">已安装</span>`);
  if (d.name_valid === false) badges.push(`<span class="mkt-badge priv">名字不合法·装不了</span>`);
  blocks.push(
    `<div id="mkt-d-meta">
      ${badges.join('')}
      <span>版本 ${escapeHtml(String(d.latest_version || '?'))}</span>
      <span>${d.file_count} 个文件</span>
      <span>${fmtSize(d.total_bytes)}</span>
      ${d.author ? `<span>作者 ${escapeHtml(String(d.author))}</span>` : ''}
      ${toTags(d.tags).map((x) => `<span class="mkt-chip">${escapeHtml(String(x))}</span>`).join('')}
    </div>`
  );
  if (d.description) blocks.push(`<div id="mkt-d-desc">${escapeHtml(d.description)}</div>`);

  // 权限清单：每 flow 一张小卡
  if (flows.length) {
    const cards = flows.map((f) => {
      const tools = (f.tools || [])
        .map((t) => `<span class="mkt-badge tool" style="color:${LEVEL_COLOR[f.max_level] || LEVEL_COLOR.none}">${escapeHtml(t)}</span>`)
        .join('') || '<span class="mkt-dim">无工具</span>';
      const shell = (f.shell_allow || []).length
        ? `<div class="mkt-flow-shell"><div class="mkt-dim">shell 放行：</div>${f.shell_allow.map((s) => `<code>${escapeHtml(s)}</code>`).join('')}</div>`
        : '';
      const deny = (f.deny_tools || []).length
        ? `<div class="mkt-dim">禁用：${f.deny_tools.map((s) => `<code>${escapeHtml(s)}</code>`).join(' ')}</div>`
        : '';
      const marks = [
        f.privilege === 'system' ? '<span class="mkt-badge priv">system 特权</span>' : '',
        f.elevates ? '<span class="mkt-badge paid">shell 提权</span>' : '',
        f.entry === false ? '<span class="mkt-dim">非入口</span>' : '<span class="mkt-badge ok">入口</span>',
      ].join('');
      return `<div class="mkt-flow">
        <div class="mkt-flow-head"><b>${escapeHtml(f.name)}</b>${marks}${levelChip(f.max_level)}</div>
        <div class="mkt-dim">${escapeHtml((f.description || '').slice(0, 100) || '（无描述）')}</div>
        <div class="mkt-flow-tools">${tools}</div>
        ${shell}${deny}
      </div>`;
    }).join('');
    blocks.push(`<h4 class="mkt-sec">权限清单（${flows.length} 个 flow）</h4><div id="mkt-flows">${cards}</div>`);
  } else {
    blocks.push(`<h4 class="mkt-sec">权限清单</h4><div class="mkt-dim">这个分身没有声明任何 flow。</div>`);
  }

  if ((d.mcp_servers || []).length || (d.plugins || []).length) {
    blocks.push(
      `<h4 class="mkt-sec">外部连接</h4><div id="mkt-ext">` +
      (d.mcp_servers || []).map((s) => `<span class="mkt-chip">MCP · ${escapeHtml(s)}</span>`).join('') +
      (d.plugins || []).map((s) => `<span class="mkt-chip">插件 · ${escapeHtml(s)}</span>`).join('') +
      `</div>`
    );
  }

  if (errs.length) {
    blocks.push(
      `<h4 class="mkt-sec">格式校验不通过</h4><div id="mkt-warn">${errs.map((e) => `<div>⚠ ${escapeHtml(e)}</div>`).join('')}</div>`
    );
  }
  blocks.push(`<div class="mkt-note">安装后会自动附加 self-growth flow 与格式规范文件——实际内容会比上面多这两样，非 bug。</div>`);

  // 底栏：安装钮
  const blocked = errs.length > 0 || d.name_valid === false || (d.price > 0 && !d.purchased);
  const btnText = blocked
    ? (errs.length || d.name_valid === false ? '格式校验不通过，暂不可安装' : '需先在 DupHub 购买')
    : d.installed ? '重装（覆盖现有分身）' : '安装到本机';
  blocks.push(
    `<footer id="mkt-d-foot"><span id="mkt-install-status"></span>
      <button id="mkt-install-btn" class="btn-primary"${blocked || state.installing ? ' disabled' : ''}>${btnText}</button>
    </footer>`
  );

  box.innerHTML = blocks.join('');
  $('mkt-back').onclick = () => { showView('market'); };
  $('mkt-install-btn').onclick = () => installClone(d.name);
}

async function installClone(name) {
  const btn = $('mkt-install-btn');
  const status = $('mkt-install-status');
  state.installing = true;
  btn.disabled = true;
  btn.textContent = '拉取并安装中…';
  status.textContent = '';
  try {
    const r = await apiSend(`/api/market/${encodeURIComponent(name)}/install`, 'POST', {});
    status.textContent = '已安装，正在启动…';
    await loadAgents();
    const entry = state.agents.find((a) => a.name === r.name || a.name === name);
    showView('chat');
    if (entry) await selectAgent(entry);
  } catch (e) {
    status.textContent = `安装失败：${e.message}`;
    btn.disabled = false;
    btn.textContent = '重试安装';
  }
  state.installing = false;
}

function openHubKey() {
  $('hubkey-env-name').textContent = state.market.hubEnv || 'OPENCLONE_HUB_KEY';
  $('hubkey-value').value = '';
  const h = $('hubkey-hint');
  h.textContent = '';
  h.className = 'hint';
  $('hubkey-dialog').showModal();
}

async function saveHubKey() {
  const env = state.market.hubEnv || 'OPENCLONE_HUB_KEY';
  const value = $('hubkey-value').value.trim();
  const h = $('hubkey-hint');
  if (!value) { h.textContent = 'key 不能为空'; h.className = 'hint err'; return; }
  h.textContent = '保存中…';
  try {
    await apiSend('/api/key', 'POST', { name: env, value });
    h.textContent = '已保存';
    h.className = 'hint ok';
    setTimeout(() => { $('hubkey-dialog').close(); loadMarket(true); }, 400);
  } catch (e) {
    h.textContent = `保存失败：${e.message}`;
    h.className = 'hint err';
  }
}

// ---------- 接入本地工具页（第三刀：网关 agent 列表 / 添加 / 移除） ----------

async function loadTools() {
  let gwErr = '';
  try {
    const data = await apiGet('/api/tools');
    state.tools = data.tools || [];
    state.remoteContacts = data.remote || [];
    gwErr = data.gateway_error || '';
  } catch (e) {
    state.tools = [];
    state.remoteContacts = [];
    gwErr = e.message;
  }
  const banner = $('tools-banner');
  if (gwErr) {
    banner.textContent = `本机网关不可达：${gwErr}——已添加工具保留在联系人，恢复后可用（远程联系人不受影响）`;
    banner.classList.remove('hidden');
  } else {
    banner.classList.add('hidden');
  }
  renderTools();
  renderContacts();
}

// ---------- 远程网关地址���（第五刀：用别人家的分身走标准 agent:// 流程；
// 第六刀：私有网关配对码绑定） ----------

async function loadGateways() {
  try {
    const data = await apiGet('/api/gateways');
    state.gateways = data.gateways || [];
    // 已绑定的网关不再挂"待配对"标
    for (const g of state.gateways) {
      if (g.bound) delete state.gwNeedsBind[g.target];
    }
  } catch (e) {
    state.gateways = [];
  }
  renderGateways();
  // 只有一个网关时自动展开它的 agent 列表
  if (!state.activeGateway && state.gateways.length === 1) {
    await loadRemoteAgents(state.gateways[0].target);
  }
}

function renderGateways() {
  const list = $('gw-list');
  list.innerHTML = '';
  if (!state.gateways.length) {
    list.innerHTML = '<div class="cwd-row-item cwd-dim">还没有远程网关——输入对方 agent:// 地址连接</div>';
    return;
  }
  for (const g of state.gateways) {
    const chip = document.createElement('button');
    chip.type = 'button';
    chip.className = 'gw-chip' + (state.activeGateway === g.target ? ' on' : '');
    // 双槽身份（第九刀）：🔒=主人绑定 👤=访客授权，可并存
    const ownerB = g.owner_bound !== undefined ? g.owner_bound : (g.bound && g.role !== 'visitor');
    const visAuth = g.visitor_authorized !== undefined ? g.visitor_authorized : (g.bound && g.role === 'visitor');
    const marks = [];
    if (ownerB) marks.push('🔒');
    if (visAuth) marks.push('👤');
    if (!marks.length && state.gwNeedsBind && state.gwNeedsBind[g.target]) marks.push('⚠️');
    const mark = marks.join('');
    const tips = [];
    if (ownerB) tips.push(`主人绑定：${g.device_name || 'webui'}`);
    if (visAuth) tips.push('访客授权');
    chip.title = tips.length ? `${tips.join(' · ')}（${g.url}）` : g.url;
    const visBtn = ownerB
      ? ` <span class="gw-vis" title="访客管理（同意/吊销）">👥</span>` : '';
    chip.innerHTML = `${mark} ${escapeHtml(g.target)}${visBtn} <span class="gw-del" title="移除网关（连带联系人）">✕</span>`;
    chip.onclick = (e) => {
      if (e.target.classList.contains('gw-del')) removeGateway(g.target);
      else if (e.target.classList.contains('gw-vis')) openVisitors(g.target);
      else loadRemoteAgents(g.target);
    };
    list.appendChild(chip);
  }
}

async function addGateway() {
  const url = $('gw-url').value.trim();
  if (!url) return;
  const btn = $('gw-add-btn');
  btn.disabled = true;
  btn.textContent = '连接中…';
  try {
    const d = await apiSend('/api/gateways', 'POST', { url });
    state.activeGateway = d.target;
    state.remoteAgents[d.target] = d.agents || [];
    if (d.needs_bind) {
      state.gwNeedsBind[d.target] = true;
    }
    $('gw-url').value = '';
    await loadGateways();
    renderRemoteAgents();
  } catch (e) {
    alert(`连接失败：${e.message}`);
  }
  btn.disabled = false;
  btn.textContent = '连接';
}

async function loadRemoteAgents(target) {
  state.activeGateway = target;
  renderGateways();
  $('remote-grid').innerHTML = '<div class="mkt-loading">拉取对方网关 agent 列表…</div>';
  try {
    const d = await apiGet(`/api/remote-agents?gateway=${encodeURIComponent(target)}`);
    if (d.needs_bind) {
      // 网关活着但私有且未绑定（或 token 失效）——走配对码绑定
      state.gwNeedsBind[target] = true;
      state.remoteAgents[target] = [];
      renderRemoteAgents();
      return;
    }
    if (d.gateway_error) {
      $('remote-grid').innerHTML = `<div class="mkt-loading mkt-err">对方网关不可达：${escapeHtml(d.gateway_error)}</div>`;
      return;
    }
    delete state.gwNeedsBind[target];
    state.remoteAgents[target] = d.agents || [];
  } catch (e) {
    $('remote-grid').innerHTML = `<div class="mkt-loading mkt-err">拉取失败：${escapeHtml(e.message)}</div>`;
    return;
  }
  renderRemoteAgents();
}

/// 私有网关的准入行（renderRemoteAgents 的 needs_bind 分支渲染）：
/// 主人设备走配对码绑定；访客走申请访问（第八刀同意流）。
function renderBindRow(grid, target) {
  const row = document.createElement('div');
  row.className = 'gw-bind-row';
  row.innerHTML =
    `<div class="gw-bind-tip">⚠️ 网关 ${escapeHtml(target)} 是私有的。你是这台网关的主人设备？在对方机器上执行 <code>aginx pair</code> 拿配对码绑定；你只是访客？发访问申请等主人同意。</div>` +
    `<div class="gw-bind-controls">` +
    `<input id="pair-code" type="text" placeholder="6 位配对码（主人）" autocomplete="off">` +
    `<button id="gw-bind-btn" class="btn-primary" type="button">绑定</button>` +
    `<button id="gw-consent-btn" class="btn-ghost" type="button">申请访问…</button>` +
    `</div>`;
  grid.appendChild(row);
  const input = row.querySelector('#pair-code');
  const doBind = () => bindGateway(target, input.value.trim());
  row.querySelector('#gw-bind-btn').onclick = doBind;
  const pend = state.pendingScanAgent;
  row.querySelector('#gw-consent-btn').onclick = () =>
    openConsent(target, pend && pend.target === target ? pend.agent : null);
  input.onkeydown = (e) => { if (e.key === 'Enter') doBind(); };
  input.focus();
}

async function bindGateway(target, pairCode) {
  if (!pairCode) return;
  const btn = $('gw-bind-btn');
  if (btn) { btn.disabled = true; btn.textContent = '绑定中…'; }
  try {
    const d = await apiSend(`/api/gateways/${encodeURIComponent(target)}/bind`, 'POST', { pair_code: pairCode });
    delete state.gwNeedsBind[target];
    state.remoteAgents[target] = d.agents || [];
    await loadGateways();
    // 扫码带 /分身名 后缀的客服码：绑定完直达会话，不停在工具页
    const pend = state.pendingScanAgent;
    if (pend && pend.target === target) {
      await autoAddAgent(target, pend.agent, d.agents);
      return;
    }
    renderRemoteAgents();
  } catch (e) {
    alert(`配对失败：${e.message}`);
    if (btn) { btn.disabled = false; btn.textContent = '绑定'; }
  }
}

// ---------- 同意流（第八刀）：访客申请 → 主人同意 → per-访客 token ----------

function openConsent(target, agent) {
  state.consent = { target, agent: agent || null, requestId: null, name: '', timer: null };
  $('consent-gw').textContent = `网关 ${target}`;
  const row = $('consent-agent-row');
  if (agent) { $('consent-agent').textContent = agent; row.classList.remove('hidden'); }
  else row.classList.add('hidden');
  $('consent-name').value = localStorage.getItem('consent-name') || '';
  const st = $('consent-status');
  st.classList.add('hidden');
  st.classList.remove('mkt-err');
  $('consent-submit').disabled = false;
  $('consent-submit').textContent = '发申请';
  $('consent-name').disabled = false;
  $('consent-dialog').showModal();
  $('consent-name').focus();
}

function stopConsentPoll() {
  if (state.consent && state.consent.timer) { clearTimeout(state.consent.timer); state.consent.timer = null; }
}

async function consentSubmit() {
  const c = state.consent;
  if (!c) return;
  const name = $('consent-name').value.trim();
  if (!name) {
    const st = $('consent-status');
    st.textContent = '先填名字——对方同意时会看到';
    st.classList.add('mkt-err');
    st.classList.remove('hidden');
    return;
  }
  localStorage.setItem('consent-name', name);
  c.name = name;
  const btn = $('consent-submit');
  btn.disabled = true;
  btn.textContent = '发送中…';
  try {
    const d = await apiSend(`/api/gateways/${encodeURIComponent(c.target)}/request-access`, 'POST',
      { name, agent: c.agent || undefined });
    if (d.status === 'approved') {
      // 网关开了自动通过（客服码即扫即用）——token 已入地址簿，直达
      await consentApproved(c.target, d.agents || []);
      return;
    }
    c.requestId = d.requestId;
    const st = $('consent-status');
    st.textContent = '⏳ 已发申请，等主人同意…（申请 24 小时内有效）';
    st.classList.remove('mkt-err');
    st.classList.remove('hidden');
    $('consent-name').disabled = true;
    btn.textContent = '等待中…';
    consentPoll();
  } catch (e) {
    btn.disabled = false;
    btn.textContent = '发申请';
    const st = $('consent-status');
    st.textContent = `申请失败：${e.message}`;
    st.classList.add('mkt-err');
    st.classList.remove('hidden');
  }
}

async function consentPoll() {
  const c = state.consent;
  if (!c || !c.requestId || !$('consent-dialog').open) { stopConsentPoll(); return; }
  try {
    const d = await apiGet(`/api/gateways/${encodeURIComponent(c.target)}/access-status?request=${encodeURIComponent(c.requestId)}&name=${encodeURIComponent(c.name)}`);
    if (d.status === 'approved') {
      stopConsentPoll();
      await consentApproved(c.target, d.agents || []);
      return;
    }
    if (d.status === 'notFound') {
      stopConsentPoll();
      const st = $('consent-status');
      st.textContent = '❌ 申请没通过（被拒绝或已过期）。可以重新发一次，或联系网关主人。';
      st.classList.add('mkt-err');
      st.classList.remove('hidden');
      $('consent-submit').disabled = false;
      $('consent-submit').textContent = '再发一次';
      $('consent-name').disabled = false;
      c.requestId = null;
      return;
    }
    // pending：继续等
  } catch (e) { /* 网关/网络抖动，下轮再试 */ }
  if (state.consent === c) c.timer = setTimeout(consentPoll, 3000);
}

/// 申请通过的收尾（自动通过/取票两条路共用）：刷新地址簿（role=visitor）、
/// 有客服码后缀则直达会话，否则展示该网关 agent 列表。
async function consentApproved(target, agents) {
  $('consent-dialog').close();
  state.consent = null;
  delete state.gwNeedsBind[target];
  state.remoteAgents[target] = agents;
  state.activeGateway = target;
  await loadGateways();
  showView('tools');
  renderRemoteAgents();
  const pend = state.pendingScanAgent;
  if (pend && pend.target === target && agents.length) {
    await autoAddAgent(target, pend.agent, agents);
  }
}

// ---------- 访客管理面板（第八刀，主人侧） ----------

function openVisitors(target) {
  state.visitorsTarget = target;
  $('vis-gw').textContent = target;
  $('vis-requests').innerHTML = '<div class="hint">拉取中…</div>';
  $('vis-clients').innerHTML = '';
  $('visitors-dialog').showModal();
  refreshVisitors();
  stopVisitorsPoll();
  state.visitorsTimer = setInterval(refreshVisitors, 5000);
}

function stopVisitorsPoll() {
  if (state.visitorsTimer) { clearInterval(state.visitorsTimer); state.visitorsTimer = null; }
}

async function refreshVisitors() {
  const t = state.visitorsTarget;
  if (!t || !$('visitors-dialog').open) { stopVisitorsPoll(); return; }
  try {
    const d = await apiGet(`/api/gateways/${encodeURIComponent(t)}/visitors`);
    renderVisitorPanels(d);
  } catch (e) {
    $('vis-requests').innerHTML = `<div class="hint mkt-err">${escapeHtml(e.message)}</div>`;
  }
}

function renderVisitorPanels(d) {
  const reqBox = $('vis-requests');
  const reqs = d.requests || [];
  if (!reqs.length) {
    reqBox.innerHTML = '<div class="hint cwd-dim">暂无待审申请</div>';
  } else {
    reqBox.innerHTML = '';
    for (const r of reqs) {
      const row = document.createElement('div');
      row.className = 'vis-row';
      row.innerHTML =
        `<div class="vis-main"><b>${escapeHtml(r.clientName)}</b>` +
        `${r.agent ? ` <span class="mkt-badge">想用 ${escapeHtml(r.agent)}</span>` : ' <span class="mkt-badge">全部分身</span>'}` +
        `${r.approved ? ' <span class="mkt-badge">已批准 · 待访客领取</span>' : ''}` +
        `<span class="cwd-dim"> · ${new Date(r.createdAt * 1000).toLocaleString()}</span></div>` +
        `<div class="vis-ops"></div>`;
      const ops = row.querySelector('.vis-ops');
      const okBtn = document.createElement('button');
      okBtn.className = 'btn-primary'; okBtn.textContent = '同意';
      okBtn.onclick = () => visitorAction('approve', { requestId: r.requestId });
      const noBtn = document.createElement('button');
      noBtn.className = 'btn-ghost'; noBtn.textContent = '拒绝';
      noBtn.onclick = () => visitorAction('reject', { requestId: r.requestId });
      // 已批准待领取：不可再同意（票已铸），只留拒绝（= 撤单收回）
      ops.append(...(r.approved ? [noBtn] : [okBtn, noBtn]));
      reqBox.appendChild(row);
    }
  }
  const cliBox = $('vis-clients');
  const clients = d.clients || [];
  if (!clients.length) {
    cliBox.innerHTML = '<div class="hint cwd-dim">还没有授权访客</div>';
  } else {
    cliBox.innerHTML = '';
    for (const c of clients) {
      const row = document.createElement('div');
      row.className = 'vis-row';
      const scope = c.allowedAgents.length ? c.allowedAgents.join(', ') : '全部分身';
      const exp = c.expiresAt ? ` · ${new Date(c.expiresAt * 1000).toLocaleDateString()} 到期` : '';
      row.innerHTML =
        `<div class="vis-main"><b>${escapeHtml(c.name)}</b>` +
        ` <span class="mkt-badge">${escapeHtml(scope)}</span>` +
        `<span class="cwd-dim"> · ${new Date(c.createdAt * 1000).toLocaleDateString()} 授权${exp}</span></div>` +
        `<div class="vis-ops"></div>`;
      const ops = row.querySelector('.vis-ops');
      const btn = document.createElement('button');
      btn.className = 'btn-ghost'; btn.textContent = '吊销';
      btn.onclick = () => visitorAction('revoke', { clientId: c.id });
      ops.appendChild(btn);
      cliBox.appendChild(row);
    }
  }
}

async function visitorAction(kind, body) {
  const t = state.visitorsTarget;
  if (!t) return;
  if (kind === 'revoke' && !confirm('吊销后对方 token 立即失效，需要重新申请。确定？')) return;
  try {
    const d = await apiSend(`/api/gateways/${encodeURIComponent(t)}/visitors/${kind}`, 'POST', body);
    if (d.panels) renderVisitorPanels(d.panels);
  } catch (e) {
    alert(`操作失败：${e.message}`);
  }
}

function renderRemoteAgents() {
  const grid = $('remote-grid');
  grid.innerHTML = '';
  const t = state.activeGateway;
  if (!t || !state.remoteAgents[t]) return;
  const agents = state.remoteAgents[t];
  if (state.gwNeedsBind && state.gwNeedsBind[t]) {
    renderBindRow(grid, t);
    return;
  }
  if (!agents.length) {
    grid.innerHTML = '<div class="mkt-loading">对方网关没有可列出的 agent</div>';
    return;
  }
  for (const a of agents) {
    const card = document.createElement('div');
    card.className = 'mkt-card';
    card.innerHTML =
      `<div class="mkt-card-name">👤 ${escapeHtml(a.name || a.id)}` +
      `<span class="mkt-badge">${escapeHtml(a.agent_type || 'agent')}</span>` +
      `${a.added ? '<span class="mkt-badge ok">已添加</span>' : ''}</div>` +
      `<div class="mkt-card-desc">${escapeHtml((a.description || '').slice(0, 90) || '（无描述）')}</div>` +
      `<div class="mkt-card-meta"><span class="mkt-card-id">${escapeHtml(a.id)}</span>` +
      `<span>· 算力在 ${escapeHtml(t)} 家 · 会话凭 sessionId 续接</span></div>`;
    const btn = document.createElement('button');
    btn.className = a.added ? 'btn-ghost' : 'btn-primary';
    btn.textContent = a.added ? '移除（清会话）' : '添加到联系人';
    btn.onclick = async () => {
      if (a.added && !confirm(`移除 ${a.name || a.id}？本地聊天记录会一并清掉（对方网关的会话台账不受影响）。`)) return;
      btn.disabled = true;
      try {
        await apiSend(`/api/tools/${encodeURIComponent(a.contact_id)}/${a.added ? 'remove' : 'add'}`, 'POST',
          a.added ? {} : { name: a.name, description: a.description, agent_type: a.agent_type, gateway: t });
        a.added = !a.added;
      } catch (e) {
        btn.disabled = false;
        return;
      }
      if (!a.added && state.current && state.current.kind === 'gateway' && state.current.id === a.contact_id) {
        state.current = null;
        $('chat-active').classList.add('hidden');
        $('chat-empty').classList.remove('hidden');
      }
      await loadTools();
      renderRemoteAgents();
    };
    card.appendChild(btn);
    grid.appendChild(card);
  }
}

async function removeGateway(target) {
  if (!confirm(`移除网关 ${target}？它名下的远程联系人和本地聊天记录会一并清掉。`)) return;
  try {
    await apiSend(`/api/gateways/${encodeURIComponent(target)}/remove`, 'POST', {});
  } catch (e) {
    alert(`移除失败：${e.message}`);
    return;
  }
  if (state.activeGateway === target) {
    state.activeGateway = null;
    $('remote-grid').innerHTML = '';
  }
  if (state.current && state.current.kind === 'gateway' && state.current.id.startsWith(`@${target}~`)) {
    state.current = null;
    $('chat-active').classList.add('hidden');
    $('chat-empty').classList.remove('hidden');
  }
  await loadGateways();
  await loadTools();
}

// ---------- ＋ 菜单（第七刀：制作/扫码/添加agent/市场 四入口） ----------

function togglePlusMenu(show) {
  const m = $('plus-menu');
  m.classList.toggle('hidden', show === undefined ? !m.classList.contains('hidden') : !show);
}

async function menuCreate() {
  togglePlusMenu(false);
  const cc = state.agents.find((a) => a.name === 'clone-creator');
  if (!cc) {
    alert('克隆大师（clone-creator）不在本机——点 ⟳ 刷新重试；它随 carrier 内嵌，缺失属异常');
    return;
  }
  showView('chat');
  await selectAgent(cc);
}

function menuScan() { togglePlusMenu(false); openScan(); }
function menuAddAgent() { togglePlusMenu(false); openAddAgent(); }
function menuMarket() {
  togglePlusMenu(false);
  showView('market');
  if (!state.market.templates.length) loadMarket(true);
}

// ---------- agent:// 地址解析与添加（扫码/手输共用管线，第七刀） ----------

/// `agent://<id>.relay.<domain>[:port][/<agent>]` → {url,target,agent}；
/// 与后端 AgentEndpoint::parse_url 同形（此处只做入口过滤，真校验在后端）。
function parseAgentUrl(raw) {
  const m = /^agent:\/\/([A-Za-z0-9][A-Za-z0-9-]*(?:\.[A-Za-z0-9-]+)*\.relay\.[A-Za-z0-9.-]+(?::\d+)?)(?:\/([A-Za-z0-9_-]+))?\/?$/.exec(
    String(raw || '').trim()
  );
  if (!m) return null;
  return { url: `agent://${m[1]}`, target: m[1].split('.')[0], agent: m[2] || null };
}

/// 扫码/手输统一落点：加网关（探活三步走在后端）→ needs_bind 先配对 →
/// 带 /分身名 后缀（客服码）自动加联系人直达会话，否则落工具页选人。
async function handleAgentUrl(raw, statusEl) {
  const parsed = parseAgentUrl(raw);
  if (!parsed) {
    const msg = '不是有效的 agent:// 地址（应为 agent://<id>.relay.<domain>[/分身名]）';
    if (statusEl) statusEl.textContent = msg; else alert(msg);
    return false;
  }
  state.pendingScanAgent = parsed.agent ? { target: parsed.target, agent: parsed.agent } : null;
  if (statusEl) statusEl.textContent = '连接中…';
  try {
    const d = await apiSend('/api/gateways', 'POST', { url: parsed.url });
    state.activeGateway = d.target;
    state.remoteAgents[d.target] = d.agents || [];
    if (d.needs_bind) state.gwNeedsBind[d.target] = true;
    await loadGateways();
    await afterGatewayAdded(d);
    return true;
  } catch (e) {
    const msg = `连接失败：${e.message}`;
    if (statusEl) statusEl.textContent = msg; else alert(msg);
    return false;
  }
}

/// 网关就绪后的落点分叉。
async function afterGatewayAdded(d) {
  if (d.needs_bind) {
    // 私有网关：落工具页，renderRemoteAgents 会给配对码输入行；
    // pendingScanAgent 记着客服码的后缀，绑定成功后续接
    showView('tools');
    renderRemoteAgents();
    return;
  }
  if (state.pendingScanAgent && state.pendingScanAgent.target === d.target) {
    await autoAddAgent(d.target, state.pendingScanAgent.agent, d.agents);
    return;
  }
  showView('tools');
  renderRemoteAgents();
}

/// 自动加远程联系人并直达会话（微信「扫一扫加好友」的落点）。
async function autoAddAgent(target, agentId, agents) {
  const found = (agents || []).find((a) => a.id === agentId);
  if (!found) {
    // 对方网关没有这个 agent：落工具页自己挑，不硬崩
    showView('tools');
    renderRemoteAgents();
    return;
  }
  if (!found.added) {
    await apiSend(`/api/tools/${encodeURIComponent(found.contact_id)}/add`, 'POST', {
      name: found.name, description: found.description, agent_type: found.agent_type, gateway: target,
    });
    await loadTools();
  }
  const c = state.remoteContacts.find((x) => x.id === found.contact_id);
  if (c) {
    showView('chat');
    await selectTool(c);
  } else {
    showView('tools');
    renderRemoteAgents();
  }
  state.pendingScanAgent = null;
}

// ---------- 扫一扫（getUserMedia + jsQR + 选图兜底） ----------

let scanStream = null;
let scanRAF = 0;

async function openScan() {
  $('scan-status').textContent = '对准 agent:// 分享二维码';
  $('scan-file').value = '';
  $('scan-dialog').showModal();
  try {
    scanStream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: 'environment' } });
    const v = $('scan-video');
    v.srcObject = scanStream;
    await v.play();
    scanLoop();
  } catch (e) {
    $('scan-status').textContent = `摄像头不可用（${e.message}）——用下方「选二维码图片」`;
  }
}

function stopScan() {
  if (scanRAF) { cancelAnimationFrame(scanRAF); scanRAF = 0; }
  if (scanStream) { scanStream.getTracks().forEach((t) => t.stop()); scanStream = null; }
  const v = $('scan-video');
  if (v) v.srcObject = null;
}

function scanLoop() {
  const v = $('scan-video');
  const c = $('scan-canvas');
  if (v.readyState >= v.HAVE_ENOUGH_DATA && v.videoWidth) {
    c.width = v.videoWidth;
    c.height = v.videoHeight;
    const ctx = c.getContext('2d', { willReadFrequently: true });
    ctx.drawImage(v, 0, 0);
    const img = ctx.getImageData(0, 0, c.width, c.height);
    const code = window.jsQR(img.data, img.width, img.height, { inversionAttempts: 'attemptBoth' });
    if (code && code.data) { onScanned(code.data); return; }
  }
  scanRAF = requestAnimationFrame(scanLoop);
}

function onScanned(text) {
  if (!String(text || '').startsWith('agent://')) {
    $('scan-status').textContent = `解出「${String(text).slice(0, 40)}」不是 agent:// 分享码，继续扫…`;
    scanRAF = requestAnimationFrame(scanLoop);
    return;
  }
  stopScan();
  $('scan-dialog').close();
  handleAgentUrl(text);
}

// 选二维码图片兜底（截图/相册——E2E 也走这条路）
function scanFromFile(file) {
  const url = URL.createObjectURL(file);
  const img = new Image();
  img.onload = () => {
    const c = $('scan-canvas');
    c.width = img.naturalWidth;
    c.height = img.naturalHeight;
    const ctx = c.getContext('2d', { willReadFrequently: true });
    ctx.drawImage(img, 0, 0);
    const d = ctx.getImageData(0, 0, c.width, c.height);
    const code = window.jsQR(d.data, d.width, d.height, { inversionAttempts: 'attemptBoth' });
    URL.revokeObjectURL(url);
    if (code && code.data) onScanned(code.data);
    else $('scan-status').textContent = '图里没解出二维码——换一张，或关掉改手输';
  };
  img.onerror = () => { URL.revokeObjectURL(url); $('scan-status').textContent = '图片读不出来'; };
  img.src = url;
}

// ---------- 添加 agent（手输：无码场景，如手机绑电脑的 claude） ----------

function openAddAgent() {
  $('aa-url').value = '';
  $('aa-status').textContent = '可带 /分身名 后缀直达该联系人；私有网关连接后按提示输配对码。';
  $('add-agent-dialog').showModal();
  $('aa-url').focus();
}

async function addAgentSubmit() {
  const url = $('aa-url').value.trim();
  if (!url) { $('aa-status').textContent = '先输入 agent:// 地址'; return; }
  const ok = await handleAgentUrl(url, $('aa-status'));
  if (ok) $('add-agent-dialog').close();
}

// ---------- 分享出码（聊天头部 📤 → agent:// 二维码） ----------

async function loadLocalGateway() {
  try {
    state.localGateway = await apiGet('/api/local-gateway');
  } catch (e) {
    state.localGateway = null;
  }
}

/// 当前会话联系人的分享地址：分身/本机工具 = 本机网关 + 名；
/// 远程联系人 @target~agent = 地址簿网关 url + 分身名。
function currentShareUrl() {
  const cur = state.current;
  if (!cur) return null;
  if (cur.kind === 'gateway' && cur.id.startsWith('@')) {
    const [target, agent] = cur.id.slice(1).split('~');
    const g = state.gateways.find((x) => x.target === target);
    return g && g.url ? `${g.url}/${agent}` : null;
  }
  const lg = state.localGateway;
  if (!lg || !lg.url) return null;
  return cur.kind === 'gateway' ? `${lg.url}/${cur.id}` : `${lg.url}/${cur.name}`;
}

function updateShareBtn() {
  $('share-btn').classList.toggle('hidden', !currentShareUrl());
}

function openShare() {
  const url = currentShareUrl();
  if (!url) { alert('拿不到分享地址（本机网关未配置或地址簿缺项）'); return; }
  $('share-url').textContent = url;
  const qr = window.qrcode(0, 'M'); // typeNumber 0 = 按内容自适应
  qr.addData(url);
  qr.make();
  const n = qr.getModuleCount();
  const cell = 8;
  const margin = 4; // 静区（模块数）
  const c = $('share-qr');
  c.width = c.height = (n + margin * 2) * cell;
  const ctx = c.getContext('2d');
  ctx.fillStyle = '#fff';
  ctx.fillRect(0, 0, c.width, c.height);
  ctx.fillStyle = '#000';
  for (let r = 0; r < n; r++) {
    for (let col = 0; col < n; col++) {
      if (qr.isDark(r, col)) {
        ctx.fillRect((margin + col) * cell, (margin + r) * cell, cell, cell);
      }
    }
  }
  $('share-dialog').showModal();
}

async function copyShareUrl() {
  const url = $('share-url').textContent;
  try {
    await navigator.clipboard.writeText(url);
    $('share-copy').textContent = '已复制';
    setTimeout(() => { $('share-copy').textContent = '复制地址'; }, 1200);
  } catch (e) {
    // 剪贴板不可用（非安全上下文）——选中地址让用户 Cmd+C
    const r = document.createRange();
    r.selectNodeContents($('share-url'));
    const sel = window.getSelection();
    sel.removeAllRanges();
    sel.addRange(r);
  }
}

function renderTools() {
  const grid = $('tools-grid');
  grid.innerHTML = '';
  if (!state.tools.length) {
    grid.innerHTML = '<div class="mkt-loading">网关没有可列出的 agent——在 ~/.aginx/agents/ 注册后重试</div>';
    return;
  }
  for (const t of state.tools) {
    const card = document.createElement('div');
    card.className = 'mkt-card';
    card.innerHTML =
      `<div class="mkt-card-name">🖥 ${escapeHtml(t.name || t.id)}` +
      `<span class="mkt-badge">${escapeHtml(t.agent_type || 'agent')}</span>` +
      `${t.added ? '<span class="mkt-badge ok">已添加</span>' : ''}</div>` +
      `<div class="mkt-card-desc">${escapeHtml((t.description || '').slice(0, 90) || '（无描述）')}</div>` +
      `<div class="mkt-card-meta"><span class="mkt-card-id">${escapeHtml(t.id)}</span>` +
      `<span>· 聊天经网关路由到对应 CLI</span></div>`;
    const btn = document.createElement('button');
    btn.className = t.added ? 'btn-ghost' : 'btn-primary';
    btn.textContent = t.added ? '移除（清会话）' : '添加到联系人';
    btn.onclick = async () => {
      if (t.added && !confirm(`移除 ${t.name || t.id}？该工具的会话记录会一并清掉。`)) return;
      btn.disabled = true;
      try {
        await apiSend(`/api/tools/${encodeURIComponent(t.id)}/${t.added ? 'remove' : 'add'}`, 'POST', {});
        t.added = !t.added;
      } catch (e) {
        btn.disabled = false;
        return;
      }
      // 移除的若正开着，退回空态
      if (!t.added && state.current && state.current.kind === 'gateway' && state.current.id === t.id) {
        state.current = null;
        $('chat-active').classList.add('hidden');
        $('chat-empty').classList.remove('hidden');
      }
      renderTools();
      renderContacts();
    };
    card.appendChild(btn);
    grid.appendChild(card);
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
  $('refresh-btn').onclick = () => { loadAgents(); loadTools(); };
  $('settings-btn').onclick = openSettings;
  $('banner').onclick = openSettings;
  $('search-input').addEventListener('input', renderContacts);

  // 接入本地工具页（第三刀 + 第五刀远程网关）
  $('tools-btn').onclick = () => {
    showView('tools');
    loadTools();
    loadGateways();
  };
  $('tools-back').onclick = () => showView('chat');

  // 远程网关地址簿
  $('gw-add-btn').onclick = addGateway;
  $('gw-url').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.isComposing) {
      e.preventDefault();
      addGateway();
    }
  });

  // 历史会话（网关台账）：点选续接 / 新建
  $('sessions-btn').onclick = openSessions;
  $('sessions-close').onclick = () => $('sessions-dialog').close();
  $('sessions-new').onclick = newSession;

  // ＋ 菜单（第七刀）：制作分身 / 扫码 / 手输 agent:// / 市场
  $('market-btn').onclick = () => togglePlusMenu();
  $('pm-create').onclick = menuCreate;
  $('pm-scan').onclick = menuScan;
  $('pm-addagent').onclick = menuAddAgent;
  $('pm-market').onclick = menuMarket;
  document.addEventListener('click', (e) => {
    if (!e.target.closest('#plus-menu') && !e.target.closest('#market-btn')) togglePlusMenu(false);
  });

  // 扫一扫 / 手输 / 分享
  $('scan-cancel').onclick = () => { stopScan(); $('scan-dialog').close(); };
  $('scan-dialog').addEventListener('close', stopScan);
  $('scan-file').addEventListener('change', (e) => {
    if (e.target.files && e.target.files[0]) scanFromFile(e.target.files[0]);
  });
  $('aa-cancel').onclick = () => $('add-agent-dialog').close();
  $('aa-go').onclick = addAgentSubmit;
  $('aa-url').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !e.isComposing) {
      e.preventDefault();
      addAgentSubmit();
    }
  });
  $('share-btn').onclick = openShare;
  $('share-close').onclick = () => $('share-dialog').close();
  $('share-copy').onclick = copyShareUrl;
  // 第八刀：访客申请 + 主人访客管理
  $('consent-cancel').onclick = () => { stopConsentPoll(); state.consent = null; $('consent-dialog').close(); };
  $('consent-submit').onclick = consentSubmit;
  $('consent-name').onkeydown = (e) => { if (e.key === 'Enter') consentSubmit(); };
  $('visitors-close').onclick = () => { stopVisitorsPoll(); state.visitorsTarget = null; $('visitors-dialog').close(); };
  let mktSearchTimer = null;
  $('mkt-search').addEventListener('input', () => {
    clearTimeout(mktSearchTimer);
    mktSearchTimer = setTimeout(() => loadMarket(true), 350);
  });
  $('mkt-more-btn').onclick = () => {
    state.market.page += 1;
    loadMarket(false);
  };
  $('hubkey-save').onclick = saveHubKey;
  $('hubkey-cancel').onclick = () => $('hubkey-dialog').close();
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
  await Promise.all([loadAgents(), loadBrain(), loadTools(), loadLocalGateway(), loadGateways()]);
  if (brainMissing()) openSettings();
}

boot();
