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
  view: 'chat', // 'chat' | 'market' | 'detail'
  market: { q: '', page: 1, templates: [], hasMore: false, keyOk: true, hubEnv: '', hubUrl: '' },
  installing: false,
  tools: [], // 网关 agent（/api/tools）；网关不可达时只含已添加的 stale 项
  cwdByTool: {}, // 工具 id -> 工作目录（空=默认；换目录＝新会话）
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
  if (!contactList.children.length) {
    contactList.innerHTML = `<div class="empty-list">${q ? '无匹配联系人' : '还没有联系人<br>点 ⟳ 刷新 · ＋ 装分身 · 🖥 接工具'}</div>`;
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
  $('chat-cwd-row').classList.add('hidden');
  setStatus('', false);
  await applyHistory(a.name);
  scrollChat();
  $('msg-input').focus();
}

/// 拉取并渲染一个联系人的历史（分身按 name，网关工具按 id + cwd）。
async function applyHistory(key, cwd) {
  $('chat-body').innerHTML = '';
  state.history.delete(key);
  try {
    let url = `/api/history?agent=${encodeURIComponent(key)}&sender=${encodeURIComponent(state.senderId)}`;
    if (cwd) url += `&cwd=${encodeURIComponent(cwd)}`;
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

// ---------- 网关工具会话（第三刀：agent:// 经网关路由 CLI） ----------

function cwdOf(toolId) {
  if (state.cwdByTool[toolId] !== undefined) return state.cwdByTool[toolId];
  const t = state.tools.find((x) => x.id === toolId);
  return (t && t.default_cwd) || '';
}

// 目录选择器：home 门内逐级浏览（/api/fs/browse 只列目录）
const cwdPicker = { path: '' };

async function openCwdPicker() {
  const cur = state.current;
  if (!cur || cur.kind !== 'gateway') return;
  await loadCwdDir(cwdOf(cur.id) || '~');
  $('cwd-dialog').showModal();
}

async function loadCwdDir(p) {
  let data;
  try {
    data = await apiGet(`/api/fs/browse?path=${encodeURIComponent(p)}`);
  } catch (e) {
    $('cwd-path-bar').textContent = `加载失败：${e.message}`;
    $('cwd-list').innerHTML = '';
    return;
  }
  cwdPicker.path = data.path;
  $('cwd-path-bar').textContent = data.path;
  const list = $('cwd-list');
  list.innerHTML = '';
  if (data.parent !== null && data.parent !== undefined) {
    const up = document.createElement('div');
    up.className = 'cwd-row-item cwd-up';
    up.textContent = '⬆ 返回上级';
    up.onclick = () => loadCwdDir(data.parent);
    list.appendChild(up);
  }
  for (const name of data.entries || []) {
    const el = document.createElement('div');
    el.className = 'cwd-row-item';
    el.textContent = `📁 ${name}`;
    el.onclick = () =>
      loadCwdDir(data.path === '/' ? `/${name}` : `${data.path}/${name}`);
    list.appendChild(el);
  }
  if (!list.children.length) {
    list.innerHTML = '<div class="cwd-row-item cwd-dim">（没有子目录）</div>';
  }
}

async function selectTool(t) {
  if (state.streaming) return;
  state.current = { kind: 'gateway', id: t.id, name: t.name || t.id, emoji: '🖥' };
  renderContacts();
  $('chat-empty').classList.add('hidden');
  $('chat-active').classList.remove('hidden');
  $('chat-avatar').textContent = '🖥';
  $('chat-name').textContent = t.name || t.id;
  $('chat-sub').textContent = `网关 · ${t.agent_type || 'agent'} · ${t.id}`;
  $('chat-cwd-row').classList.remove('hidden');
  $('chat-cwd').value = cwdOf(t.id);
  setStatus('', false);
  await applyHistory(t.id, cwdOf(t.id) || undefined);
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
  const cwd = isTool ? cwdOf(agent.id) : '';

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
      body: JSON.stringify({ message: text, sender_id: state.senderId, ...(cwd ? { cwd } : {}) }),
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
              // 网关工具轮：真金白银计量（cost/duration），不折算 tok/轮
              const dur = ev.duration_ms != null ? `${(ev.duration_ms / 1000).toFixed(1)}s` : '?';
              setStatus(`完成 · $${Number(ev.cost_usd).toFixed(4)} · ${dur}`, false);
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
    gwErr = data.gateway_error || '';
  } catch (e) {
    state.tools = [];
    gwErr = e.message;
  }
  const banner = $('tools-banner');
  if (gwErr) {
    banner.textContent = `网关不可达：${gwErr}——已添加工具保留在联系人，恢复后可用`;
    banner.classList.remove('hidden');
  } else {
    banner.classList.add('hidden');
  }
  renderTools();
  renderContacts();
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
      `<span>· 默认目录 ${escapeHtml(t.default_cwd || '')}</span></div>`;
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

  // 接入本地工具页（第三刀）
  $('tools-btn').onclick = () => {
    showView('tools');
    loadTools();
  };
  $('tools-back').onclick = () => showView('chat');
  $('cwd-browse-btn').onclick = openCwdPicker;
  $('cwd-cancel').onclick = () => $('cwd-dialog').close();
  $('cwd-pick').onclick = () => {
    if (!cwdPicker.path) return;
    $('cwd-dialog').close();
    const input = $('chat-cwd');
    input.value = cwdPicker.path;
    input.dispatchEvent(new Event('change')); // 复用换目录＝新会话逻辑
  };
  $('chat-cwd').addEventListener('change', () => {
    const cur = state.current;
    if (!cur || cur.kind !== 'gateway' || state.streaming) return;
    const v = $('chat-cwd').value.trim();
    if (v === cwdOf(cur.id)) return;
    state.cwdByTool[cur.id] = v;
    applyHistory(cur.id, v || undefined); // 换目录＝新会话
  });

  // 装分身页
  $('market-btn').onclick = () => {
    showView('market');
    if (!state.market.templates.length) loadMarket(true);
  };
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
  await Promise.all([loadAgents(), loadBrain(), loadTools()]);
  if (brainMissing()) openSettings();
}

boot();
