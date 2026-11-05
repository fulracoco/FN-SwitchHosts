'use strict';
const $ = (id) => document.getElementById(id);
const base = '/app/switchhosts';
let snapshot = null;
let selectedId = null;
let editorRevision = 0;
let dirty = false;
let busy = false;
let token = sessionStorage.getItem('switchhosts-development-token') || '';

function notify(message, error = false) {
  $('notice').textContent = message;
  $('notice').classList.toggle('error', error);
  $('notice').hidden = false;
}
async function request(path, body) {
  const headers = { 'X-SwitchHosts-Request': '1' };
  if (token) headers.Authorization = `Bearer ${token}`;
  if (body !== undefined) headers['Content-Type'] = 'application/json';
  const response = await fetch(`${base}/api/${path}`, { method: body === undefined ? 'GET' : 'POST', headers, body: body === undefined ? undefined : JSON.stringify(body), cache: 'no-store', credentials: 'same-origin' });
  if (response.status === 403) $('authentication').hidden = false;
  let result;
  try { result = await response.json(); } catch { throw new Error(`服务返回 ${response.status}，请检查连接或请求内容`); }
  if (!response.ok) throw new Error(result.error || `请求失败：${response.status}`);
  $('authentication').hidden = true;
  return result;
}
function showTab(name) {
  for (const tab of ['editor', 'system', 'history']) {
    $(`${tab}-panel`).hidden = name !== tab;
    $(`tab-${tab}`).classList.toggle('active', name === tab);
  }
}
function entryStatus(entry) {
  if (entry.last_error) return entry.last_error;
  if (!entry.url) return '本地方案';
  return entry.last_success ? `订阅 · ${new Date(entry.last_success * 1000).toLocaleString()}` : '订阅 · 尚未刷新';
}
function fillEditor(entry = null) {
  selectedId = entry?.id || null;
  editorRevision = snapshot?.document.revision || 0;
  $('editor-heading').textContent = entry ? '编辑方案' : '新建方案';
  $('title').value = entry?.title || '';
  $('url').value = entry?.url || '';
  $('content').value = entry?.content || '';
  $('enabled').checked = entry?.enabled || false;
  $('interval').value = (entry?.interval_seconds || 0) / 60;
  $('delete').hidden = !entry;
  $('refresh-subscription').hidden = !entry?.url;
  $('subscription-status').textContent = entry ? entryStatus(entry) : '';
  dirty = false;
  $('draft-status').textContent = '保存已启用的方案会立即应用。';
}
function choose(entry) {
  if (dirty && !confirm('放弃当前未保存的修改？')) return;
  fillEditor(entry);
  renderEntries();
  showTab('editor');
}
function renderEntries() {
  $('entries').replaceChildren();
  const entries = snapshot?.document.entries || [];
  $('empty').hidden = entries.length > 0;
  for (const entry of entries) {
    const row = document.createElement('div');
    row.className = `entry${entry.id === selectedId ? ' selected' : ''}`;
    const toggle = document.createElement('input');
    toggle.type = 'checkbox'; toggle.checked = entry.enabled; toggle.disabled = busy || snapshot.recovery_required;
    toggle.setAttribute('aria-label', `${entry.enabled ? '停用' : '启用'} ${entry.title}`);
    toggle.addEventListener('change', () => {
      if (dirty) { toggle.checked = entry.enabled; notify('请先保存或放弃编辑，再切换方案。', true); return; }
      run(async () => { await request('entry', { revision: snapshot.document.revision, entry: { ...entry, enabled: toggle.checked } }); await load(true); }, '方案已切换');
    });
    const button = document.createElement('button'); button.type = 'button';
    const title = document.createElement('strong'); title.textContent = entry.title;
    const status = document.createElement('small'); status.textContent = entryStatus(entry);
    button.append(title, status); button.addEventListener('click', () => choose(entry));
    row.append(toggle, button); $('entries').append(row);
  }
}
function renderHistory() {
  const selected = $('history-select').value;
  $('history-select').replaceChildren();
  for (const item of [...(snapshot?.document.history || [])].reverse()) {
    const option = document.createElement('option'); option.value = item.id;
    option.textContent = `${new Date(item.created * 1000).toLocaleString()} · ${item.label}`;
    $('history-select').append(option);
  }
  if (snapshot?.document.history.some((item) => item.id === selected)) $('history-select').value = selected;
  historyPreview();
}
function historyPreview() {
  const item = snapshot?.document.history.find((item) => item.id === $('history-select').value);
  $('history-content').textContent = item?.content || '暂无历史记录';
  $('restore').disabled = !item || busy || snapshot.recovery_required;
}
async function load(forceEditor = false) {
  const next = await request('state');
  const previousRevision = snapshot?.document.revision;
  snapshot = next;
  $('system-content').textContent = next.hosts ?? next.hosts_error ?? '无法读取 hosts';
  $('recovery').hidden = !next.recovery_required;
  if (forceEditor || !dirty) {
    fillEditor(next.document.entries.find((entry) => entry.id === selectedId) || null);
  } else if (previousRevision !== next.document.revision) {
    $('draft-status').textContent = '后台数据已更新。草稿已保留；保存时会校验版本。';
  }
  renderEntries(); renderHistory();
}
async function run(action, message) {
  if (busy) return;
  busy = true;
  for (const control of document.querySelectorAll('button,input,textarea,select')) control.disabled = true;
  try { await action(); if (message) notify(message); }
  catch (error) { notify(error.message, true); try { await load(); } catch { /* Retain the visible error and draft. */ } }
  finally {
    busy = false;
    for (const control of document.querySelectorAll('button,input,textarea,select')) control.disabled = false;
    renderEntries(); renderHistory();
  }
}
$('editor').addEventListener('input', () => { dirty = true; $('draft-status').textContent = '有未保存的修改'; });
$('editor').addEventListener('submit', (event) => {
  event.preventDefault();
  if (!snapshot) return notify('请先连接管理服务。', true);
  // getRandomValues also works on a NAS served over ordinary LAN HTTP.
  const id = selectedId || Array.from(crypto.getRandomValues(new Uint8Array(16)), (byte) => byte.toString(16).padStart(2, '0')).join('');
  const entry = { id, title: $('title').value.trim(), enabled: $('enabled').checked, content: $('content').value, url: $('url').value.trim(), interval_seconds: Number($('interval').value) * 60 };
  run(async () => { await request('entry', { revision: editorRevision, entry }); selectedId = entry.id; dirty = false; await load(true); }, '方案已保存');
});
$('new').addEventListener('click', () => choose(null));
$('reload').addEventListener('click', () => run(() => load(), '状态已刷新'));
$('delete').addEventListener('click', () => {
  if (!selectedId || !confirm('删除此方案？已启用的内容会同时从管理段移除。')) return;
  run(async () => { await request('delete', { revision: editorRevision, id: selectedId }); selectedId = null; dirty = false; await load(true); }, '方案已删除');
});
$('refresh-subscription').addEventListener('click', () => {
  if (dirty) return notify('请先保存编辑，再刷新订阅。', true);
  run(async () => { await request('refresh', { revision: snapshot.document.revision, id: selectedId }); await load(true); }, '订阅已刷新');
});
$('token-form').addEventListener('submit', (event) => { event.preventDefault(); token = $('token').value; sessionStorage.setItem('switchhosts-development-token', token); $('token').value = ''; run(() => load(), '已连接'); });
for (const tab of ['editor', 'system', 'history']) $(`tab-${tab}`).addEventListener('click', () => showTab(tab));
$('history-select').addEventListener('change', historyPreview);
$('restore').addEventListener('click', () => {
  if (dirty) return notify('请先保存或放弃草稿，再恢复历史。', true);
  if (!confirm('恢复此完整 hosts 快照，并暂停全部方案？')) return;
  run(async () => { await request('restore', { revision: snapshot.document.revision, id: $('history-select').value }); await load(true); }, '历史已恢复，全部方案已暂停');
});
$('reconcile').addEventListener('click', () => run(async () => { await request('reconcile', {}); await load(); }, '写入结果已核对'));
$('keep-current').addEventListener('click', () => {
  if (!confirm('保留当前系统 hosts，结束待恢复操作，并暂停全部方案？')) return;
  run(async () => { await request('keep-current', { revision: snapshot.document.revision }); dirty = false; await load(true); }, '已保留当前 hosts，全部方案已暂停');
});
$('export').addEventListener('click', () => run(async () => {
  const data = await request('export');
  const url = URL.createObjectURL(new Blob([JSON.stringify(data, null, 2)], { type: 'application/json' }));
  const link = document.createElement('a'); link.href = url; link.download = `FN-SwitchHosts-${new Date().toISOString().slice(0, 10)}.json`; link.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}, '方案已导出'));
$('import').addEventListener('change', (event) => {
  const file = event.target.files[0]; event.target.value = '';
  if (!file || !snapshot) return;
  if (dirty) return notify('请先保存或放弃草稿，再导入方案。', true);
  if (file.size > 32 * 1024 * 1024) return notify('导入文件不能超过 32 MiB。', true);
  if (!confirm('替换全部方案？导入后方案处于停用状态。当前启用的方案需先手动停用。')) return;
  run(async () => { const document = JSON.parse(await file.text()); await request('import', { revision: snapshot.document.revision, document }); selectedId = null; await load(true); }, '方案已导入，均处于停用状态');
});
window.addEventListener('beforeunload', (event) => { if (dirty) { event.preventDefault(); event.returnValue = ''; } });
setInterval(() => { if (!busy && snapshot) load().catch((error) => notify(error.message, true)); }, 15000);
run(() => load());
