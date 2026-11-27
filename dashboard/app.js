import { call, describe, onSignedOut, onReauthRequired, preview } from './api.js';
import { h, clear, icon, button, busy, field, input, formError, skeleton, errorState, nextId, openDialog } from './ui.js';

const root = document.getElementById('app');

// A TV's verification_url is <server>/link?code=XXXX-XXXX: open the Connect a TV page with the
// code, and keep the code out of the address bar and history.
if (/\/link$/.test(location.pathname)) {
  const code = new URLSearchParams(location.search).get('code') || '';
  history.replaceState(null, '', `./#/link/${encodeURIComponent(code)}`);
}

const pages = {
  overview: { title: 'Overview', icon: 'overview', load: () => import('./pages/overview.js'), admin: true },
  people: { title: 'People', icon: 'users', load: () => import('./pages/people.js'), admin: true },
  invites: { title: 'Invites', icon: 'invite', load: () => import('./pages/invites.js'), admin: true },
  libraries: { title: 'Libraries', icon: 'library', load: () => import('./pages/libraries.js'), admin: true },
  speakers: { title: 'Speakers', icon: 'speaker', load: () => import('./pages/speakers.js'), admin: true },
  requests: { title: 'Requests', icon: 'requests', load: () => import('./pages/acquisition.js'), admin: true, acquisition: true },
  backups: { title: 'Backups', icon: 'backups', load: () => import('./pages/backups.js'), admin: true },
  settings: { title: 'Settings', icon: 'settings', load: () => import('./pages/settings.js'), admin: true },
  activity: { title: 'Activity', icon: 'activity', load: () => import('./pages/audit.js'), admin: true },
  account: { title: 'My account', icon: 'account', load: () => import('./pages/account.js') },
  link: { title: 'Connect a TV', icon: 'tv', load: () => import('./pages/link.js'), nav: 'account' },
};

// Old links keep working: Users and Devices became People. #/music-assistant opens Speakers.
const ALIASES = { users: 'people', devices: 'people', 'music-assistant': 'speakers' };

const groups = [
  [null, ['overview']],
  ['People', ['people', 'invites']],
  ['Music', ['libraries', 'speakers', 'requests']],
  ['Server', ['backups', 'settings', 'activity']],
];

const state = { user: null, server: null, acquisition: false, shell: null, main: null, nav: new Map(), renderToken: 0, cleanup: null, prefetch: {} };

/** The browser chrome on phones matches whatever is at the top of the screen. */
const THEME = { rail: ['#1f2328', '#0b0d10'], bg: ['#eef0f2', '#101317'] };
function setThemeColor(which) {
  const [light, dark] = THEME[which];
  for (const meta of document.querySelectorAll('meta[name="theme-color"]')) {
    meta.setAttribute('content', (meta.getAttribute('media') || '').includes('dark') ? dark : light);
  }
}

function pageAllowed(key) {
  const page = pages[key];
  if (!page) return false;
  if (page.admin && state.user.role !== 'admin') return false;
  if (page.acquisition && !state.acquisition) return false;
  return true;
}

