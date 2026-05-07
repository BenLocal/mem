'use strict';

// All DOM nodes representing memory data are built with createElement +
// textContent + setAttribute — never innerHTML on user-supplied strings —
// so summary/content/tags containing angle brackets, quotes, or script tags
// can never become an XSS sink.

const TENANT = 'local';
document.getElementById('tenant').textContent = TENANT;

const grid    = document.getElementById('grid');
const search  = document.getElementById('search');
const filters = document.getElementById('filters');
const countEl = document.getElementById('count');
const modal   = document.getElementById('modal');
const modalId = document.getElementById('modal-id');
const btnYes  = document.getElementById('btn-confirm');
const btnNo   = document.getElementById('btn-cancel');
const toast   = document.getElementById('toast');

let allMemories  = [];
let filterStatus = 'live';   // live = active | provisional | pending_confirmation
let filterText   = '';
let pendingDelete = null;

// ------------------------------------------------------------ helpers

function showToast(msg, err = false) {
  toast.textContent = msg;
  toast.classList.toggle('error', err);
  toast.classList.add('show');
  setTimeout(() => toast.classList.remove('show'), 2400);
}

function fmtDate(s) {
  if (!s) return '—';
  // mem stores microsecond-padded numeric strings ("00000001778060883021");
  // chop the trailing 3 chars to get milliseconds.
  const ms = Number(String(s).slice(0, -3) || s);
  if (!Number.isFinite(ms)) return s;
  const d = new Date(ms);
  if (isNaN(d)) return s;
  const yyyy = d.getUTCFullYear();
  const mm   = String(d.getUTCMonth() + 1).padStart(2, '0');
  const dd   = String(d.getUTCDate()).padStart(2, '0');
  const hh   = String(d.getUTCHours()).padStart(2, '0');
  const mi   = String(d.getUTCMinutes()).padStart(2, '0');
  return `${yyyy}-${mm}-${dd} ${hh}:${mi} UTC`;
}

function typeAbbrev(t) {
  return ({
    implementation: 'IMPL',
    observation:    'OBS',
    experience:     'EXP',
    preference:     'PREF',
    workflow:       'FLOW',
    pattern:        'PAT',
  })[t] || (t || '?').slice(0, 4).toUpperCase();
}

function matchesFilter(m) {
  if (filterStatus === 'live') {
    if (!['active', 'provisional', 'pending_confirmation'].includes(m.status)) return false;
  } else if (filterStatus !== 'all' && m.status !== filterStatus) {
    return false;
  }
  if (filterText) {
    const hay = `${m.summary || ''} ${m.content || ''} ${(m.tags || []).join(' ')} ${m.memory_id || ''}`.toLowerCase();
    if (!hay.includes(filterText)) return false;
  }
  return true;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    if (k === 'class') node.className = v;
    else if (k === 'text') node.textContent = v;
    else node.setAttribute(k, v);
  }
  for (const c of children) {
    if (c == null) continue;
    if (typeof c === 'string') node.appendChild(document.createTextNode(c));
    else node.appendChild(c);
  }
  return node;
}

// ------------------------------------------------------------ render

function buildCard(m) {
  const t = m.memory_type || '';
  const status = m.status || 'active';
  const summary = (m.summary || m.content || '').split('\n')[0].slice(0, 140);
  const contentRaw = (m.content || '').slice(0, 240);
  const contentEllipsis = (m.content && m.content.length > 240) ? '…' : '';
  const scope = [m.scope, m.repo, m.module].filter(Boolean).join(' · ');
  const tags = (m.tags || []).slice(0, 6);

  const card = el('article', { class: 'card', 'data-id': m.memory_id });
  card.appendChild(el('div', { class: `type-stamp ${t}`, text: typeAbbrev(t) }));
  card.appendChild(el('div', { class: 'card-id', text: m.memory_id || '—' }));
  card.appendChild(el('h3', { class: 'card-summary', text: summary }));
  card.appendChild(el('div', { class: 'card-content', text: contentRaw + contentEllipsis }));

  const meta = el('div', { class: 'card-meta' });
  meta.appendChild(el('span', { class: `status-pill ${status}`, text: status.replace('_', ' ') }));
  if (scope) meta.appendChild(el('span', { text: scope }));
  for (const tag of tags) meta.appendChild(el('span', { class: 'tag', text: tag }));
  card.appendChild(meta);

  const foot = el('div', { class: 'card-foot' });
  foot.appendChild(el('span', { class: 'card-when', text: fmtDate(m.created_at) }));
  const removed = ['archived', 'rejected'].includes(status);
  const delAttrs = { class: 'card-delete', 'data-del': m.memory_id, text: 'archive ⟶' };
  if (removed) { delAttrs.disabled = ''; delAttrs.title = 'already removed'; }
  foot.appendChild(el('button', delAttrs));
  card.appendChild(foot);

  return card;
}

