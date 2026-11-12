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

  // The Autoplay and "keep the mood" switches for one room. `onState` receives the room state the
  // server answers with. `update(room)` shows what the server says, except just after a local change.
  const switches = (api, onState) => {
    let touched = -Infinity;
    const make = (label, hint) => {
      const lab = el('label', 'switch');
      const input = el('input');
      input.type = 'checkbox';
      input.setAttribute('role', 'switch');
      const text = el('span', 'switch-text');
      text.append(el('span', 'switch-label', label), el('span', 'switch-hint', hint));
      lab.append(input, el('span', 'switch-track'), text);
      return { lab, input };
    };
    const auto = make('Autoplay', 'When the queue is empty, keep playing songs from the library.');
    const mood = make('Keep the mood', 'Pick songs like the ones that just played, not at random.');
    const node = el('div', 'switches');
    node.append(auto.lab, mood.lab);
    const send = (body) => {
      touched = performance.now();
      api('/settings', 'POST', body).then(onState).catch(() => {});
    };
    auto.input.addEventListener('change', () => send({ autoplay: auto.input.checked }));
    mood.input.addEventListener('change', () => send({ mood: mood.input.checked }));
    const update = (room) => {
      if (!room || performance.now() - touched < 2500) return;
      auto.input.checked = !!room.autoplay;
      mood.input.checked = !!room.mood;
      mood.lab.classList.toggle('dim', !room.autoplay);
    };
    return { node, update };
  };

  const STATUS = { queued: 'Waiting', working: 'Fetching', ready: 'Ready', playing: 'Playing', played: 'Played', failed: 'Failed' };
  const chip = (t) => el('span', 'chip ' + (t.status === 'queued' ? '' : t.status), STATUS[t.status] || t.status);

  return { el, sleeve, roomApi, clock, chip, every, onWake, switches };
})();
