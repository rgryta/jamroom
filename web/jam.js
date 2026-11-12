// Helpers shared by every jamroom page.
window.Jam = (() => {
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text != null) n.textContent = text;
    return n;
  };

  // ---- icons: one set, one stroke weight, all the same size ----
  const NS = 'http://www.w3.org/2000/svg';
  const ICONS = {
    play: ['fill', 'M8 5v14l11-7z'],
    pause: ['fill', 'M7 5h3.5v14H7zM13.5 5H17v14h-3.5z'],
    skip: ['fill', 'M6 5l9 7-9 7zM16 5h2.5v14H16z'],
    x: ['stroke', 'M6 6l12 12M18 6L6 18'],
    plus: ['stroke', 'M12 5v14M5 12h14'],
    speaker: ['stroke', 'M4 9v6h4l5 4V5L8 9zM16.5 9a4 4 0 010 6M19 6.5a8 8 0 010 11'],
    bars: ['stroke', 'M6 10v4M10 6v12M14 8v8M18 11v2'],
    copy: ['stroke', 'M9 9h10v10H9zM5 15V5h10'],
  };
  const icon = (name) => {
    const [kind, d] = ICONS[name];
    const svg = document.createElementNS(NS, 'svg');
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('class', 'icon');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(NS, 'path');
    path.setAttribute('d', d);
    if (kind === 'fill') path.setAttribute('fill', 'currentColor');
    else for (const [k, v] of [['fill', 'none'], ['stroke', 'currentColor'], ['stroke-width', '2'], ['stroke-linecap', 'round'], ['stroke-linejoin', 'round']]) path.setAttribute(k, v);
    svg.append(path);
    return svg;
  };
  const setIcon = (button, name) => button.replaceChildren(icon(name));
  const iconButton = (name, label, cls) => {
    const b = el('button', 'icon-btn' + (cls ? ' ' + cls : ''));
    b.type = 'button';
    b.setAttribute('aria-label', label);
    b.title = label;
    b.append(icon(name));
    return b;
  };

  const hash = (s) => {
    let h = 2166136261;
    for (const c of s) { h ^= c.codePointAt(0); h = Math.imul(h, 16777619); }
    return h >>> 0;
  };

  // Artwork for a song: its cover when it has one, otherwise a quiet gradient and a monogram. The
  // colours come from the title and artist, so the same song always looks the same.
  const sleeve = (t, cls) => {
    const box = el('div', 'sleeve' + (cls ? ' ' + cls : ''));
    box.setAttribute('aria-hidden', 'true');
    const h = hash((t.artist || '') + '|' + (t.title || ''));
    const hue = h % 360, hue2 = (hue + 25 + ((h >>> 9) % 40)) % 360, angle = 120 + ((h >>> 17) % 90);
    box.style.background = `radial-gradient(circle at 25% 20%, rgba(255,255,255,.2), transparent 55%), linear-gradient(${angle}deg, hsl(${hue} 42% 44%), hsl(${hue2} 48% 28%))`;
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

  // A song's status, shown only when it is not simply ready: waiting, fetching or failed.
  const stateTag = (t) => {
    if (t.status === 'failed') return el('span', 'state failed', t.error || 'Failed');
    if (t.status === 'working') return el('span', 'state working', 'Fetching');
    if (t.status === 'queued') return el('span', 'state', 'Waiting');
    return null;
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

  // The rooms this browser has joined lately (newest first, at most eight), kept in this browser only.
  const rooms = (() => {
    const KEY = 'jam-rooms';
    const read = () => {
      try {
        const v = JSON.parse(localStorage.getItem(KEY) || '[]');
        return Array.isArray(v) ? v.filter((r) => r && typeof r.code === 'string' && typeof r.name === 'string') : [];
      } catch (e) { return []; }
    };
    const write = (list) => { try { localStorage.setItem(KEY, JSON.stringify(list.slice(0, 8))); } catch (e) {} };
    return {
      list: read,
      remember: (code, name) => write([{ code, name: name || code, at: Date.now() }, ...read().filter((r) => r.code !== code)]),
      forget: (code) => write(read().filter((r) => r.code !== code)),
    };
  })();

  // "5 min ago", "2 h ago", "yesterday".
  const ago = (t) => {
    const m = Math.round((Date.now() - t) / 60000);
    if (m < 1) return 'just now';
    if (m < 60) return `${m} min ago`;
    const h = Math.round(m / 60);
    if (h < 24) return `${h} h ago`;
    const d = Math.round(h / 24);
    return d === 1 ? 'yesterday' : `${d} days ago`;
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

  // The Autoplay button and its panel for one room. `button` goes wherever the page wants it; the
  // panel is a dialog that opens from it. `onState` receives the room state the server answers
  // with. `update(room)` shows what the server says, except just after a local change.
  const radio = (api, onState) => {
    let touched = -Infinity, current = null;
    const send = (body) => {
      touched = performance.now();
      return api('/settings', 'POST', body).then(onState).catch(() => {});
    };

    const button = el('button', 'pill');
    button.type = 'button';
    button.setAttribute('aria-haspopup', 'dialog');
    const buttonText = el('span', null, 'Autoplay');
    button.append(icon('bars'), buttonText);

    const dlg = el('dialog', 'sheet');
    dlg.setAttribute('aria-labelledby', 'radio-title');
    const head = el('div', 'sheet-head');
    const title = el('h2', null, 'Autoplay');
    title.id = 'radio-title';
    const close = iconButton('x', 'Close');
    head.append(title, close);

    const auto = el('label', 'switch');
    const autoInput = el('input');
    autoInput.type = 'checkbox';
    autoInput.setAttribute('role', 'switch');
    auto.append(el('span', 'switch-label', 'Keep playing when the queue is empty'), autoInput, el('span', 'switch-track'));

    const panel = el('div', 'mood');
    panel.hidden = true;
    const moodTitle = el('p', 'mood-title', 'Keep the mood');
    moodTitle.id = 'mood-title';
    const hint = el('p', 'mood-hint', 'Follow what just played, or leave all off for a random mix.');
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
    const groupName = 'close-' + Math.random().toString(36).slice(2);
    const segment = el('div', 'segment');
    segment.setAttribute('role', 'radiogroup');
    segment.setAttribute('aria-label', 'How close to follow it');
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
    panel.append(moodTitle, hint, chips, closeRow, note, learning);
    dlg.append(head, auto, panel);
    document.body.append(dlg);

    const refreshNote = () => {
      const on = buttons.filter(([, b]) => b.getAttribute('aria-pressed') === 'true').length;
      note.textContent = !on ? 'Random mix.' : current && current.mood_note ? 'Following: ' + current.mood_note : 'Following the next songs as they play…';
    };
    const paint = (on) => {
      button.classList.toggle('on', on);
      button.setAttribute('aria-label', `Autoplay, ${on ? 'on' : 'off'}`);
      panel.hidden = !on;
    };

    button.addEventListener('click', () => dlg.showModal());
    close.addEventListener('click', () => dlg.close());
    dlg.addEventListener('click', (e) => { if (e.target === dlg) dlg.close(); });
    autoInput.addEventListener('change', () => { paint(autoInput.checked); send({ autoplay: autoInput.checked }); });

    const update = (room) => {
      if (!room || performance.now() - touched < 2500) return;
      current = room;
      autoInput.checked = !!room.autoplay;
      paint(!!room.autoplay);
      buttons.forEach(([key, b]) => b.setAttribute('aria-pressed', String(!!(room.mood && room.mood[key]))));
      radios.forEach(([value, input]) => { input.checked = room.close === value; });
      refreshNote();
      const t = room.tempo;
      learning.hidden = !(room.mood && room.mood.tempo && t && t.known < t.total);
      if (!learning.hidden) {
        learning.textContent = `Tempo is measured in the background: ${t.known.toLocaleString()} of ${t.total.toLocaleString()} songs so far.`;
      }
    };
    return { button, update };
  };

  return { el, icon, setIcon, iconButton, sleeve, roomApi, clock, stateTag, every, onWake, rooms, ago, radio };
})();
