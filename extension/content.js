// Content script: detect media links (video/audio/image sources) on the page
// the user is viewing and report them to the background service worker.
//
// Detection only — this never downloads anything and only inspects the DOM of
// pages the user has already opened, in line with Downpour's responsible-use
// boundary. The background worker caches the results so the popup can offer
// them for capture.

(function () {
  "use strict";

  const MAX_LINKS = 500;

  /**
   * Collect candidate media URLs from the current document.
   * Looks at <video>/<audio> (and their <source> children), <img>, and
   * anchors that point at obvious media files.
   * @returns {Array<{url: string, kind: string, label: string|null}>}
   */
  function collectMediaLinks() {
    const seen = new Set();
    const out = [];

    function push(url, kind, label) {
      if (!url || typeof url !== "string") return;
      const abs = absolutize(url);
      if (!abs || seen.has(abs)) return;
      if (!/^https?:/i.test(abs)) return; // only http(s); skip blob:/data:
      seen.add(abs);
      out.push({ url: abs, kind, label: label || null });
    }

    // <video>/<audio> elements and their nested <source>s.
    document.querySelectorAll("video, audio").forEach((el) => {
      const kind = el.tagName.toLowerCase();
      if (el.currentSrc) push(el.currentSrc, kind, el.getAttribute("title"));
      if (el.src) push(el.src, kind, el.getAttribute("title"));
      el.querySelectorAll("source").forEach((s) => {
        push(s.src || s.getAttribute("src"), kind, s.getAttribute("type"));
      });
    });

    // Images.
    document.querySelectorAll("img").forEach((img) => {
      push(img.currentSrc || img.src, "image", img.getAttribute("alt"));
    });

    // Anchors that link directly to media files.
    const MEDIA_HREF = /\.(mp4|mkv|webm|avi|mov|m4v|mp3|m4a|aac|flac|wav|ogg|opus|jpg|jpeg|png|gif|webp|svg|m3u8|mpd)(\?|#|$)/i;
    document.querySelectorAll("a[href]").forEach((a) => {
      const href = a.getAttribute("href");
      if (href && MEDIA_HREF.test(href)) {
        push(a.href, "link", (a.textContent || "").trim().slice(0, 80) || null);
      }
    });

    return out.slice(0, MAX_LINKS);
  }

  function absolutize(url) {
    try {
      return new URL(url, document.baseURI).href;
    } catch (_) {
      return null;
    }
  }

  function report() {
    let links;
    try {
      links = collectMediaLinks();
    } catch (_) {
      return;
    }
    try {
      chrome.runtime.sendMessage({ type: "downpour-media-links", links }, () => {
        // Swallow "receiving end does not exist" when worker is asleep.
        void chrome.runtime.lastError;
      });
    } catch (_) {
      // Extension context invalidated (e.g. reload) — ignore.
    }
  }

  // Initial scan once the DOM is ready, then a delayed rescan to catch media
  // injected after load. Kept lightweight to avoid impacting page performance.
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", report, { once: true });
  } else {
    report();
  }
  setTimeout(report, 2500);
})();
