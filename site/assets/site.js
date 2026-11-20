/* Filewinch site: OS detection, accessible tabs, checkout link. No tracking, no storage. */
(function () {
  "use strict";
  var doc = document.documentElement;
  var cfg = window.FILEWINCH || {};

  // Which desktop OS is the visitor on? "macos", "windows", "linux" or "" (phones, unknown).
  function detectOS() {
    var uad = navigator.userAgentData;
    var p = ((uad && uad.platform) || navigator.platform || "").toLowerCase();
    var ua = (navigator.userAgent || "").toLowerCase();
    if (/android|iphone|ipad|ipod/.test(ua) || (uad && uad.mobile)) return "";
    if (p.indexOf("mac") === 0 || p === "macos") {
      // iPadOS reports "MacIntel" with touch; it can't run Filewinch.
      return navigator.maxTouchPoints > 1 ? "" : "macos";
    }
    if (p.indexOf("win") === 0) return "windows";
    if (p.indexOf("linux") === 0 || /x11|linux/.test(ua)) return /cros/.test(ua) ? "" : "linux";
    return "";
  }
  var os = detectOS();
  var names = { macos: "macOS", windows: "Windows", linux: "Linux" };
  if (os) doc.setAttribute("data-os", os);
  window.FILEWINCH_OS = os;

  // "Download for macOS" style labels.
  document.querySelectorAll("[data-download-label]").forEach(function (el) {
    if (os) el.textContent = "Download for " + names[os];
  });

  // Tabs (WAI-ARIA tabs pattern, automatic activation, roving tabindex).
  document.querySelectorAll("[role=tablist]").forEach(function (list) {
    var tabs = Array.prototype.slice.call(list.querySelectorAll("[role=tab]"));
    function select(tab, focus) {
      tabs.forEach(function (t) {
        var on = t === tab;
        t.setAttribute("aria-selected", on ? "true" : "false");
        t.tabIndex = on ? 0 : -1;
        var panel = document.getElementById(t.getAttribute("aria-controls"));
        if (panel) panel.hidden = !on;
      });
      if (focus) tab.focus();
    }
    tabs.forEach(function (tab, i) {
      tab.addEventListener("click", function () { select(tab, false); });
      tab.addEventListener("keydown", function (e) {
        var next = null;
        if (e.key === "ArrowRight" || e.key === "ArrowDown") next = tabs[(i + 1) % tabs.length];
        else if (e.key === "ArrowLeft" || e.key === "ArrowUp") next = tabs[(i - 1 + tabs.length) % tabs.length];
        else if (e.key === "Home") next = tabs[0];
        else if (e.key === "End") next = tabs[tabs.length - 1];
        if (next) { e.preventDefault(); select(next, true); }
      });
    });
    // OS switchers open on the visitor's own OS.
    if (list.hasAttribute("data-follow-os") && os) {
      var mine = list.querySelector('[data-os="' + os + '"]');
      if (mine) select(mine, false);
    }
  });

  // Checkout buttons: the link lives only in config.js.
  document.querySelectorAll("[data-checkout]").forEach(function (a) {
    var url = cfg.CHECKOUT_URL;
    if (url) {
      a.href = url;
      return;
    }
    a.removeAttribute("href");
    a.setAttribute("aria-disabled", "true");
    a.setAttribute("role", "link");
    var label = a.querySelector("[data-checkout-label]") || a;
    label.textContent = "Licences go on sale soon";
  });
})();
