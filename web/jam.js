// Helpers shared by every jamroom page.
window.Jam = (() => {
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  };

  const hash = (s) => {
    let h = 2166136261;
    for (const c of s) { h ^= c.codePointAt(0); h = Math.imul(h, 16777619); }
    return h >>> 0;
  };

  // Artwork for a song: its cover when it has one, otherwise a sleeve whose colours and pattern
  // come from the title and artist, so the same song always looks the same.
  const sleeve = (t, cls) => {
    const box = el('div', 'sleeve' + (cls ? ' ' + cls : ''));
    box.setAttribute('aria-hidden', 'true');
    const h = hash((t.artist || '') + '|' + (t.title || ''));
    const h1 = h % 360, h2 = (h1 + 40 + ((h >>> 9) % 80)) % 360;
    const a = `hsl(${h1} 70% 52%)`, b = `hsl(${h2} 75% 32%)`;
    box.style.background = [
      `linear-gradient(135deg, ${a}, ${b})`,
      `radial-gradient(circle at 30% 30%, ${a} 0 28%, ${b} 29%)`,
      `conic-gradient(from ${h % 90}deg, ${a}, ${b}, ${a})`,
      `repeating-linear-gradient(45deg, ${a} 0 14px, ${b} 14px 28px)`,
    ][(h >>> 17) % 4];
    box.append(el('span', 'sleeve-letter', (Array.from((t.title || '?').trim())[0] || '?').toUpperCase()));
    if (t.cover) {
      const img = el('img');
      img.alt = ''; img.loading = 'lazy'; img.referrerPolicy = 'no-referrer'; img.src = t.cover;
      img.onerror = () => img.remove();
      box.append(img);
    }
    return box;
  };

  // JSON calls for one room. Errors carry the server's message and HTTP status.
  const roomApi = (code) => async (path, method = 'GET', body) => {
    const r = await fetch(`/api/r/${code}${path}`, {
      method,
      headers: body ? { 'content-type': 'application/json' } : undefined,
      body: body ? JSON.stringify(body) : undefined,
    });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw Object.assign(new Error(j.error || r.statusText), { status: r.status });
    return j;
  };

  const clock = (s) => {
    s = Math.max(0, Math.floor(s || 0));
    return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
  };

  // A repeating timer that keeps its pace in a hidden tab. Browsers slow page timers right down
  // once a tab has been hidden for a few minutes (and a tab with nothing playing is not
  // exempt), but a worker's timer is left alone. Falls back to a plain timer.
  const every = (ms, fn) => {
    try {
      const w = new Worker('/assets/tick.js');
      w.onmessage = fn;
      w.postMessage(ms);
    } catch (e) {
      setInterval(fn, ms);
    }
  };

  // Runs `fn` when the page comes back: tab shown again, phone unlocked, network restored.
  const onWake = (fn) => {
    document.addEventListener('visibilitychange', () => { if (document.visibilityState === 'visible') fn(); });
    window.addEventListener('pageshow', fn);
    window.addEventListener('online', fn);
  };

  // What Autoplay can follow when it keeps the mood.
  const FOLLOW = [
    ['genre', 'Genre', 'Songs from the same genres'],
    ['era', 'Era', 'Songs from the same years'],
    ['artist', 'Artist', 'More from the same artists'],
    ['language', 'Language', 'Songs in the same language'],
    ['tempo', 'Tempo', 'Songs at a similar pace'],
  ];
  const CLOSE = [['loose', 'Loose'], ['balanced', 'Balanced'], ['tight', 'Tight']];

  // The Autoplay switch and, while it is on, the mood panel for one room. `onState` receives the
  // room state the server answers with. `update(room)` shows what the server says, except just
  // after a local change.
  const switches = (api, onState) => {
    let touched = -Infinity;
    const send = (body) => {
      touched = performance.now();
      return api('/settings', 'POST', body).then(onState).catch(() => {});
    };

    const auto = el('label', 'switch');
    const autoInput = el('input');
    autoInput.type = 'checkbox';
    autoInput.setAttribute('role', 'switch');
    const autoText = el('span', 'switch-text');
    autoText.append(el('span', 'switch-label', 'Autoplay'), el('span', 'switch-hint', 'When the queue is empty, keep playing songs from the library.'));
    auto.append(autoInput, el('span', 'switch-track'), autoText);
    autoInput.addEventListener('change', () => {
      panel.hidden = !autoInput.checked;
      send({ autoplay: autoInput.checked });
    });

    const panel = el('div', 'mood');
    panel.hidden = true;
    const title = el('p', 'mood-title', 'Keep the mood');
    title.id = 'mood-title';
    const hint = el('p', 'mood-hint', 'Pick songs like the ones that just played. Choose what to follow, or leave all off for a random mix.');
    const chips = el('div', 'picks');
    chips.setAttribute('role', 'group');
    chips.setAttribute('aria-labelledby', 'mood-title');
    const buttons = FOLLOW.map(([key, label, tip]) => {
      const b = el('button', 'pick', label);
      b.type = 'button';
      b.title = tip;
      b.setAttribute('aria-pressed', 'false');
      b.addEventListener('click', () => {
        const on = b.getAttribute('aria-pressed') !== 'true';
        b.setAttribute('aria-pressed', String(on));
        send({ mood: { [key]: on } });
        refreshNote();
      });
      chips.append(b);
      return [key, b];
    });

    const segment = el('div', 'segment');
    segment.setAttribute('role', 'radiogroup');
    segment.setAttribute('aria-label', 'How close to follow it');
    const groupName = 'close-' + Math.random().toString(36).slice(2);
    const radios = CLOSE.map(([value, label]) => {
      const lab = el('label');
      const input = el('input');
      input.type = 'radio';
      input.name = groupName;
      input.value = value;
      input.addEventListener('change', () => send({ close: value }));
      lab.append(input, el('span', null, label));
      segment.append(lab);
      return [value, input];
    });
    const closeRow = el('div', 'close-row');
    closeRow.append(el('span', 'mood-hint', 'How close'), segment);

    const note = el('p', 'mood-note');
    note.setAttribute('aria-live', 'polite');
    const learning = el('p', 'mood-hint');
    panel.append(title, hint, chips, closeRow, note, learning);

    let current = null;
    const refreshNote = () => {
      const on = buttons.filter(([, b]) => b.getAttribute('aria-pressed') === 'true').length;
      note.textContent = !on ? 'Random mix: any song from the library.' : current && current.mood_note ? 'Following: ' + current.mood_note : 'Following the next songs as they play…';
    };

    const node = el('div', 'switches');
    node.append(auto, panel);
    const update = (room) => {
      if (!room || performance.now() - touched < 2500) return;
      current = room;
      autoInput.checked = !!room.autoplay;
      panel.hidden = !room.autoplay;
      buttons.forEach(([key, b]) => b.setAttribute('aria-pressed', String(!!(room.mood && room.mood[key]))));
      radios.forEach(([value, input]) => { input.checked = room.close === value; });
      refreshNote();
      const t = room.tempo;
      learning.hidden = !(room.mood && room.mood.tempo && t && t.known < t.total);
      if (!learning.hidden) {
        learning.textContent = `Tempo is measured in the background: ${t.known.toLocaleString()} of ${t.total.toLocaleString()} songs so far. Songs without a measured tempo are skipped by this option.`;
      }
    };
    return { node, update };
  };

  const STATUS = { queued: 'Waiting', working: 'Fetching', ready: 'Ready', playing: 'Playing', played: 'Played', failed: 'Failed' };
  const chip = (t) => el('span', 'chip ' + (t.status === 'queued' ? '' : t.status), STATUS[t.status] || t.status);

  return { el, sleeve, roomApi, clock, chip, every, onWake, switches };
})();
