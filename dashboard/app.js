import { call, describe, onSignedOut, onReauthRequired, preview } from './api.js';
import { h, clear, icon, button, busy, field, input, formError, skeleton, errorState, nextId } from './ui.js';

const root = document.getElementById('app');

const pages = {
  overview: { title: 'Overview', icon: 'overview', load: () => import('./pages/overview.js'), admin: true },
  users: { title: 'Users', icon: 'users', load: () => import('./pages/users.js'), admin: true },
  invites: { title: 'Invites', icon: 'invite', load: () => import('./pages/invites.js'), admin: true },
  devices: { title: 'Devices and Keys', icon: 'devices', load: () => import('./pages/devices.js'), admin: true },
  libraries: { title: 'Libraries', icon: 'library', load: () => import('./pages/libraries.js'), admin: true },
  requests: { title: 'Requests', icon: 'requests', load: () => import('./pages/acquisition.js'), admin: true, acquisition: true },
  backups: { title: 'Backups', icon: 'backups', load: () => import('./pages/backups.js'), admin: true },
  settings: { title: 'Settings', icon: 'settings', load: () => import('./pages/settings.js'), admin: true },
  activity: { title: 'Activity', icon: 'activity', load: () => import('./pages/audit.js'), admin: true },
  account: { title: 'My Account', icon: 'account', load: () => import('./pages/account.js') },
};

const groups = [
  [null, ['overview']],
  ['People', ['users', 'invites', 'devices']],
  ['Music', ['libraries', 'requests']],
  ['Server', ['backups', 'settings', 'activity']],
];

const state = { user: null, server: null, acquisition: false, shell: null, main: null, nav: new Map(), renderToken: 0, cleanup: null };

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

async function boot() {
  try {
    const status = await call('GET /setup/status');
    if (status.setup_required) {
      location.replace(`setup${preview ? '.html' + location.search : ''}`);
      return;
    }
  } catch (error) {
    renderFatal(error);
    return;
  }
  const caps = await call('GET /capabilities').catch(() => null);
  state.server = { name: caps?.name || 'Trackside', version: caps?.version };
  document.title = state.server.name;
  try {
    state.user = await call('GET /auth/session', { quietAuth: true });
  } catch (error) {
    if (error.code === 'unauthenticated') { renderSignIn(); return; }
    renderFatal(error);
    return;
  }
  await startShell();
}

async function startShell() {
  if (state.user.role === 'admin') {
    try {
      const overview = await call('GET /admin/overview');
      state.acquisition = Boolean(overview.acquisition);
      state.server.name = overview.server?.name || state.server.name;
    } catch {
      state.acquisition = false;
    }
  }
  renderShell();
  window.addEventListener('hashchange', route);
  route();
}

onSignedOut(() => {
  if (state.user) {
    state.user = null;
    window.removeEventListener('hashchange', route);
    renderSignIn('Session ended. Sign in again.');
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
              error.show(err.code === 'unauthenticated' ? 'Wrong password.' : describe(err));
              password.focus();
            }
          });
        },
      },
    },
    h('div.dialog-body',
      h('h2', { id: titleId }, 'Confirm Password'),
      field('Password', password),
      error.el),
    h('div.dialog-actions', button('Cancel', { onClick: () => dialog.close() }), submit)));
  document.body.append(dialog);
  dialog.addEventListener('close', () => { dialog.remove(); resolve(ok); });
  dialog.showModal();
  password.focus();
}));

/* ---------- Sign in ---------- */

function renderSignIn(notice) {
  clear(root);
  document.title = `Sign In · ${state.server?.name || 'Trackside'}`;
  const username = input({ autocomplete: 'username', autocapitalize: 'none', spellcheck: 'false', required: true });
  const password = input({ type: 'password', autocomplete: 'current-password', required: true });
  const error = formError();
  const submit = button('Sign In', { kind: 'primary', type: 'submit', size: 'large' });
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
            error.show(err.code === 'unauthenticated' ? 'Wrong username or password.' : describe(err));
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
      h('div.solo-head', h('span.mark.lg', icon('cassette')), h('h1', state.server?.name || 'Trackside')),
      form
    )));
  username.focus();
}

/** After a 429 the button waits out Retry-After, counting down, so nobody hammers the limiter. */
function holdUntilRetry(btn, seconds) {
  const label = btn.lastChild;
  const original = label.textContent;
  let left = seconds;
  btn.disabled = true;
  btn.dataset.hold = '1';
  const tick = () => {
    if (left <= 0) { clearInterval(timer); delete btn.dataset.hold; btn.disabled = false; label.textContent = original; return; }
    label.textContent = `Try again in ${left} s`;
    left -= 1;
  };
  const timer = setInterval(tick, 1000);
  tick();
}

