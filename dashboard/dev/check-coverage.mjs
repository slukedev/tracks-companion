// CI check for the dashboard. Node only, no dependencies: node dashboard/dev/check-coverage.mjs
//
// 1. Every admin endpoint in api.md section 16 is called by some page (route strings like
//    'GET /admin/users/{id}' in the dashboard's modules).
// 2. Every GET route the pages use has a preview fixture.
// 3. No "http" anywhere in the shipped .html, .js and .css files, so no external URL can creep in.
// 4. Nothing that would break the CSP or insert HTML: inline scripts or styles, innerHTML and friends.
// 5. Source provider names appear only on the acquisition page.
// 6. Security rules: no browser storage or cookies from script, network only through api.js,
//    CSRF header and same-origin credentials on every call, no new windows or javascript: URLs,
//    every DELETE that destroys something sits beside a confirm().
// 7. Motion rules: no transition: all, no ease-in, no scale(0), hover only behind a pointer query.

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative, extname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('..', import.meta.url));
const apiMd = fileURLToPath(new URL('../../docs/api.md', import.meta.url));
const failures = [];
const fail = (msg) => failures.push(msg);

function walk(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : [path];
  });
}

const files = walk(root);
const shipped = files.filter((f) => !relative(root, f).startsWith('dev') && ['.html', '.js', '.css'].includes(extname(f)));
const modules = shipped.filter((f) => extname(f) === '.js');
const read = (f) => readFileSync(f, 'utf8');

const norm = (route) => route.replace(/\?.*$/, '').replace(/\{[^}]+\}/g, '{}').replace(/\/+$/, '');
const ROUTE = /['"`](GET|POST|PUT|PATCH|DELETE) (\/[^'"`\s]*)['"`]/g;

const used = new Map();
for (const f of modules) {
  for (const [, method, path] of read(f).matchAll(ROUTE)) {
    used.set(`${method} ${norm(path)}`, relative(root, f));
  }
}

// 1. Admin coverage.
const doc = read(apiMd);
const start = doc.indexOf('## 16. Admin endpoints');
const end = doc.indexOf('## 17.', start);
if (start < 0 || end < 0) fail('api.md: could not find section 16');
const spec = new Set();
for (const [, method, path] of doc.slice(start, end).matchAll(/`(GET|POST|PUT|PATCH|DELETE) (\/admin[^`\s]*)`/g)) {
  spec.add(`${method} ${norm(path)}`);
}
const missing = [...spec].filter((r) => !used.has(r));
for (const r of missing) fail(`no page calls ${r}`);

// 2. Preview fixtures for every GET.
const preview = read(join(root, 'dev/preview.js'));
for (const route of used.keys()) {
  if (!route.startsWith('GET ')) continue;
  const literal = [...preview.matchAll(ROUTE)].some(([, m, p]) => `${m} ${norm(p)}` === route);
  const hrefOnly = /\/backups\/\{\}$|\/listens\/export$/.test(route);
  if (!literal && !hrefOnly) fail(`no preview fixture for ${route}`);
}
for (const f of files.filter((x) => extname(x) === '.json')) {
  try { JSON.parse(read(f)); } catch (e) { fail(`${relative(root, f)} is not valid JSON: ${e.message}`); }
}

// 3. External URLs.
for (const f of [...shipped, ...files.filter((x) => relative(root, x).startsWith('dev') && ['.js', '.html', '.css'].includes(extname(x)))]) {
  read(f).split('\n').forEach((line, i) => {
    if (/http/i.test(line)) fail(`${relative(root, f)}:${i + 1} mentions http`);
  });
}

// 4. CSP and HTML insertion.
for (const f of shipped) {
  const text = read(f);
  const rel = relative(root, f);
  if (extname(f) === '.html') {
    if (/<script(?![^>]*\bsrc=)[^>]*>/i.test(text)) fail(`${rel}: inline <script>`);
    if (/<style[\s>]/i.test(text) || /\sstyle=/i.test(text)) fail(`${rel}: inline style`);
    if (/\son[a-z]+=/i.test(text)) fail(`${rel}: inline event handler`);
  }
  if (extname(f) === '.js') {
    for (const bad of ['innerHTML', 'outerHTML', 'insertAdjacentHTML', 'document.write', 'eval(', 'new Function', "setAttribute('style'", 'cssText']) {
      if (text.includes(bad)) fail(`${rel}: uses ${bad}`);
    }
  }
}

// 5. Provider names.
const PROVIDER = /yt-?dlp|soulseek|slskd/i;
for (const f of shipped) {
  const rel = relative(root, f);
  if (rel === join('pages', 'acquisition.js')) continue;
  read(f).split('\n').forEach((line, i) => {
    if (PROVIDER.test(line)) fail(`${rel}:${i + 1} names a source outside the acquisition page`);
  });
}

// 6. Security.
for (const f of shipped.filter((x) => extname(x) === '.js')) {
  const rel = relative(root, f);
  const text = read(f);
  for (const bad of ['localStorage', 'sessionStorage', 'indexedDB', 'document.cookie', 'window.open', 'javascript:', "target: '_blank'", 'postMessage(']) {
    if (text.includes(bad)) fail(`${rel}: uses ${bad}`);
  }
  if (rel !== 'api.js' && /\bfetch\(|XMLHttpRequest|sendBeacon|new WebSocket|new EventSource/.test(text)) fail(`${rel}: network access outside api.js`);
  const NOT_DESTRUCTIVE = ['DELETE /auth/session', 'DELETE /me/pairing-codes/{id}'];
  for (const [, path] of text.matchAll(/'DELETE (\/[^']*)'/g)) {
    if (!NOT_DESTRUCTIVE.includes(`DELETE ${path}`) && !text.includes('confirm(')) fail(`${rel}: DELETE ${path} without a confirm dialog`);
  }
}
{
  const api = read(join(root, 'api.js'));
  if (!api.includes("'X-Trackside-CSRF'")) fail('api.js: CSRF header missing');
  if (!api.includes("credentials: 'same-origin'")) fail('api.js: requests must use same-origin credentials');
  if (!api.includes('Retry-After')) fail('api.js: Retry-After is not read');
}

// 7. Motion.
for (const f of shipped.filter((x) => extname(x) === '.css')) {
  const rel = relative(root, f);
  const text = read(f);
  if (/transition:\s*all\b/.test(text)) fail(`${rel}: transition: all`);
  if (/\bease-in\b(?!-out)/.test(text)) fail(`${rel}: ease-in on UI`);
  if (/scale\(0\)/.test(text)) fail(`${rel}: scale(0)`);
  const gate = text.indexOf('@media (hover: hover) and (pointer: fine)');
  text.split('\n').forEach((line, i) => {
    if (line.includes(':hover') && (gate < 0 || text.indexOf(line) < gate)) fail(`${rel}:${i + 1} hover outside the pointer media query`);
  });
}

if (failures.length) {
  console.error(failures.map((f) => `FAIL ${f}`).join('\n'));
  console.error(`\n${failures.length} problem${failures.length === 1 ? '' : 's'}.`);
  process.exit(1);
}
console.log(`ok: ${spec.size} admin endpoints covered, ${used.size} routes in use, ${shipped.length} shipped files checked.`);