function parseHash() {
  const raw = location.hash.replace(/^#\/?/, '');
  const [key = '', ...rest] = raw.split('/');
  return { key: decodeURIComponent(key), args: rest.map(decodeURIComponent) };
}

export function go(path) {
  location.hash = `#/${path}`;
}

/** A frame shaped like the dashboard, shown before any answer arrives. */
function renderBootSkeleton() {
  if (root.querySelector('.shell')) return;
  clear(root);
  root.append(h('div.shell', { 'aria-busy': 'true' },
    h('header.topbar', h('div.brand', h('span.mark', icon('cassette')), h('span.brand-text', h('span.brand-name', 'Trackside')))),
    h('aside.sidebar', { 'aria-hidden': 'true' },
      h('div.brand', h('span.mark', icon('cassette')), h('span.brand-text', h('span.brand-name', 'Trackside'))),
      Array.from({ length: 7 }, () => h('span.skeleton.nav-line'))),
    h('main.main', skeleton(location.hash.includes('account') ? 'detail' : 'overview'))));
  setThemeColor('rail');
}

async function boot() {
  const hold = setTimeout(renderBootSkeleton, 120);
  // Everything the first screen needs is asked for at once.
  const [status, caps, session, overview] = await Promise.allSettled([
    call('GET /setup/status'),
    call('GET /capabilities'),
    call('GET /auth/session', { quietAuth: true }),
    call('GET /admin/overview', { quietAuth: true }),
  ]);
  clearTimeout(hold);
  if (status.status === 'rejected') { renderFatal(status.reason); return; }
  if (status.value.setup_required) {
    location.replace(`setup${preview ? '.html' + location.search : ''}`);
    return;
  }
  state.server = { name: caps.value?.name || 'Trackside', version: caps.value?.version };
  document.title = state.server.name;
  if (session.status === 'rejected') {
    if (session.reason.code === 'unauthenticated') { renderSignIn(); return; }
    renderFatal(session.reason);
    return;
  }
  state.user = session.value;
  await startShell(overview.status === 'fulfilled' ? overview.value : undefined);
}

async function startShell(overview) {
  if (state.user.role === 'admin') {
    if (overview === undefined) overview = await call('GET /admin/overview').catch(() => null);
    state.acquisition = Boolean(overview?.acquisition);
    if (overview?.server?.name) state.server.name = overview.server.name;
    if (overview) state.prefetch.overview = overview;
  }
  renderShell();
  window.addEventListener('hashchange', route);
  route();
}

onSignedOut(() => {
  if (state.user) {
    state.user = null;
    window.removeEventListener('hashchange', route);
    renderSignIn('Your session ended. Sign in again.');
  }
});

/* ---------- Confirm it is you ---------- */

onReauthRequired(() => new Promise((resolve) => {
  const password = input({ type: 'password', autocomplete: 'current-password', required: true });
  const error = formError();
  const titleId = nextId('d');
  const submit = button('Confirm', { kind: 'primary', type: 'submit' });
  let ok = false;
  const dialog = h('dialog', { 'aria-labelledby': titleId },
    h('form', {
      on: {
        submit: async (event) => {
          event.preventDefault();
          error.hide();
          await busy(submit, async () => {
            try {
              await call('POST /auth/reauth', { body: { password: password.value }, quietAuth: true });
              ok = true;
              dialog.close();
            } catch (err) {
              password.value = '';
              error.show(err.code === 'unauthenticated' ? 'That password is wrong. Try again.' : describe(err));
              password.focus();
            }
          });
        },
      },
    },
    h('div.dialog-body',
      h('h2', { id: titleId }, 'Confirm your password'),
      h('p', 'This change needs your password again.'),
      field('Password', password),
      error.el),
    h('div.dialog-actions', button('Cancel', { onClick: () => dialog.close() }), submit)));
  openDialog(dialog, () => resolve(ok));
  password.focus();
}));

/* ---------- Sign in ---------- */

function renderSignIn(notice) {
  clear(root);
  setThemeColor('bg');
  const serverName = state.server?.name || 'Trackside';
  document.title = `Sign in · ${serverName}`;
  const username = input({ autocomplete: 'username', autocapitalize: 'none', spellcheck: 'false', required: true });
  const password = input({ type: 'password', autocomplete: 'current-password', required: true });
  const error = formError();
  const submit = button('Sign in', { kind: 'primary', type: 'submit', size: 'large' });
  submit.classList.add('block');
  if (notice) error.show(notice);
  const form = h('form.panel.form', {
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        await busy(submit, async () => {
          try {
            state.user = await call('POST /auth/session', { body: { username: username.value.trim(), password: password.value }, quietAuth: true });
            await startShell();
          } catch (err) {
            password.value = '';
            error.show(err.code === 'unauthenticated' ? 'That username and password do not match.' : describe(err));
            password.focus();
            if (err.code === 'rate_limited' && err.retryAfter) holdUntilRetry(submit, err.retryAfter);
          }
        });
      },
    },
  },
  field('Username', username),
  field('Password', password),
  error.el,
  submit);
  root.append(h('main.solo', { id: 'main' },
    h('div.solo-card',
      h('div.solo-head', h('span.mark.lg', icon('cassette')), h('p.product', 'Trackside'), h('h1', serverName)),
      form
    )));
  username.focus();
}