function renderFatal(error) {
  clear(root);
  root.append(h('main.solo', { id: 'main' }, h('div.solo-card', errorState(error, () => location.reload()))));
}

/* ---------- Shell ---------- */

function renderShell() {
  clear(root);
  state.nav.clear();
  const isAdmin = state.user.role === 'admin';
  const menuButton = button('', { iconName: 'menu', ariaLabel: 'Menu' });
  menuButton.setAttribute('aria-expanded', 'false');
  menuButton.setAttribute('aria-controls', 'sidebar');

  const navGroups = isAdmin
    ? groups.map(([heading, keys]) => [heading, keys.filter(pageAllowed)]).filter(([, keys]) => keys.length)
    : [];
  const link = (key) => {
    const a = h('a', { href: `#/${key}` }, icon(pages[key].icon), h('span', pages[key].title));
    state.nav.set(key, a);
    return a;
  };
  // The account entry is the person's name; its label still says what it opens.
  const accountLink = () => {
    const a = h('a', { href: '#/account', 'aria-label': `My Account, ${state.user.display_name || state.user.username}` }, icon('account'), h('span', state.user.display_name || state.user.username));
    state.nav.set('account', a);
    return a;
  };
  const nav = h('nav.nav', { 'aria-label': 'Sections' },
    navGroups.map(([heading, keys]) => {
      const headingId = heading ? `nav-${heading.toLowerCase()}` : undefined;
      return h('div.nav-group', { role: heading ? 'group' : undefined, 'aria-labelledby': headingId },
        heading ? h('div.nav-heading', { id: headingId }, heading) : null,
        keys.map(link));
    }));
  const signOut = button('Sign Out', { kind: 'plain', iconName: 'signout', onClick: async () => {
    try { await call('DELETE /auth/session'); } catch { /* signing out locally either way */ }
    location.reload();
  } });
  const sidebar = h('aside.sidebar', { id: 'sidebar' },
    h('div.brand', h('span.mark', icon('cassette')), h('span.brand-name', state.server.name)),
    nav,
    h('div.nav-foot.nav',
      accountLink(),
      signOut));

  const shell = h('div.shell',
    h('a.skip-link', { href: '#main', on: { click: (e) => { e.preventDefault(); state.main.focus(); } } }, 'Skip to content'),
    h('header.topbar',
      h('div.brand', h('span.mark', icon('cassette')), h('span.brand-name', state.server.name)),
      menuButton),
    sidebar,
    h('div.scrim', { on: { click: () => setMenu(false) } }),
    h('main.main', { id: 'main', tabindex: '-1' }));
  state.shell = shell;
  state.main = shell.querySelector('main');
  const setMenu = (open) => {
    shell.classList.toggle('nav-open', open);
    menuButton.setAttribute('aria-expanded', String(open));
    menuButton.setAttribute('aria-label', open ? 'Close menu' : 'Menu');
  };
  menuButton.addEventListener('click', () => setMenu(!shell.classList.contains('nav-open')));
  sidebar.addEventListener('click', (e) => { if (e.target.closest('a')) setMenu(false); });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && shell.classList.contains('nav-open')) { setMenu(false); menuButton.focus(); }
  });
  root.append(shell);
}

async function route() {
  let { key, args } = parseHash();
  const fallback = state.user.role === 'admin' ? 'overview' : 'account';
  if (!pageAllowed(key)) {
    history.replaceState(null, '', `#/${fallback}`);
    key = fallback;
    args = [];
  }
  for (const [k, a] of state.nav) {
    if (k === key) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current');
  }
  const token = ++state.renderToken;
  if (state.cleanup) { state.cleanup(); state.cleanup = null; }
  const main = state.main;
  const previousKey = main.dataset.page;
  main.dataset.page = key;
  if (previousKey !== key) {
    clear(main).append(skeleton());
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
    isCurrent: () => token === state.renderToken,
    onLeave: (fn) => { state.cleanup = fn; },
    rerender: () => route(),
  };
  try {
    const module = await page.load();
    const view = await module.render(ctx);
    if (token !== state.renderToken) return;
    clear(main).append(view);
    if (previousKey !== key && previousKey) main.querySelector('[data-page-title]')?.focus({ preventScroll: true });
  } catch (error) {
    if (token !== state.renderToken) return;
    if (error.code === 'unauthenticated') return;
    clear(main).append(h('div.page', errorState(error, () => route())));
  }
}

boot();
