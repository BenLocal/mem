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
// ─── transcripts view ────────────────────────────────────────
const tabs           = document.getElementById('tabs');
const viewArchive    = document.getElementById('view-archive');
const viewTranscripts = document.getElementById('view-transcripts');
const titleMode      = document.getElementById('title-mode');
const titleSub       = document.getElementById('title-sub');
const countLabel     = document.getElementById('count-label');
const sessionsList   = document.getElementById('sessions-list');
const txSearch       = document.getElementById('tx-search');
const transcriptBg   = document.getElementById('transcript-bg');
const transcriptBody = document.getElementById('transcript-body');
const transcriptMeta = document.getElementById('transcript-meta');
const transcriptClose = document.getElementById('transcript-close');

let currentView = 'archive';
let allSessions = [];
let txFilterText = '';
let openTranscriptSession = null;

const detailBg       = document.getElementById('detail-bg');
const detailBody     = document.getElementById('detail-body');
const detailClose    = document.getElementById('detail-close');
const detailActions  = document.getElementById('detail-actions');
const detailNote     = document.getElementById('detail-actions-note');
const detailArchiveBtn = document.getElementById('detail-archive-btn');
const detailDeleteBtn  = document.getElementById('detail-delete-btn');

const deleteModal    = document.getElementById('delete-modal');
const deleteModalId  = document.getElementById('delete-modal-id');
const deleteBtnYes   = document.getElementById('delete-btn-confirm');
const deleteBtnNo    = document.getElementById('delete-btn-cancel');

let allMemories  = [];
let filterStatus = 'live';   // live = active | provisional | pending_confirmation
let filterText   = '';
let pendingDelete = null;
let pendingHardDelete = null;
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
  // Two formats land here:
  //   1. memories side — `current_timestamp()` (src/storage/time.rs)
  //      writes `{millis:020}`, so a 20-digit zero-padded epoch millis
  //      string like "00000001778060883021".
  //   2. transcripts side — `conversation_messages.created_at` is the
  //      verbatim ISO-8601 timestamp ingested from the transcript
  //      JSONL ("2026-04-30T06:53:13.501Z").
  // Detect by content: pure digits → epoch ms; otherwise ISO-8601.
  const str = String(s).trim();
  let d;
  if (/^\d+$/.test(str)) {
    const ms = Number(str);
    if (!Number.isFinite(ms) || ms <= 0) return s;
    d = new Date(ms);
  } else {
    d = new Date(str);
  }
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
// keydown for Escape is consolidated in the priority handler near the
// detail-panel close logic — see `document.addEventListener('keydown',...)`
// further down. Avoid registering a second listener here, otherwise Escape
// would close the modal AND the detail panel under it in the same tick.

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

  return wrap;
}

/// Populate the static footer slot with archive/delete buttons + note.
/// The footer lives outside `.detail-body` so it always sits at the
/// panel's bottom (flex sibling), no `position: sticky` games.
function populateActions(detail) {
  const m = detail.memory || {};
  const status = m.status || 'active';
  const removed = ['archived', 'rejected'].includes(status);
  detailNote.textContent = removed
    ? 'this record is already off-shelf — archive disabled, but you can still delete it forever'
    : 'archiving keeps the row verbatim — only search drops it';
  detailArchiveBtn.textContent = removed ? 'already archived' : 'archive this record';
  detailArchiveBtn.disabled = removed;
  detailArchiveBtn.onclick = removed ? null : () => openDelete(m.memory_id);
  detailDeleteBtn.onclick = () => openHardDelete(m.memory_id);
  detailActions.hidden = false;
}

async function openDetail(id) {
  if (!id) return;
  openDetailId = id;
  detailBg.classList.add('open');
  detailBg.setAttribute('aria-hidden', 'false');
  detailActions.hidden = true;
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
    populateActions(detail);
  } catch (e) {
    while (detailBody.firstChild) detailBody.removeChild(detailBody.firstChild);
    detailBody.appendChild(buildPlaceholder('could not retrieve record', e.message, 'error'));
  }
}

function closeDetail() {
  detailBg.classList.remove('open');
  detailBg.setAttribute('aria-hidden', 'true');
  detailActions.hidden = true;
  openDetailId = null;
}

