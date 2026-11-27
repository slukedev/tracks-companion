import { call, describe, now } from '../api.js';
import { h, icon, pageHead, button, busy, input, agoInline, platformName, platformIcon, toast, nextId } from '../ui.js';

// A TV that cannot scan a QR code or take a long code shows 8 characters (XXXX-XXXX). Someone
// signed in types them here (or arrives from the TV's link with ?code=), checks what it is,
// and approves or denies it. The TV collects its key by itself.

const ALPHABET = /[^0-9A-HJKMNP-TV-Z]/g;

/** What the person typed, as the TV shows it: upper case, look-alike letters read as digits, XXXX-XXXX. */
function tidy(text) {
  const plain = text.toUpperCase().replace(/O/g, '0').replace(/[IL]/g, '1').replace(ALPHABET, '').slice(0, 8);
  return plain.length > 4 ? `${plain.slice(0, 4)}-${plain.slice(4)}` : plain;
}

/** The approver's answers, in words that say what to do next. */
const PROBLEMS = {
  not_found: 'No TV is showing that code. Check it against the TV and try again.',
  link_expired: 'That code has expired. Go back to the sign-in screen on the TV to get a new one.',
  link_used: 'That code has been used already. If the TV is still waiting, ask it for a new code.',
  link_locked: 'That code was locked after too many wrong tries. Ask the TV for a new code.',
};

function problemFor(error) {
  if (error.code === 'conflict' && error.details?.reason === 'link_used') return PROBLEMS.link_used;
  if (error.code === 'rate_limited') {
    return error.retryAfter
      ? `Too many wrong codes. Try again in ${error.retryAfter} seconds.`
      : 'Too many wrong codes. Wait a few minutes, then try again.';
  }
  return PROBLEMS[error.code] || null;
}