function buildPlaceholder(text, sub, cls = '') {
  const ph = el('div', { class: `placeholder ${cls}`.trim(), text });
  if (sub) ph.appendChild(el('small', { text: sub }));
  return ph;
}

function render() {
  const rows = allMemories.filter(matchesFilter);
  countEl.textContent = String(rows.length).padStart(3, '0');
  while (grid.firstChild) grid.removeChild(grid.firstChild);
  if (rows.length === 0) {
    const empty = allMemories.length === 0
      ? 'the archive is empty'
      : 'nothing in the stacks matches that';
    const sub = `filter: ${filterStatus}${filterText ? ' · "' + filterText + '"' : ''}`;
    grid.appendChild(buildPlaceholder(empty, sub));
    return;
  }
  for (const m of rows) grid.appendChild(buildCard(m));
  for (const btn of grid.querySelectorAll('[data-del]:not(:disabled)')) {
    btn.addEventListener('click', () => openDelete(btn.dataset.del));
  }
}

// ------------------------------------------------------------ delete modal

function openDelete(id) {
  pendingDelete = id;
  modalId.textContent = id;
  modal.classList.add('open');
  modal.setAttribute('aria-hidden', 'false');
  btnYes.disabled = false;
  btnYes.textContent = 'archive it';
}

function closeModal() {
  modal.classList.remove('open');
  modal.setAttribute('aria-hidden', 'true');
  pendingDelete = null;
}

btnNo.addEventListener('click', closeModal);
modal.addEventListener('click', e => { if (e.target === modal) closeModal(); });
document.addEventListener('keydown', e => {
  if (e.key === 'Escape' && modal.classList.contains('open')) closeModal();
});

btnYes.addEventListener('click', async () => {
  if (!pendingDelete) return;
  const id = pendingDelete;
  btnYes.disabled = true;
  btnYes.textContent = 'sending…';
  try {
    const r = await fetch('/memories/feedback', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ tenant: TENANT, memory_id: id, feedback_kind: 'incorrect' })
    });
    if (!r.ok) {
      const t = await r.text();
      throw new Error(`HTTP ${r.status}: ${t.slice(0, 160)}`);
    }
    const cardEl = grid.querySelector(`[data-id="${CSS.escape(id)}"]`);
    if (cardEl) {
      cardEl.classList.add('deleting');
      await new Promise(res => setTimeout(res, 700));
    }
    closeModal();
    showToast('archived. record retained verbatim.');
    await load();
  } catch (e) {
    showToast(`failed: ${e.message}`, true);
    btnYes.disabled = false;
    btnYes.textContent = 'try again';
  }
});

// ------------------------------------------------------------ filters

filters.addEventListener('click', e => {
  const btn = e.target.closest('.filter-btn');
  if (!btn) return;
  filterStatus = btn.dataset.status;
  filters.querySelectorAll('.filter-btn').forEach(b => b.classList.toggle('active', b === btn));
  render();
});

let searchTimer = null;
search.addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => {
    filterText = search.value.trim().toLowerCase();
    render();
  }, 120);
});

// ------------------------------------------------------------ load

async function load() {
  try {
    const r = await fetch(`/memories?tenant=${encodeURIComponent(TENANT)}`);
    if (!r.ok) {
      const t = await r.text();
      throw new Error(`HTTP ${r.status}: ${t.slice(0, 160)}`);
    }
    allMemories = await r.json();
    allMemories.sort((a, b) => String(b.created_at).localeCompare(String(a.created_at)));
    render();
  } catch (e) {
    while (grid.firstChild) grid.removeChild(grid.firstChild);
    grid.appendChild(buildPlaceholder('could not reach the archive', e.message, 'error'));
  }
}

load();