// ------------------------------------------------------------ hard delete

function openHardDelete(id) {
  pendingHardDelete = id;
  deleteModalId.textContent = id;
  deleteModal.classList.add('open');
  deleteModal.setAttribute('aria-hidden', 'false');
  deleteBtnYes.disabled = false;
  deleteBtnYes.textContent = 'delete forever';
}

function closeDeleteModal() {
  deleteModal.classList.remove('open');
  deleteModal.setAttribute('aria-hidden', 'true');
  pendingHardDelete = null;
}

deleteBtnNo.addEventListener('click', closeDeleteModal);
deleteModal.addEventListener('click', e => { if (e.target === deleteModal) closeDeleteModal(); });

deleteBtnYes.addEventListener('click', async () => {
  if (!pendingHardDelete) return;
  const id = pendingHardDelete;
  deleteBtnYes.disabled = true;
  deleteBtnYes.textContent = 'erasing…';
  try {
    const r = await fetch(`/memories/${encodeURIComponent(id)}?tenant=${encodeURIComponent(TENANT)}`, {
      method: 'DELETE',
    });
    if (!r.ok) {
      const t = await r.text();
      throw new Error(`HTTP ${r.status}: ${t.slice(0, 160)}`);
    }
    closeDeleteModal();
    if (openDetailId === id) closeDetail();
    showToast('deleted forever. row erased.');
    await load();
  } catch (e) {
    showToast(`failed: ${e.message}`, true);
    deleteBtnYes.disabled = false;
    deleteBtnYes.textContent = 'try again';
  }
});

detailClose.addEventListener('click', closeDetail);
detailBg.addEventListener('click', e => { if (e.target === detailBg) closeDetail(); });
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape') return;
  // priority: hard-delete modal > archive-confirm modal > detail panel
  if (deleteModal.classList.contains('open')) closeDeleteModal();
  else if (modal.classList.contains('open')) closeModal();
  else if (detailBg.classList.contains('open')) closeDetail();
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

// ============================================================
// transcripts view
// ============================================================

const VIEW_LABELS = {
  archive:     { mode: 'archive',     sub: 'a register of remembered things, kept by hand', count: 'records on file' },
  transcripts: { mode: 'transcripts', sub: 'verbatim conversation logs, bound and shelved', count: 'sessions on file' },
};

function setView(name) {
  if (name === currentView) return;
  currentView = name;
  for (const t of tabs.querySelectorAll('.tab')) {
    const on = t.dataset.view === name;
    t.classList.toggle('active', on);
    t.setAttribute('aria-selected', on ? 'true' : 'false');
  }
  viewArchive.hidden     = name !== 'archive';
  viewTranscripts.hidden = name !== 'transcripts';
  const lbl = VIEW_LABELS[name];
  titleMode.textContent  = lbl.mode;
  titleSub.textContent   = lbl.sub;
  countLabel.textContent = lbl.count;
  if (name === 'archive') {
    countEl.textContent = String(allMemories.filter(matchesFilter).length).padStart(3, '0');
  } else if (allSessions.length === 0) {
    loadSessions();
  } else {
    renderSessions();
  }
}

tabs.addEventListener('click', e => {
  const btn = e.target.closest('.tab');
  if (btn) setView(btn.dataset.view);
});

function fmtDateShort(s) {
  // Same format-detection contract as fmtDate, just returns the UTC date
  // without the "UTC" suffix (used in dense block-stripe layouts where
  // the column is already labelled).
  if (!s) return '—';
  const str = String(s).trim();
  let d;
  if (/^\d+$/.test(str)) {
    const ms = Number(str);
    if (!Number.isFinite(ms) || ms <= 0) return s;
    d = new Date(ms);
  } else {
    d = new Date(str);
  }
  if (isNaN(d)) return s;
  return `${d.getUTCFullYear()}-${String(d.getUTCMonth()+1).padStart(2,'0')}-${String(d.getUTCDate()).padStart(2,'0')} ${String(d.getUTCHours()).padStart(2,'0')}:${String(d.getUTCMinutes()).padStart(2,'0')}`;
}