export async function render(ctx) {
  const stage = h('div.link-stage');
  const page = h('div.page.narrow',
    pageHead('Connect a TV', { lede: 'Type the code the TV shows. Approving signs the TV in to your account.', crumb: ['My account', '#/account'] }),
    stage);
  let timer = 0;
  ctx.onLeave(() => clearTimeout(timer));

  const show = (view) => {
    clearTimeout(timer);
    stage.replaceChildren(view);
    view.querySelector('[data-focus]')?.focus({ preventScroll: true });
  };

  const entry = (code = '', problem = '') => {
    const field = input({
      id: nextId('code'), mono: true, value: tidy(code), maxlength: '9', placeholder: 'XXXX-XXXX',
      autocapitalize: 'characters', autocomplete: 'one-time-code', spellcheck: 'false', enterkeyhint: 'go',
      'data-focus': '',
    });
    field.classList.add('code-input');
    const hint = h('p.hint', { id: `${field.id}-hint` }, 'Letters and numbers. Case and the dash do not matter.');
    const error = h('p.error', { id: `${field.id}-error`, role: 'alert', hidden: true });
    field.setAttribute('aria-describedby', `${hint.id} ${error.id}`);
    // Held here rather than looked up by id: the form may not be in the document yet.
    const setError = (message) => {
      error.textContent = message;
      error.hidden = !message;
      if (message) field.setAttribute('aria-invalid', 'true'); else field.removeAttribute('aria-invalid');
    };
    field.addEventListener('input', () => {
      const end = field.selectionEnd === field.value.length;
      field.value = tidy(field.value);
      if (end) field.setSelectionRange(field.value.length, field.value.length);
      setError('');
    });
    const submit = button('Continue', { kind: 'primary', type: 'submit' });
    const form = h('form.panel.form.reveal', {
      novalidate: true,
      on: {
        submit: async (event) => {
          event.preventDefault();
          const value = tidy(field.value);
          if (value.length !== 9) { setError('The code has 8 characters.'); field.focus(); return; }
          await lookup(value, submit, (message) => { setError(message); field.select(); });
        },
      },
    },
    h('div.field',
      h('label', { for: field.id }, 'Code on the TV'),
      h('div.btn-row', field, submit),
      hint,
      error));
    show(form);
    setError(problem);
    return form;
  };

  const lookup = async (code, control, onProblem) => {
    const run = async () => {
      try {
        const link = await call('POST /link/lookup', { body: { user_code: code } });
        confirmView(code, link);
      } catch (err) {
        const problem = problemFor(err);
        if (problem) onProblem(problem);
        else onProblem(describe(err));
      }
    };
    if (control) await busy(control, run);
    else await run();
  };

  const confirmView = (code, link) => {
    const device = link.device || {};
    const kind = [platformName(device.platform), device.model].filter(Boolean).join(', ');
    const status = h('p.small.muted', { role: 'status', hidden: true });
    const say = (text) => { status.textContent = text; status.hidden = !text; };
    const approve = button('Approve', { kind: 'primary', size: 'large' });
    const deny = button('Deny', { kind: 'danger', size: 'large' });
    const other = button('Use a different code', { kind: 'plain' });
    other.addEventListener('click', () => entry());

    const left = () => Math.max(0, Date.parse(link.expires_at) - now());
    const total = Math.max(1, Date.parse(link.expires_at) - Date.parse(link.requested_at));
    const bar = h('div.meter', { role: 'presentation' }, h('span'));
    const countdown = h('span.num');
    // The bar drains on the compositor at the display's own rate; the text changes once a second.
    bar.firstChild.animate([{ transform: `scaleX(${left() / total})` }, { transform: 'scaleX(0)' }], { duration: left(), easing: 'linear', fill: 'forwards' });
    const tick = () => {
      const ms = left();
      const secs = Math.ceil(ms / 1000);
      countdown.textContent = `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
      if (ms <= 0) {
        entry(code, PROBLEMS.link_expired);
        return;
      }
      timer = setTimeout(tick, (ms % 1000) || 1000);
    };

    const decide = (route, btn) => busy(btn, async () => {
      approve.disabled = true;
      deny.disabled = true;
      try {
        const result = await call(route, { body: { user_code: code } });
        doneView(result);
      } catch (err) {
        const problem = problemFor(err);
        if (problem && err.code !== 'rate_limited') entry(code, problem);
        else say(problem || describe(err));
      } finally {
        approve.disabled = false;
        deny.disabled = false;
      }
    });
    approve.addEventListener('click', () => decide('POST /link/approve', approve));
    deny.addEventListener('click', () => decide('POST /link/deny', deny));

    const where = link.nearby
      ? h('p.link-where', icon('check', 'sm tint-good'), h('span', 'Asked from your home network'))
      : h('div.callout', icon('warning', 'sm'), h('p', 'This TV is not on your home network. Approve it only if you are setting it up yourself, never because someone asked you to.'));

    const titleId = nextId('t');
    const view = h('section.panel.link-confirm.reveal', { 'aria-labelledby': titleId },
      h('div.link-device',
        h('span.link-device-icon', icon(platformIcon(device.platform))),
        h('div.link-device-text',
          h('h2', { id: titleId }, device.name || 'A new device'),
          h('p.muted', [kind, `asked ${agoInline(link.requested_at)}`].filter(Boolean).join(' · ')))),
      h('p.code-big.link-code', { 'aria-label': `Code ${code.split('').join(' ')}` }, code),
      where,
      h('div.countdown', h('p.small.muted', 'The code works for another ', countdown), bar),
      h('div.btn-row', approve, deny, h('span.link-other', other)),
      status);
    approve.dataset.focus = '';
    show(view);
    tick();
  };

  const doneView = (result) => {
    const name = result.device?.name || 'The TV';
    const approved = result.state === 'approved';
    const again = button('Connect another TV', {});
    again.addEventListener('click', () => entry());
    const done = h('a.btn.primary', { href: '#/account', 'data-focus': '' }, 'Done');
    show(h('section.panel.link-done.reveal', { role: 'status' },
      h('span', { class: `link-done-mark ${approved ? 'good' : 'muted'}` }, icon(approved ? 'check' : 'close')),
      h('h2', approved ? `${name} is connecting` : `${name} was not connected`),
      h('p.muted', approved
        ? 'It finishes on its own in a few seconds and then shows your music. It appears under Your devices, where you can remove it at any time.'
        : 'The TV has been told. Nothing was added to your account.'),
      h('div.btn-row', done, again)));
    if (approved) toast(`${name} approved`);
  };

  const code = tidy(ctx.args[0] || '');
  if (code.length === 9) {
    // Arrived from the TV's link: look the code up straight away, and fall back to the form.
    stage.append(h('div.panel', { 'aria-busy': 'true' }, h('span.skeleton.w40'), h('span.skeleton.w60')));
    await lookup(code, null, (problem) => entry(code, problem));
  } else {
    entry(code);
  }
  return page;
}
