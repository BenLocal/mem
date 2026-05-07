'use strict';

// All DOM nodes representing memory data are built with createElement +
// textContent + setAttribute — never innerHTML on user-supplied strings —
// so summary/content/tags containing angle brackets, quotes, or script tags
// can never become an XSS sink.

const TENANT = 'local';
document.getElementById('tenant').textContent = TENANT;

const grid       = document.getElementById('grid');
const search     = document.getElementById('search');
const filters    = document.getElementById('filters');
const countEl    = document.getElementById('count');
const modal      = document.getElementById('modal');
const modalId    = document.getElementById('modal-id');
const btnYes     = document.getElementById('btn-confirm');
const btnNo      = document.getElementById('btn-cancel');
const toast      = document.getElementById('toast');
const detailBg   = document.getElementById('detail-bg');
const detailBody = document.getElementById('detail-body');
const detailClose = document.getElementById('detail-close');

let allMemories  = [];
let filterStatus = 'live';   // live = active | provisional | pending_confirmation
let filterText   = '';
let pendingDelete = null;
let openDetailId = null;

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

  const card = el('article', {
    class: 'card', 'data-id': m.memory_id,
    role: 'button', tabindex: '0',
    'aria-label': `view details for ${m.memory_id}`,
  });
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
    btn.addEventListener('click', e => {
      e.stopPropagation();    // do not open detail when archiving
      openDelete(btn.dataset.del);
    });
  }
  // Whole-card click → detail. Skip if the actual click target was the
  // archive button (already handled above with stopPropagation).
  for (const card of grid.querySelectorAll('.card')) {
    card.addEventListener('click', () => openDetail(card.dataset.id));
    card.addEventListener('keydown', e => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        openDetail(card.dataset.id);
      }
    });
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
    // If the detail panel is open for the same id, refresh it so the user
    // sees the new "archived" state without having to re-click the card.
    if (openDetailId === id) await openDetail(id);
  } catch (e) {
    showToast(`failed: ${e.message}`, true);
    btnYes.disabled = false;
    btnYes.textContent = 'try again';
  }
});

// ------------------------------------------------------------ detail panel

function metaRow(label, value, mono = true) {
  const dt = el('dt', { text: label });
  let dd;
  if (value == null || value === '') {
    dd = el('dd');
    dd.appendChild(el('span', { class: 'none', text: '—' }));
  } else {
    dd = el('dd');
    if (mono) {
      dd.appendChild(el('code', { text: String(value) }));
    } else {
      dd.appendChild(document.createTextNode(String(value)));
    }
  }
  return [dt, dd];
}

function section(title, ...children) {
  const s = el('div', { class: 'detail-section' });
  s.appendChild(el('h4', { text: title }));
  for (const c of children) if (c) s.appendChild(c);
  return s;
}

