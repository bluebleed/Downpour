// Pure, dependency-free capture-filter logic for the Downpour extension.
//
// This file intentionally has NO dependency on any `chrome.*` API or the DOM,
// so it can be:
//   * loaded into the service worker via `importScripts("filter.js")`
//   * loaded into the popup via a <script src="filter.js"> tag
//   * `require()`d directly from a Node-based property test (task 12.3)
//
// It implements the size + extension whitelist/blacklist rules from
// Requirements 6.5 and 6.6 as a single pure predicate `shouldCapture`.

(function (root) {
  "use strict";

  // Defaults: capture anything >= 50 KB, no extension restrictions.
  // `minSizeBytes` is user-configurable from 0 to 100 MB (Req 6.5).
  const DEFAULT_FILTER_CONFIG = Object.freeze({
    minSizeBytes: 51200, // 50 KB
    whitelist: [], // if non-empty: capture ONLY these extensions
    blacklist: [], // if non-empty (and no whitelist): skip these extensions
  });

  // Hard limits from Requirement 6.6.
  const MAX_LIST_ENTRIES = 200;
  const MAX_MIN_SIZE_BYTES = 100 * 1024 * 1024; // 100 MB

  /**
   * Normalise a single extension token to a bare, lowercase, dot-less form.
   * Accepts ".MP4", "mp4", " Mp4 " → "mp4". Returns "" for junk.
   * @param {unknown} ext
   * @returns {string}
   */
  function normalizeExt(ext) {
    if (typeof ext !== "string") return "";
    let e = ext.trim().toLowerCase();
    while (e.startsWith(".")) e = e.slice(1);
    return e;
  }

  /**
   * Normalise and de-duplicate an extension list, dropping empties and
   * capping at MAX_LIST_ENTRIES (Req 6.6).
   * @param {unknown} list
   * @returns {string[]}
   */
  function normalizeList(list) {
    if (!Array.isArray(list)) return [];
    const out = [];
    const seen = new Set();
    for (const raw of list) {
      const e = normalizeExt(raw);
      if (!e || seen.has(e)) continue;
      seen.add(e);
      out.push(e);
      if (out.length >= MAX_LIST_ENTRIES) break;
    }
    return out;
  }

  /**
   * Extract the file extension (bare, lowercase, dot-less) from a filename,
   * falling back to the URL path. Returns "" when there is no extension.
   * Query strings and fragments on URLs are ignored.
   * @param {string|null|undefined} filename
   * @param {string|null|undefined} url
   * @returns {string}
   */
  function getExtension(filename, url) {
    const fromName = extOf(filename);
    if (fromName) return fromName;
    return extOf(stripUrlTail(url));
  }

  function stripUrlTail(url) {
    if (typeof url !== "string") return "";
    // Drop query/fragment, then keep the last path segment.
    let s = url.split("#")[0].split("?")[0];
    const slash = s.lastIndexOf("/");
    if (slash !== -1) s = s.slice(slash + 1);
    return s;
  }

  function extOf(name) {
    if (typeof name !== "string") return "";
    const base = name.trim();
    const dot = base.lastIndexOf(".");
    // No dot, or leading dot (dotfile with no real extension), or trailing dot.
    if (dot <= 0 || dot === base.length - 1) return "";
    return normalizeExt(base.slice(dot + 1));
  }

  /**
   * Decide whether an extension passes the active list filter.
   * Whitelist takes precedence over blacklist when both are present.
   *   - whitelist non-empty  → accept iff ext is in the whitelist
   *   - blacklist non-empty  → accept iff ext is NOT in the blacklist
   *   - neither              → accept
   * An empty extension ("") can never satisfy a whitelist, but is allowed
   * through when only a blacklist is active.
   * @param {string} ext  bare lowercase extension
   * @param {{whitelist?: string[], blacklist?: string[]}} config
   * @returns {boolean}
   */
  function extensionAllowed(ext, config) {
    const whitelist = normalizeList(config && config.whitelist);
    const blacklist = normalizeList(config && config.blacklist);
    if (whitelist.length > 0) {
      return ext !== "" && whitelist.includes(ext);
    }
    if (blacklist.length > 0) {
      return !blacklist.includes(ext);
    }
    return true;
  }

  /**
   * The core predicate (Property 12 / Requirements 6.5, 6.6).
   *
   * Accept the download for capture IF AND ONLY IF:
   *   1. its size meets the minimum threshold, AND
   *   2. its extension passes the active whitelist/blacklist filter.
   *
   * Size handling: when the size is unknown (null/undefined/NaN) we cannot
   * prove it is below the threshold, so the size check passes and the decision
   * is left to the extension filter.
   *
   * @param {{filesize?: number|null, filename?: string|null, url?: string|null}} payload
   * @param {{minSizeBytes?: number, whitelist?: string[], blacklist?: string[]}} [config]
   * @returns {boolean}
   */
  function shouldCapture(payload, config) {
    const cfg = config || DEFAULT_FILTER_CONFIG;
    const min = sanitizeMinSize(cfg.minSizeBytes);

    const size = payload ? payload.filesize : null;
    const sizeKnown = typeof size === "number" && isFinite(size);
    if (sizeKnown && size < min) return false;

    const ext = getExtension(
      payload ? payload.filename : null,
      payload ? payload.url : null
    );
    return extensionAllowed(ext, cfg);
  }

  /**
   * Clamp/sanitise a configured minimum size to the valid 0..100 MB range.
   * @param {unknown} value
   * @returns {number}
   */
  function sanitizeMinSize(value) {
    if (typeof value !== "number" || !isFinite(value) || value < 0) {
      return DEFAULT_FILTER_CONFIG.minSizeBytes;
    }
    return Math.min(Math.floor(value), MAX_MIN_SIZE_BYTES);
  }

  /**
   * Sanitise a raw (possibly user-supplied) filter config into a safe,
   * fully-populated config object with capped lists and a clamped size.
   * @param {object} raw
   * @returns {{minSizeBytes: number, whitelist: string[], blacklist: string[]}}
   */
  function sanitizeConfig(raw) {
    const r = raw || {};
    return {
      minSizeBytes:
        r.minSizeBytes === undefined
          ? DEFAULT_FILTER_CONFIG.minSizeBytes
          : sanitizeMinSize(r.minSizeBytes),
      whitelist: normalizeList(r.whitelist),
      blacklist: normalizeList(r.blacklist),
    };
  }

  const api = {
    DEFAULT_FILTER_CONFIG,
    MAX_LIST_ENTRIES,
    MAX_MIN_SIZE_BYTES,
    normalizeExt,
    normalizeList,
    getExtension,
    extensionAllowed,
    shouldCapture,
    sanitizeMinSize,
    sanitizeConfig,
  };

  // Dual export: globalThis (worker/popup via importScripts/<script>) and
  // CommonJS (Node property tests).
  if (typeof module !== "undefined" && module.exports) {
    module.exports = api;
  }
  root.DownpourFilter = api;
})(typeof globalThis !== "undefined" ? globalThis : self);