function matchesSession(s) {
  if (!txFilterText) return true;
  const hay = `${s.session_id || ''} ${s.caller_agent || ''}`.toLowerCase();
  return hay.includes(txFilterText);
}

function buildSessionRow(s) {
  const row = el('div', {
    class: 'session-row', tabindex: '0', role: 'button',
    'aria-label': `open transcript for ${s.session_id}`,
    'data-id': s.session_id,
  });
  const id = el('div', { class: 'session-id' });
  id.appendChild(el('span', { class: 'lead', text: '⟶' }));
  id.appendChild(document.createTextNode(s.session_id || '—'));
  row.appendChild(id);

  row.appendChild(el('span', { class: 'session-agent', text: s.caller_agent || 'unknown' }));

  const cnt = el('div', { class: 'session-count' });
  cnt.appendChild(document.createTextNode(String(s.block_count ?? 0)));
  cnt.appendChild(el('span', { class: 'lbl', text: 'blocks' }));
  row.appendChild(cnt);

  const when = el('div', { class: 'session-when' });
  when.appendChild(document.createTextNode(fmtDateShort(s.first_at)));
  when.appendChild(el('span', { class: 'arrow', text: ' → ' }));
  when.appendChild(document.createTextNode(fmtDateShort(s.last_at)));
  row.appendChild(when);
  return row;
}

function renderSessions() {
  const rows = allSessions.filter(matchesSession);
  countEl.textContent = String(rows.length).padStart(3, '0');
  while (sessionsList.firstChild) sessionsList.removeChild(sessionsList.firstChild);
  if (rows.length === 0) {
    const empty = allSessions.length === 0
      ? 'no conversation logs in the archive yet'
      : 'nothing in the volume matches that';
    sessionsList.appendChild(buildPlaceholder(empty, txFilterText ? `filter: "${txFilterText}"` : ''));
    return;
  }
  for (const s of rows) sessionsList.appendChild(buildSessionRow(s));
  for (const row of sessionsList.querySelectorAll('.session-row')) {
    row.addEventListener('click', () => openTranscript(row.dataset.id));
    row.addEventListener('keydown', e => {
      if (e.key === 'Enter' || e.key === ' ') {
        e.preventDefault();
        openTranscript(row.dataset.id);
      }
    });
  }
}

async function loadSessions() {
  while (sessionsList.firstChild) sessionsList.removeChild(sessionsList.firstChild);
  sessionsList.appendChild(buildPlaceholder('retrieving the conversation logs', null, 'loading'));
  try {
    const r = await fetch(`/transcripts/sessions?tenant=${encodeURIComponent(TENANT)}`);
    if (!r.ok) throw new Error(`HTTP ${r.status}: ${(await r.text()).slice(0, 160)}`);
    allSessions = await r.json();
    renderSessions();
  } catch (e) {
    while (sessionsList.firstChild) sessionsList.removeChild(sessionsList.firstChild);
    sessionsList.appendChild(buildPlaceholder('could not reach the conversation logs', e.message, 'error'));
  }
}

let txSearchTimer = null;
txSearch.addEventListener('input', () => {
  clearTimeout(txSearchTimer);
  txSearchTimer = setTimeout(() => {
    txFilterText = txSearch.value.trim().toLowerCase();
    renderSessions();
  }, 120);
});

// ─── transcript drawer (block stream) ──────────────────────────

const TYPE_GLYPHS = {
  text: '✦', thinking: '≈', tool_use: '▣', tool_result: '↳', image: '◧',
};