function buildDetail(detail) {
  const m = detail.memory || {};
  const t = m.memory_type || '';
  const status = m.status || 'active';

  const wrap = document.createDocumentFragment();

  const stamp = el('div', { class: `type-stamp ${t} detail-stamp`, text: typeAbbrev(t) });
  wrap.appendChild(stamp);

  wrap.appendChild(el('h2', {
    id: 'detail-title', class: 'detail-summary',
    text: m.summary || '(no summary)',
  }));
  wrap.appendChild(el('div', { class: 'detail-id', text: m.memory_id || '—' }));

  // status / type / scope inline pill row (visual continuity with the cards)
  const inlineMeta = el('div', { class: 'card-meta', style: 'margin: 0 0 1.4rem; padding-top: 0; border-top: none;' });
  inlineMeta.appendChild(el('span', { class: `status-pill ${status}`, text: status.replace('_', ' ') }));
  inlineMeta.appendChild(el('span', { text: m.memory_type || '?' }));
  for (const tag of (m.tags || [])) inlineMeta.appendChild(el('span', { class: 'tag', text: tag }));
  wrap.appendChild(inlineMeta);

  // content (verbatim, no truncation)
  if (m.content) {
    wrap.appendChild(section('content', el('pre', { text: m.content })));
  }

  // evidence
  if ((m.evidence || []).length) {
    const ul = el('ul');
    for (const ev of m.evidence) ul.appendChild(el('li', { text: ev }));
    wrap.appendChild(section('evidence', ul));
  }

  // code refs
  if ((m.code_refs || []).length) {
    const ul = el('ul');
    for (const r of m.code_refs) ul.appendChild(el('li', { text: r }));
    wrap.appendChild(section('code refs', ul));
  }

  // topics
  if ((m.topics || []).length) {
    const tagBox = el('div', { class: 'detail-tags' });
    for (const tp of m.topics) tagBox.appendChild(el('span', { class: 'tag', text: tp }));
    wrap.appendChild(section('topics', tagBox));
  }

  // primary metadata grid
  const dl = el('dl', { class: 'detail-meta' });
  const fields = [
    ['scope',                 m.scope],
    ['visibility',            m.visibility],
    ['project',               m.project],
    ['repo',                  m.repo],
    ['module',                m.module],
    ['task type',             m.task_type],
    ['version',               m.version],
    ['confidence',            m.confidence != null ? m.confidence.toFixed(2) : null],
    ['decay',                 m.decay_score != null ? m.decay_score.toFixed(2) : null],
    ['source agent',          m.source_agent],
    ['session',               m.session_id],
    ['supersedes',            m.supersedes_memory_id],
    ['idempotency',           m.idempotency_key],
    ['content hash',          m.content_hash],
    ['created',               fmtDate(m.created_at)],
    ['updated',               fmtDate(m.updated_at)],
    ['last validated',        m.last_validated_at ? fmtDate(m.last_validated_at) : null],
  ];
  for (const [k, v] of fields) {
    const [dt, dd] = metaRow(k, v);
    dl.appendChild(dt); dl.appendChild(dd);
  }
  wrap.appendChild(section('record', dl));

  // embedding
  const em = detail.embedding || {};
  const edl = el('dl', { class: 'detail-meta' });
  for (const [k, v] of [
    ['status',  em.status],
    ['model',   em.model],
    ['hash',    em.content_hash],
    ['updated', em.updated_at ? fmtDate(em.updated_at) : null],
  ]) {
    const [dt, dd] = metaRow(k, v);
    edl.appendChild(dt); edl.appendChild(dd);
  }
  wrap.appendChild(section('embedding', edl));

  // graph links
  const links = detail.graph_links || [];
  if (links.length) {
    const box = el('div');
    for (const e of links) {
      const row = el('div', { class: 'graph-edge' });
      row.appendChild(el('span', { class: 'rel', text: e.relation || '—' }));
      const target = el('span', { class: 'target', text: e.to_node_id || e.from_node_id || '—' });
      if (e.valid_to) target.appendChild(el('span', { class: 'gone', text: '  · closed' }));
      row.appendChild(target);
      box.appendChild(row);
    }
    wrap.appendChild(section(`graph links (${links.length})`, box));
  }

  // version chain
  const chain = detail.version_chain || [];
  if (chain.length) {
    const box = el('div');
    for (const v of chain) {
      const here = v.memory_id === m.memory_id;
      const row = el('div', { class: `version-row${here ? ' current' : ''}` });
      row.appendChild(el('span', { class: 'v', text: 'v' + (v.version ?? '?') }));
      const right = el('span');
      right.appendChild(document.createTextNode(`${v.status || '?'}  ·  ${fmtDate(v.updated_at)}`));
      if (here) right.appendChild(document.createTextNode('  ← here'));
      row.appendChild(right);
      box.appendChild(row);
    }
    wrap.appendChild(section('version chain', box));
  }

  // feedback summary
  const fb = detail.feedback_summary || {};
  if (Object.keys(fb).length) {
    const grid = el('div', { class: 'feedback-grid' });
    for (const [k, v] of Object.entries(fb)) {
      const cell = el('div', { class: 'feedback-cell' });
      cell.appendChild(el('span', { class: 'n', text: String(v ?? 0) }));
      cell.appendChild(el('span', { class: 'k', text: k.replace(/_/g, ' ') }));
      grid.appendChild(cell);
    }
    wrap.appendChild(section('feedback', grid));
  }

  // archive button
  const removed = ['archived', 'rejected'].includes(status);
  const actions = el('div', { class: 'detail-actions' });
  actions.appendChild(el('span', {
    class: 'detail-archive-note',
    text: removed
      ? 'this record is already off-shelf'
      : 'archiving keeps the row verbatim — only search drops it',
  }));
  const archiveBtn = el('button', {
    class: 'detail-archive-btn', type: 'button',
    text: removed ? 'already archived' : 'archive this record',
  });
  if (removed) archiveBtn.disabled = true;
  else archiveBtn.addEventListener('click', () => openDelete(m.memory_id));
  actions.appendChild(archiveBtn);
  wrap.appendChild(actions);

  return wrap;
}

async function openDetail(id) {
  if (!id) return;
  openDetailId = id;
  detailBg.classList.add('open');
  detailBg.setAttribute('aria-hidden', 'false');
  while (detailBody.firstChild) detailBody.removeChild(detailBody.firstChild);
  detailBody.appendChild(buildPlaceholder('retrieving record', id, 'loading'));
  try {
    const r = await fetch(`/memories/${encodeURIComponent(id)}?tenant=${encodeURIComponent(TENANT)}`);
    if (!r.ok) {
      const t = await r.text();
      throw new Error(`HTTP ${r.status}: ${t.slice(0, 160)}`);
    }
    const detail = await r.json();
    if (openDetailId !== id) return;  // user closed / opened another while loading
    while (detailBody.firstChild) detailBody.removeChild(detailBody.firstChild);
    detailBody.appendChild(buildDetail(detail));
    detailBody.scrollTop = 0;
  } catch (e) {
    while (detailBody.firstChild) detailBody.removeChild(detailBody.firstChild);
    detailBody.appendChild(buildPlaceholder('could not retrieve record', e.message, 'error'));
  }
}

function closeDetail() {
  detailBg.classList.remove('open');
  detailBg.setAttribute('aria-hidden', 'true');
  openDetailId = null;
}

detailClose.addEventListener('click', closeDetail);
detailBg.addEventListener('click', e => { if (e.target === detailBg) closeDetail(); });
document.addEventListener('keydown', e => {
  // detail panel takes priority; only close it if delete-confirm modal is not open
  if (e.key === 'Escape' && detailBg.classList.contains('open') && !modal.classList.contains('open')) {
    closeDetail();
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