/** After a 429 the button waits out Retry-After, counting down, so nobody hammers the limiter. */
function holdUntilRetry(btn, seconds) {
  const label = btn.lastChild;
  const original = label.textContent;
  const until = performance.now() + seconds * 1000;
  btn.disabled = true;
  btn.dataset.hold = '1';
  const tick = () => {
    const left = Math.ceil((until - performance.now()) / 1000);
    if (left <= 0) { delete btn.dataset.hold; btn.disabled = false; label.textContent = original; return; }
    label.textContent = `Try again in ${left} s`;
    setTimeout(tick, 1000 - ((until - performance.now()) % 1000));
  };
  tick();
}

function renderFatal(error) {
  clear(root);
  setThemeColor('bg');
  root.append(h('main.solo', { id: 'main' }, h('div.solo-card', errorState(error, () => location.reload(), 'Trackside did not load'))));
}

/* ---------- Shell ---------- */

function brand() {
  return h('div.brand',
    h('span.mark', icon('cassette')),
    h('span.brand-text', h('span.brand-name', 'Trackside'), h('span.brand-server', state.server.name)));
}

function renderShell() {
  clear(root);
  setThemeColor('rail');
  state.nav.clear();
  const isAdmin = state.user.role === 'admin';
  const who = state.user.display_name || state.user.username;

  const navGroups = isAdmin
    ? groups.map(([heading, keys]) => [heading, keys.filter(pageAllowed)]).filter(([, keys]) => keys.length)
    : [];
  const remember = (key, el) => {
    if (!state.nav.has(key)) state.nav.set(key, []);
    state.nav.get(key).push(el);
    return el;
  };
  const link = (key) => remember(key, h('a', { href: `#/${key}` }, icon(pages[key].icon), h('span', pages[key].title)));
  // The account entry is the person's name; its label still says what it opens.
  const accountLink = () => remember('account', h('a', { href: '#/account', 'aria-label': `My account, ${who}` }, icon('account'), h('span', who)));
  const nav = h('nav.nav', { 'aria-label': 'Sections' },
    navGroups.map(([heading, keys]) => {
      const headingId = heading ? `nav-${heading.toLowerCase()}` : undefined;
      return h('div.nav-group', { role: heading ? 'group' : undefined, 'aria-labelledby': headingId },
        heading ? h('div.nav-heading', { id: headingId }, heading) : null,
        keys.map(link));
    }));
  const signOutAction = async () => {
    try { await call('DELETE /auth/session'); } catch { /* signing out locally either way */ }
    location.reload();
  };
  const signOut = button('Sign out', { kind: 'plain', iconName: 'signout', onClick: signOutAction });

  const closeSheet = button('', { iconName: 'close', ariaLabel: 'Close' });
  const sidebar = h('aside.sidebar', { id: 'sidebar', 'aria-label': 'All sections' },
    brand(),
    h('div.sheet-head', brand(), closeSheet),
    nav,
    h('div.nav-foot.nav', accountLink(), signOut));

  // Phones: the pages used most sit in a tab bar; the rest open from More.
  let more = null;
  let tabbar = null;
  if (isAdmin) {
    const primary = ['overview', 'people', 'libraries', pageAllowed('requests') ? 'requests' : 'backups'];
    more = h('button', { type: 'button', 'aria-expanded': 'false', 'aria-controls': 'sidebar' }, icon('more'), h('span', 'More'));
    tabbar = h('nav.tabbar', { 'aria-label': 'Main' },
      primary.map((key) => remember(key, h('a', { href: `#/${key}` }, icon(pages[key].icon), h('span', pages[key].title)))),
      more);
  }

  const shell = h(`div.shell${isAdmin ? '' : '.solo-nav'}`,
    h('a.skip-link', { href: '#main', on: { click: (e) => { e.preventDefault(); state.main.focus(); } } }, 'Skip to content'),
    h('header.topbar', brand(), isAdmin ? null : button('', { iconName: 'signout', ariaLabel: 'Sign out', onClick: signOutAction })),
    sidebar,
    h('div.scrim', { on: { click: () => setMenu(false) } }),
    h('main.main', { id: 'main', tabindex: '-1' }),
    tabbar);
  state.shell = shell;
  state.main = shell.querySelector('main');
  const phone = window.matchMedia('(max-width: 860px)');
  const setMenu = (open, { restore = true } = {}) => {
    if (!more) return;
    const wasOpen = shell.classList.contains('nav-open');
    shell.classList.toggle('nav-open', open);
    more.setAttribute('aria-expanded', String(open));
    state.main.inert = open && phone.matches;
    if (open) nav.querySelector('a')?.focus({ preventScroll: true });
    else if (wasOpen && restore) more.focus({ preventScroll: true });
  };
  more?.addEventListener('click', () => setMenu(!shell.classList.contains('nav-open')));
  closeSheet.addEventListener('click', () => setMenu(false));
  sidebar.addEventListener('click', (e) => { if (e.target.closest('a')) setMenu(false, { restore: false }); });
  phone.addEventListener('change', () => setMenu(false, { restore: false }));
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && shell.classList.contains('nav-open')) setMenu(false);
  });
  root.append(shell);
}

