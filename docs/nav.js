// KASUGAI Agent ドキュメント共通ナビゲーション
// （KASUGAI Canvas docs/nav.js 準拠）
// 使い方: <script src="nav.js" defer></script>
// - 左サイドバーにサイト内リンク・外部リンクを生成
// - 現在ページを自動ハイライト
// - JS が無効でも本文は通常表示される（サイドバーだけ出ない）
//
// 正本: このファイルは diagrams/docs/nav.js。docs/nav.js は
//   tools/sync-docs.mjs が nav.json を埋め込んで生成する写し（直接編集禁止）。
//   ナビ構造の正本は diagrams/docs/nav.json。
//   groups[]: {section, sectionPage?, label?, labelPage?, items[]}
//   items[] : {page, text, children?[{page, text}]}
//   demos[] : {href, text}（http(s) は外部リンクとして新規タブで開く）
//     または {action:"launch", target, port, text}（control API で環境起動）
(function () {
  const base = new URL('.', document.currentScript.src).pathname;
  const NAV = {"site":"KASUGAI Agent マニュアル","version":"v0.2.0","groups":[{"section":"概要","items":[{"href":"index.html","text":"1-1 概要","children":[]}]},{"section":"使い方","label":"パネル","items":[{"href":"2-1-panel.html","text":"2-1 パネル構成","children":[]},{"href":"2-2-chat.html","text":"2-2 AIチャット","children":[]},{"href":"2-3-status.html","text":"2-3 処理状況","children":[]},{"href":"2-4-docs.html","text":"2-4 doc browser","children":[]},{"href":"2-5-settings.html","text":"2-5 設定","children":[]},{"href":"2-6-devenv.html","text":"2-6 開発環境","children":[]}]},{"section":"設計","items":[{"href":"3-1-diagrams.html","text":"3-1 統一モデル図","children":[]},{"href":"3-2-autonomy.html","text":"3-2 自律ループと実装方式","children":[]},{"href":"3-3-distribution.html","text":"3-3 配布とアセット更新","children":[]}]}],"demos":[{"href":"../","text":"▶ パネルを開く"},{"action":"launch","target":"prod","port":8586,"text":"▶ 環境を起動"},{"href":"https://github.com/yamamoto-ryuzo/kasugai_agent/issues","text":"🐛 問い合わせ（GitHub Issues）"}]};

  function norm(path) {
    // 末尾の index.html / / と #hash を正規化し、日本語ファイル名用に
    // デコードして比較できる形にする
    let p = path.replace(/#.*$/, '').replace(/index\.html$/, '').replace(/\/$/, '');
    try { p = decodeURIComponent(p); } catch (e) { /* 不正な%列はそのまま */ }
    return p;
  }
  const current = norm(location.pathname);

  function isActive(href) {
    return norm(base + href) === current;
  }

  const nav = document.createElement('aside');
  nav.id = 'site-nav';

  let html = `<div class="nav-title"><a href="${base}index.html">${NAV.site}</a><span class="nav-version">${NAV.version}</span></div>`;

  if (NAV.demos && NAV.demos.length) {
    html += '<div class="nav-demos">';
    for (const d of NAV.demos) {
      if (d.action === 'launch') {
        html += `<a class="nav-demo" href="#" data-launch="${d.target}" data-port="${d.port}" title="稼働中の control が別環境を起動します。全停止時は kasugai-agent:// プロトコル経由で起動を試行し、未登録なら exe または npm run dev（cargo run）を先に起動してください">${d.text}</a>`;
      } else {
        const ext = /^https?:\/\//.test(d.href);
        html += `<a class="nav-demo" href="${ext ? d.href : base + d.href}"${ext ? ' target="_blank" rel="noopener noreferrer"' : ''}>${d.text}</a>`;
      }
    }
    html += '</div>';
  }

  for (const g of NAV.groups) {
    if (g.section) html += g.sectionHref
      ? `<div class="nav-section"><a href="${base}${g.sectionHref}">${g.section}</a></div>`
      : `<div class="nav-section">${g.section}</div>`;
    html += '<div class="nav-group">';
    if (g.label) {
      const lActive = g.labelHref && isActive(g.labelHref) ? ' class="active"' : '';
      html += g.labelHref
        ? `<div class="nav-group-label"><a href="${base}${g.labelHref}"${lActive}>${g.label}</a></div>`
        : `<div class="nav-group-label">${g.label}</div>`;
    }
    html += '<ul>';
    for (const item of g.items) {
      const active = isActive(item.href) ? ' class="active"' : '';
      html += `<li><a href="${base}${item.href}"${active}>${item.text}</a>`;
      if (item.children && item.children.length) {
        html += '<ul class="nav-sub">';
        for (const c of item.children) {
          const cActive = isActive(c.href) ? ' class="active"' : '';
          html += `<li><a href="${base}${c.href}"${cActive}>${c.text}</a></li>`;
        }
        html += '</ul>';
      }
      html += '</li>';
    }
    html += '</ul></div>';
  }

  nav.innerHTML = html;
  document.body.prepend(nav);
  document.body.classList.add('has-nav');

  // サイドバーのスクロール位置をページ遷移間で保持（タブ単位）
  const SCROLL_KEY = 'kasugai-agent-docs-nav-scroll';
  try {
    const savedTop = sessionStorage.getItem(SCROLL_KEY);
    if (savedTop) nav.scrollTop = parseInt(savedTop, 10) || 0;
    let scrollTimer = null;
    nav.addEventListener('scroll', () => {
      if (scrollTimer) clearTimeout(scrollTimer);
      scrollTimer = setTimeout(() => {
        try { sessionStorage.setItem(SCROLL_KEY, String(nav.scrollTop)); } catch (e) { /* 無視 */ }
      }, 80);
    });
  } catch (e) { /* sessionStorage 不可時は位置保持なし */ }

  // モバイル用ハンバーガー
  const btn = document.createElement('button');
  btn.className = 'nav-toggle';
  btn.type = 'button';
  btn.setAttribute('aria-label', 'メニュー');
  btn.textContent = '☰';
  btn.addEventListener('click', () => document.body.classList.toggle('nav-open'));
  document.body.prepend(btn);

  // 環境起動ボタン（demos の action:"launch" 項目）
  // 対象ポートが既に応答していればそのまま開く。未起動なら
  // POST /api/control/launch で起動を依頼する。control API が無い
  // （全停止・旧版・静的配信）場合は kasugai-agent:// プロトコルへ
  // フォールバックし、OS 登録済みのハンドラ（exe --uri）に起動させる。
  // 起動が確認できたら対象ポートを開く
  const probe = (url) =>
    fetch(url + 'api/state', { mode: 'no-cors', cache: 'no-store' })
      .then(() => true)
      .catch(() => false);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  async function launchEnv(el) {
    const port = el.dataset.port || '8586';
    const url = `http://127.0.0.1:${port}/`;
    const orig = el.textContent;
    if (await probe(url)) {
      window.open(url, '_blank', 'noopener');
      return;
    }
    let launched = false;
    try {
      const r = await fetch('/api/control/launch', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ target: el.dataset.launch, port: Number(port) })
      });
      launched = r.ok;
    } catch (e) { /* control API が無い環境 */ }
    // ユーザー操作由来の新規タブを確保してから起動を待つ
    // （起動完了後に window.open すると popup blocker に弾かれるため）
    const w = window.open('about:blank', '_blank');
    el.textContent = '⏳ 起動中…';
    if (w) {
      w.document.write(
        '<p style="font-family:sans-serif;padding:2em">' +
        '起動中…（初回はビルドのため時間がかかります）</p>');
    }
    if (!launched) {
      // kasugai-agent:// プロトコル経由（exe が起動時に HKCU 自己登録）。
      // 未登録なら遷移しても何も起きず、下のポーリングがタイムアウトする
      location.href =
        'kasugai-agent:launch?target=' + encodeURIComponent(el.dataset.launch) +
        '&port=' + encodeURIComponent(port);
    }
    const deadline = Date.now() + 180000;
    let up = false;
    while (Date.now() < deadline) {
      await sleep(1500);
      if (await probe(url)) { up = true; break; }
    }
    el.textContent = orig;
    if (up) {
      if (w && !w.closed) w.location.href = url;
      else window.open(url, '_blank', 'noopener');
      return;
    }
    if (w && !w.closed) w.close();
    // control 未起動・旧版 exe・静的配信・protocol 未登録では起動できない。
    // refused のタブを開く代わりに理由をボタン上に表示する
    el.textContent = launched
      ? '⚠ 起動を確認できません'
      : '⚠ control 未起動（exe または npm run dev を先に起動）';
    setTimeout(() => { el.textContent = orig; }, 8000);
  }

  nav.addEventListener('click', (e) => {
    const launcher = e.target.closest('[data-launch]');
    if (launcher) {
      e.preventDefault();
      launchEnv(launcher);
      return;
    }
    if (e.target.closest('a')) document.body.classList.remove('nav-open');
  });
})();
