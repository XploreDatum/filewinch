/* Filewinch downloads: resolve the latest GitHub Release and point each button at its file.
   Every button already links to the releases page, so with JS off, a failed request or no
   release yet, the page still works. */
(function () {
  "use strict";
  var cfg = window.FILEWINCH || {};
  var repo = cfg.REPO || "XploreDatum/filewinch";
  var releasesUrl = "https://github.com/" + repo + "/releases";
  var api = "https://api.github.com/repos/" + repo + "/releases/latest";
  var cacheKey = "filewinch-release-v1";
  var cacheMs = 10 * 60 * 1000;   // the API allows 60 unauthenticated calls an hour per IP

  var status = document.getElementById("dl-status");
  var os = window.FILEWINCH_OS || "";

  // Highlight the visitor's OS and move it first on narrow screens.
  if (os) {
    var card = document.querySelector('.platform[data-os="' + os + '"]');
    if (card) {
      card.classList.add("is-yours");
      if (card.parentNode.firstElementChild !== card) card.parentNode.insertBefore(card, card.parentNode.firstElementChild);
    }
  }

  // Formats per OS; `test` matches release asset names (lower-cased).
  var formats = {
    macos: [{ id: "dmg", label: ".dmg", note: "Universal: Apple silicon and Intel", test: /\.dmg$/ }],
    windows: [
      { id: "msi", label: ".msi installer", note: "Installs for your account", test: /\.msi$/ },
      { id: "zip", label: ".zip", note: "Portable folder, no installer", test: /windows.*\.zip$/ }
    ],
    linux: [
      { id: "appimage", label: ".AppImage", note: "Runs without installing", test: /\.appimage$/ },
      { id: "tar", label: ".tar.gz", note: "Includes install.sh for ~/.local", test: /linux.*\.tar\.gz$/ }
    ]
  };

  function arch(name) {
    if (/aarch64|arm64/.test(name)) return "ARM64";
    if (/x86_64|amd64|x64/.test(name)) return "x64";
    if (/universal/.test(name)) return "Universal";
    return "";
  }
  function size(bytes) {
    if (!bytes) return "";
    var mb = bytes / 1048576;
    return mb >= 10 ? Math.round(mb) + " MB" : mb.toFixed(1) + " MB";
  }
  function date(iso) {
    try {
      return new Date(iso).toLocaleDateString(undefined, { day: "numeric", month: "long", year: "numeric" });
    } catch (e) { return ""; }
  }
  function el(tag, cls, text) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text) n.textContent = text;
    return n;
  }
  var arrow = '<svg aria-hidden="true" viewBox="0 0 20 20" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M10 3v10M5.5 8.5 10 13l4.5-4.5M4 16.5h12"/></svg>';

  function button(href, title, sub, primary, noIcon) {
    var a = el("a", "btn " + (primary ? "btn-primary" : "btn-secondary"));
    a.href = href;
    var t = el("span");
    t.appendChild(document.createTextNode(title));
    if (sub) { t.appendChild(document.createElement("br")); t.appendChild(el("span", "sub", sub)); }
    a.appendChild(t);
    if (!noIcon) a.insertAdjacentHTML("beforeend", arrow);
    return a;
  }

  function setStatus(state, text, retry) {
    status.setAttribute("data-state", state);
    status.textContent = text;
    if (retry) {
      var b = el("button", "btn btn-secondary btn-small", "Try again");
      b.type = "button";
      b.addEventListener("click", function () { load(true); });
      status.appendChild(b);
    }
  }

  function render(rel) {
    var assets = (rel.assets || []).map(function (a) {
      return { name: a.name, lname: a.name.toLowerCase(), url: a.browser_download_url, size: a.size };
    });
    var page = rel.html_url || releasesUrl + "/latest";
    var version = (rel.tag_name || rel.name || "").replace(/^v/i, "");
    var used = {};

    Object.keys(formats).forEach(function (key) {
      var box = document.querySelector('.platform[data-os="' + key + '"] .buttons');
      var meta = document.querySelector('.platform[data-os="' + key + '"] .meta');
      if (!box) return;
      box.textContent = "";
      var found = 0;
      formats[key].forEach(function (f, fi) {
        var matches = assets.filter(function (a) {
          return !used[a.name] && f.test.test(a.lname) && !/\.(sha256|sig|asc|minisig)$/.test(a.lname);
        });
        matches.sort(function (a, b) { return arch(a.lname) === "x64" ? -1 : arch(b.lname) === "x64" ? 1 : 0; });
        matches.forEach(function (a) {
          used[a.name] = true;
          var ar = arch(a.lname);
          var sub = [ar || (key === "macos" ? "Universal" : ""), size(a.size)].filter(Boolean).join(" · ");
          box.appendChild(button(a.url, "Download " + f.label, sub, fi === 0 && found === 0));
          found++;
        });
      });
      if (!found) {
        box.appendChild(button(page, "See the release page", "No " + (key === "macos" ? "Mac" : key === "windows" ? "Windows" : "Linux") + " build in " + (version ? "version " + version : "the latest release") + " yet", false, true));
      }
      if (meta) meta.textContent = version ? "Version " + version + (rel.published_at ? ", " + date(rel.published_at) : "") : "";
    });

    var sums = assets.filter(function (a) { return /sha256|checksum/.test(a.lname); })[0];
    var sumLink = document.getElementById("dl-checksums");
    if (sumLink) {
      sumLink.href = sums ? sums.url : page;
      sumLink.textContent = sums ? sums.name : "Checksums on the release page";
    }
    var notes = document.getElementById("dl-notes");
    if (notes) notes.href = page;

    setStatus("ok", "Filewinch " + (version || "") + (rel.published_at ? ", released " + date(rel.published_at) : "") + ".");
  }

  function comingSoon() {
    document.querySelectorAll(".platform .buttons").forEach(function (box) {
      box.textContent = "";
      box.appendChild(button(releasesUrl, "Coming soon", "Watch the releases page on GitHub", false, true));
    });
    setStatus("none", "The first release is on its way. Builds will appear here the moment it's published.");
  }

  function readCache() {
    try {
      var c = JSON.parse(sessionStorage.getItem(cacheKey) || "null");
      if (c && Date.now() - c.at < cacheMs) return c;
    } catch (e) { /* storage blocked: fetch instead */ }
    return null;
  }
  function writeCache(kind, rel) {
    try { sessionStorage.setItem(cacheKey, JSON.stringify({ at: Date.now(), kind: kind, rel: rel })); } catch (e) { /* ignore */ }
  }

  function load(force) {
    var c = !force && readCache();
    if (c) { c.kind === "none" ? comingSoon() : render(c.rel); return; }
    setStatus("loading", "Checking GitHub for the latest release…");
    var ctl = "AbortController" in window ? new AbortController() : null;
    var timer = setTimeout(function () { if (ctl) ctl.abort(); }, 8000);
    fetch(api, { headers: { Accept: "application/vnd.github+json" }, signal: ctl ? ctl.signal : undefined })
      .then(function (r) {
        clearTimeout(timer);
        if (r.status === 404) { writeCache("none", null); comingSoon(); return null; }
        if (!r.ok) throw new Error(r.status === 403 || r.status === 429 ? "rate" : "http " + r.status);
        return r.json();
      })
      .then(function (rel) {
        if (!rel) return;
        if (!rel.assets || !rel.assets.length) { comingSoon(); return; }
        writeCache("ok", rel);
        render(rel);
      })
      .catch(function (err) {
        clearTimeout(timer);
        var why = err && err.message === "rate"
          ? "GitHub is limiting requests from your network right now."
          : "Couldn't reach GitHub just now.";
        setStatus("error", why + " The buttons below open the releases page, where every build is listed.", true);
      });
  }

  if (status && window.fetch) load(false);
})();