function skeletonFor(key, args) {
  if (key === 'overview') return skeleton('overview');
  if (args.length || ['account', 'settings', 'backups', 'activity', 'speakers'].includes(key)) return skeleton('detail');
  return skeleton('list');
}

async function route() {
  let { key, args } = parseHash();
  if (ALIASES[key]) {
    key = ALIASES[key];
    history.replaceState(null, '', `#/${[key, ...args].map(encodeURIComponent).join('/')}`);
  }
  const fallback = state.user.role === 'admin' ? 'overview' : 'account';
  if (!pageAllowed(key)) {
    history.replaceState(null, '', `#/${fallback}`);
    key = fallback;
    args = [];
  }
  const navKey = pages[key].nav || key;
  for (const [k, els] of state.nav) {
    for (const a of els) if (k === navKey) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  }
  const token = ++state.renderToken;
  if (state.cleanup) { state.cleanup(); state.cleanup = null; }
  const main = state.main;
  const previous = main.dataset.route;
  const routeId = [key, ...args].join('/');
  const newPage = main.dataset.page !== key;
  main.dataset.page = key;
  main.dataset.route = routeId;
  if (previous !== routeId) {
    clear(main).append(skeletonFor(key, args));
    window.scrollTo(0, 0);
  }
  const page = pages[key];
  document.title = `${page.title} · ${state.server.name}`;
  const ctx = {
    user: state.user,
    server: state.server,
    acquisition: state.acquisition,
    args,
    go,
    /** Data fetched while booting, handed to the first page that wants it, once. */
    take: (name) => { const v = state.prefetch[name]; delete state.prefetch[name]; return v; },
    isCurrent: () => token === state.renderToken,
    onLeave: (fn) => { const prev = state.cleanup; state.cleanup = prev ? () => { prev(); fn(); } : fn; },
    rerender: () => route(),
  };
  try {
    const module = await page.load();
    const view = await module.render(ctx);
    if (token !== state.renderToken) return;
    if (previous !== routeId) view.classList.add('enter');
    clear(main).append(view);
    if (previous && previous !== routeId) main.querySelector('[data-page-title]')?.focus({ preventScroll: true });
    else if (newPage && previous) main.focus({ preventScroll: true });
  } catch (error) {
    if (token !== state.renderToken) return;
    if (error.code === 'unauthenticated') return;
    clear(main).append(h('div.page', errorState(error, () => route())));
  }
}

boot();