function buildBlock(b) {
  const role = (b.role || 'unknown').toLowerCase();
  const type = (b.block_type || 'text').toLowerCase();
  const wrap = el('div', { class: `block ${type}` });

  const stripe = el('div', { class: 'block-stripe' });
  stripe.appendChild(el('span', { class: `role ${role}`, text: role }));
  stripe.appendChild(el('span', { class: 'type', text: type.replace('_', ' ') }));
  stripe.appendChild(el('span', { class: 'when', text: fmtDateShort(b.created_at) }));
  wrap.appendChild(stripe);

  const body = el('div', { class: 'block-body' });
  const glyph = TYPE_GLYPHS[type] || '·';
  body.appendChild(el('span', { class: 'glyph', text: glyph }));

  if (type === 'tool_use' || type === 'tool_result') {
    if (b.tool_name) {
      body.appendChild(el('span', {
        class: 'tool-name',
        text: b.tool_name + (b.tool_use_id ? ` (${String(b.tool_use_id).slice(0, 12)})` : ''),
      }));
    }
    body.appendChild(el('div', { class: 'mono', text: b.content || '' }));
  } else if (type === 'thinking' || type === 'text') {
    body.appendChild(el('span', { class: 'text', text: b.content || '' }));
  } else {
    body.appendChild(el('div', { class: 'mono', text: b.content || '(no content)' }));
  }
  wrap.appendChild(body);
  return wrap;
}

function buildTranscriptMeta(blocks, sessionId) {
  while (transcriptMeta.firstChild) transcriptMeta.removeChild(transcriptMeta.firstChild);
  if (blocks.length === 0) { transcriptMeta.hidden = true; return; }
  transcriptMeta.hidden = false;
  const first = blocks[0];
  const last = blocks[blocks.length - 1];
  for (const [k, v] of [
    ['session', sessionId],
    ['agent',   first.caller_agent || '—'],
    ['blocks',  String(blocks.length)],
    ['span',    `${fmtDateShort(first.created_at)}  →  ${fmtDateShort(last.created_at)}`],
    ['source',  first.transcript_path || '—'],
  ]) {
    transcriptMeta.appendChild(el('dt', { text: k }));
    const dd = el('dd');
    dd.appendChild(el('code', { text: v }));
    transcriptMeta.appendChild(dd);
  }
}

async function openTranscript(sessionId) {
  if (!sessionId) return;
  openTranscriptSession = sessionId;
  transcriptBg.classList.add('open');
  transcriptBg.setAttribute('aria-hidden', 'false');
  transcriptMeta.hidden = true;
  while (transcriptBody.firstChild) transcriptBody.removeChild(transcriptBody.firstChild);
  transcriptBody.appendChild(buildPlaceholder('unbinding the volume', sessionId, 'loading'));
  try {
    const r = await fetch(`/transcripts?session_id=${encodeURIComponent(sessionId)}&tenant=${encodeURIComponent(TENANT)}`);
    if (!r.ok) throw new Error(`HTTP ${r.status}: ${(await r.text()).slice(0, 160)}`);
    const data = await r.json();
    if (openTranscriptSession !== sessionId) return;
    const blocks = data.messages || [];
    while (transcriptBody.firstChild) transcriptBody.removeChild(transcriptBody.firstChild);
    buildTranscriptMeta(blocks, sessionId);
    if (blocks.length === 0) {
      transcriptBody.appendChild(buildPlaceholder('the volume is empty', sessionId));
      return;
    }
    const frag = document.createDocumentFragment();
    for (const b of blocks) frag.appendChild(buildBlock(b));
    transcriptBody.appendChild(frag);
    transcriptBody.scrollTop = 0;
  } catch (e) {
    while (transcriptBody.firstChild) transcriptBody.removeChild(transcriptBody.firstChild);
    transcriptBody.appendChild(buildPlaceholder('could not retrieve the volume', e.message, 'error'));
  }
}

function closeTranscript() {
  transcriptBg.classList.remove('open');
  transcriptBg.setAttribute('aria-hidden', 'true');
  openTranscriptSession = null;
}

transcriptClose.addEventListener('click', closeTranscript);
transcriptBg.addEventListener('click', e => { if (e.target === transcriptBg) closeTranscript(); });

// extend Esc priority cascade: hard-delete > archive-confirm > detail
// > transcript drawer. Earlier listener handles the first three; this
// one is last so transcript only closes when no higher-priority overlay
// is open.
document.addEventListener('keydown', e => {
  if (e.key !== 'Escape') return;
  if (deleteModal.classList.contains('open')) return;
  if (modal.classList.contains('open')) return;
  if (detailBg.classList.contains('open')) return;
  if (transcriptBg.classList.contains('open')) closeTranscript();
});

load();
