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

  const STATUS = { queued: 'Waiting', working: 'Fetching', ready: 'Ready', playing: 'Playing', played: 'Played', failed: 'Failed' };
  const chip = (t) => el('span', 'chip ' + (t.status === 'queued' ? '' : t.status), STATUS[t.status] || t.status);

  return { el, sleeve, roomApi, clock, chip };
})();
