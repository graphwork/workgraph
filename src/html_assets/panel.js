/* ───────────────────────────────────────────────────────────────────────────
   Workgraph HTML viewer — interaction layer.

   Loaded once per page. Reads:
   - window.WG_TASKS  : { task_id: <task detail object>, ... }
   - window.WG_EDGES  : { task_id: { up: [ids], down: [ids] }, ... }
                        Pre-computed reachable upstream + downstream sets per
                        task. Used to apply highlight classes to edge / node
                        spans on selection.
   - window.WG_CYCLES : { task_id: [member_ids...] }   (optional)

   Behavior:
   1. Click a `.task-link` (or anywhere with [data-task-id]) → open the side
      panel with that task's details and decorate the viz with edge / node
      highlight classes (upstream = magenta, downstream = cyan, cycle = yellow).
   2. ESC or click outside → close the panel and clear highlights.
   3. Theme toggle in the page header — auto-follow OS preference until the
      user clicks the toggle, then switch to explicit dark/light mode and
      persist the choice via localStorage.
   ─────────────────────────────────────────────────────────────────────────── */

(function () {
    'use strict';

    var tasks = window.WG_TASKS || {};
    var edgeMap = window.WG_EDGES || {};
    var cycleMap = window.WG_CYCLES || {};

    var panel = document.getElementById('side-panel');
    var panelContent = document.getElementById('panel-content');
    var closeBtn = document.getElementById('panel-close');
    var themeToggle = document.getElementById('theme-toggle');
    var vizPre = document.querySelector('.viz-pre');

    // ── Helpers ─────────────────────────────────────────────────────────

    function escapeHtml(s) {
        return String(s)
            .replace(/&/g, '&amp;')
            .replace(/</g, '&lt;')
            .replace(/>/g, '&gt;')
            .replace(/"/g, '&quot;')
            .replace(/'/g, '&#39;');
    }

    function statusCls(s) {
        return String(s || '').replace(/\s+/g, '-').toLowerCase();
    }

    // ── Theme management ────────────────────────────────────────────────

    var STORAGE_KEY = 'wg-html-theme';

    function applyTheme(mode) {
        // mode: 'dark' | 'light' | 'auto'
        if (mode === 'dark' || mode === 'light') {
            document.documentElement.setAttribute('data-theme', mode);
        } else {
            document.documentElement.removeAttribute('data-theme');
        }
        updateToggleLabel();
    }

    function effectiveTheme() {
        var explicit = document.documentElement.getAttribute('data-theme');
        if (explicit) return explicit;
        if (window.matchMedia &&
            window.matchMedia('(prefers-color-scheme: dark)').matches) {
            return 'dark';
        }
        return 'light';
    }

    function updateToggleLabel() {
        if (!themeToggle) return;
        var current = effectiveTheme();
        themeToggle.textContent = current === 'dark' ? 'Light theme' : 'Dark theme';
        themeToggle.setAttribute('aria-label',
            'Switch to ' + (current === 'dark' ? 'light' : 'dark') + ' theme');
    }

    function initTheme() {
        var saved;
        try { saved = localStorage.getItem(STORAGE_KEY); } catch (_) { saved = null; }
        if (saved === 'dark' || saved === 'light') {
            applyTheme(saved);
            return;
        }
        applyTheme('auto');
        // Auto-follow OS prefers-color-scheme until the user explicitly toggles.
        if (window.matchMedia) {
            var mq = window.matchMedia('(prefers-color-scheme: dark)');
            var listener = function () { updateToggleLabel(); };
            if (mq.addEventListener) mq.addEventListener('change', listener);
            else if (mq.addListener) mq.addListener(listener);  // Safari < 14
        }
    }

    if (themeToggle) {
        themeToggle.addEventListener('click', function () {
            var current = effectiveTheme();
            var next = current === 'dark' ? 'light' : 'dark';
            applyTheme(next);
            try { localStorage.setItem(STORAGE_KEY, next); } catch (_) {}
        });
    }

    initTheme();

    // ── Edge / node highlighting ────────────────────────────────────────

    /**
     * Apply highlight classes to edge spans + task-link spans matching the
     * upstream / downstream / cycle sets for the selected task.
     */
    function applyHighlight(selectedId) {
        if (!vizPre) return;

        // Clear any existing highlight classes.
        clearHighlight();

        if (!selectedId || !tasks[selectedId]) {
            document.body.removeAttribute('data-selected');
            return;
        }
        document.body.setAttribute('data-selected', selectedId);

        var info = edgeMap[selectedId] || { up: [], down: [] };
        var upSet = {}; (info.up || []).forEach(function (id) { upSet[id] = true; });
        var downSet = {}; (info.down || []).forEach(function (id) { downSet[id] = true; });
        var cycleSet = {};
        (cycleMap[selectedId] || []).forEach(function (id) { cycleSet[id] = true; });

        // Tag every task-link with selection / upstream / downstream classes.
        var links = vizPre.querySelectorAll('.task-link');
        for (var i = 0; i < links.length; i++) {
            var el = links[i];
            var id = el.getAttribute('data-task-id');
            if (id === selectedId) {
                el.classList.add('is-selected');
            } else if (upSet[id]) {
                el.classList.add('is-upstream');
            } else if (downSet[id]) {
                el.classList.add('is-downstream');
            }
        }

        // Tag every edge cell where (from, to) lies in the upstream / downstream
        // / cycle chain of the selection. Matches the TUI priority order:
        // cycle > upstream > downstream (render.rs:1500).
        var inUp = function (id) { return id === selectedId || upSet[id] === true; };
        var inDown = function (id) { return id === selectedId || downSet[id] === true; };
        var inCycle = function (id) { return cycleSet[id] === true; };

        var edges = vizPre.querySelectorAll('.edge');
        for (var j = 0; j < edges.length; j++) {
            var ee = edges[j];
            var raw = ee.getAttribute('data-edges') || '';
            if (!raw) continue;
            var pairs = raw.split('|');
            var isCycle = false, isUp = false, isDown = false;
            for (var k = 0; k < pairs.length; k++) {
                var pair = pairs[k];
                var sep = pair.indexOf('>');
                if (sep < 0) continue;
                var from = pair.substring(0, sep);
                var to = pair.substring(sep + 1);
                if (inCycle(from) && inCycle(to)) { isCycle = true; }
                if (inUp(from) && inUp(to)) { isUp = true; }
                if (inDown(from) && inDown(to)) { isDown = true; }
            }
            if (isCycle) ee.classList.add('is-cycle');
            else if (isUp) ee.classList.add('is-upstream');
            else if (isDown) ee.classList.add('is-downstream');
        }
    }

    function clearHighlight() {
        if (!vizPre) return;
        var classed = vizPre.querySelectorAll(
            '.is-selected, .is-upstream, .is-downstream, .is-cycle'
        );
        for (var i = 0; i < classed.length; i++) {
            classed[i].classList.remove('is-selected', 'is-upstream', 'is-downstream', 'is-cycle');
        }
        document.body.removeAttribute('data-selected');
    }

    // ── Side panel rendering ────────────────────────────────────────────

    function renderPanel(task) {
        var h = '';
        h += '<div class="panel-header">';
        h += '<code class="panel-id">' + escapeHtml(task.id) + '</code>';
        h += '<span class="badge ' + statusCls(task.status) + '">' + escapeHtml(task.status) + '</span>';
        h += '</div>';
        h += '<p class="panel-title">' + escapeHtml(task.title || '(untitled)') + '</p>';

        // Meta block: model / agent / executor / loop / timestamps.
        var metaBits = [];
        if (task.exec) metaBits.push('<strong>Exec:</strong> <code>' + escapeHtml(task.exec) + '</code>');
        if (task.model) metaBits.push('<strong>Model:</strong> <code>' + escapeHtml(task.model) + '</code>');
        if (task.agent) {
            var shown = task.agent.length > 12 ? task.agent.slice(0, 8) + '…' : task.agent;
            metaBits.push('<strong>Agent:</strong> <code>' + escapeHtml(shown) + '</code>');
        }
        if (typeof task.loop_iteration === 'number' && task.loop_iteration > 0) {
            metaBits.push('<strong>Iteration:</strong> ' + escapeHtml(String(task.loop_iteration)));
        }
        if (task.created_at) metaBits.push('<strong>Created:</strong> ' + escapeHtml(task.created_at.slice(0, 19).replace('T', ' ')));
        if (task.started_at) metaBits.push('<strong>Started:</strong> ' + escapeHtml(task.started_at.slice(0, 19).replace('T', ' ')));
        if (task.completed_at) metaBits.push('<strong>Completed:</strong> ' + escapeHtml(task.completed_at.slice(0, 19).replace('T', ' ')));
        if (metaBits.length) {
            h += '<p class="panel-meta">' + metaBits.join(' &middot; ') + '</p>';
        }

        if (task.tags && task.tags.length) {
            h += '<div class="panel-tags">';
            for (var i = 0; i < task.tags.length; i++) {
                h += '<code>' + escapeHtml(task.tags[i]) + '</code>';
            }
            h += '</div>';
        }

        if (task.description) {
            h += '<details open><summary>Description</summary>';
            h += '<pre class="panel-desc">' + escapeHtml(task.description) + '</pre>';
            h += '</details>';
        }

        if (task.failure_reason) {
            h += '<details open><summary>Failure reason</summary>';
            h += '<pre class="panel-desc">' + escapeHtml(task.failure_reason) + '</pre>';
            h += '</details>';
        }

        if (task.after && task.after.length) {
            h += '<details open><summary>Depends on (' + task.after.length + ')</summary><ul class="panel-deps">';
            for (var j = 0; j < task.after.length; j++) {
                var depId = task.after[j];
                var dep = tasks[depId];
                h += '<li>';
                if (dep) {
                    h += '<a href="#" class="dep-link" data-task-id="' + escapeHtml(depId) + '">';
                    h += '<span class="badge ' + statusCls(dep.status) + '">' + escapeHtml(dep.status) + '</span>';
                    h += '<code>' + escapeHtml(depId) + '</code></a>';
                } else {
                    h += '<code>' + escapeHtml(depId) + '</code>';
                    h += ' <span class="muted">(not visible)</span>';
                }
                h += '</li>';
            }
            h += '</ul></details>';
        }

        if (task.before && task.before.length) {
            h += '<details><summary>Required by (' + task.before.length + ')</summary><ul class="panel-deps">';
            for (var k = 0; k < task.before.length; k++) {
                var depId2 = task.before[k];
                var dep2 = tasks[depId2];
                h += '<li>';
                if (dep2) {
                    h += '<a href="#" class="dep-link" data-task-id="' + escapeHtml(depId2) + '">';
                    h += '<span class="badge ' + statusCls(dep2.status) + '">' + escapeHtml(dep2.status) + '</span>';
                    h += '<code>' + escapeHtml(depId2) + '</code></a>';
                } else {
                    h += '<code>' + escapeHtml(depId2) + '</code>';
                }
                h += '</li>';
            }
            h += '</ul></details>';
        }

        if (typeof task.eval_score === 'number') {
            h += '<details open><summary>Evaluation</summary>';
            h += '<p class="eval-score">' + task.eval_score.toFixed(2) + '</p>';
            if (task.eval_dims) {
                h += '<table class="eval-dims"><tbody>';
                var keys = Object.keys(task.eval_dims).sort();
                for (var d = 0; d < keys.length; d++) {
                    var dim = keys[d];
                    var v = task.eval_dims[dim];
                    h += '<tr><td>' + escapeHtml(dim.replace(/_/g, ' ')) + '</td>';
                    h += '<td class="eval-dim-val">' + (typeof v === 'number' ? v.toFixed(2) : escapeHtml(String(v))) + '</td></tr>';
                }
                h += '</tbody></table>';
            }
            h += '</details>';
        }

        if (task.log && task.log.length) {
            h += '<details><summary>Log (' + task.log.length + ' entr' + (task.log.length === 1 ? 'y' : 'ies') + ')</summary><ul class="panel-log">';
            for (var l = 0; l < task.log.length; l++) {
                var entry = task.log[l];
                var ts = entry.timestamp ? entry.timestamp.slice(0, 19).replace('T', ' ') : '';
                h += '<li><span class="log-ts">' + escapeHtml(ts) + '</span> ' + escapeHtml(entry.message) + '</li>';
            }
            h += '</ul></details>';
        }

        if (task.detail_href) {
            h += '<p class="panel-deeplink"><a href="' + escapeHtml(task.detail_href) + '">View full task page →</a></p>';
        }

        panelContent.innerHTML = h;

        // Wire dep-link clicks to navigate inside the overlay.
        var deps = panelContent.querySelectorAll('.dep-link');
        for (var dd = 0; dd < deps.length; dd++) {
            (function (a) {
                a.addEventListener('click', function (ev) {
                    ev.preventDefault();
                    openTask(a.getAttribute('data-task-id'));
                });
            })(deps[dd]);
        }
    }

    function openTask(id) {
        var task = tasks[id];
        if (!task) return;
        renderPanel(task);
        panel.classList.add('is-open');
        applyHighlight(id);
    }

    function closePanel() {
        panel.classList.remove('is-open');
        clearHighlight();
    }

    // ── Wire viz click handlers ─────────────────────────────────────────

    // Use event delegation on the viz container so dynamically-rendered nodes
    // also work. This handles both .task-link span clicks and clicks on any
    // descendant (status glyph, etc.).
    if (vizPre) {
        vizPre.addEventListener('click', function (ev) {
            var t = ev.target;
            // Walk up to the nearest .task-link.
            while (t && t !== vizPre) {
                if (t.classList && t.classList.contains('task-link')) {
                    var id = t.getAttribute('data-task-id');
                    if (id) {
                        ev.preventDefault();
                        openTask(id);
                        return;
                    }
                }
                t = t.parentNode;
            }
        });
    }

    // Also wire generic [data-task-id] elsewhere on the page (legend, list).
    document.body.addEventListener('click', function (ev) {
        var t = ev.target;
        while (t && t !== document.body) {
            if (t.dataset && t.dataset.taskId && !t.classList.contains('task-link')) {
                if (t.tagName === 'A' && t.getAttribute('href') &&
                    !t.classList.contains('inline-task-link')) {
                    return;  // let normal nav happen
                }
                openTask(t.dataset.taskId);
                return;
            }
            t = t.parentNode;
        }
    });

    if (closeBtn) closeBtn.addEventListener('click', closePanel);

    document.addEventListener('keydown', function (ev) {
        if (ev.key === 'Escape') closePanel();
    });

    // Click outside the panel = close (but only when it's open).
    document.addEventListener('click', function (ev) {
        if (!panel.classList.contains('is-open')) return;
        if (panel.contains(ev.target)) return;
        // Don't close on task-link clicks (they reopen the panel anyway).
        var t = ev.target;
        while (t && t !== document.body) {
            if (t.classList && t.classList.contains('task-link')) return;
            if (t.dataset && t.dataset.taskId) return;
            t = t.parentNode;
        }
        closePanel();
    });
})();
