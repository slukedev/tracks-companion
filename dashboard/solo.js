// Shared frame for the pages people reach before they have a session: setup, invite, reset.

import { call, preview } from './api.js';
import { h, clear, icon, button, busy, field, input, formError, setFieldError } from './ui.js';


export async function serverName() {
  const caps = await call('GET /capabilities').catch(() => null);
  return caps?.name || 'Trackside';
}

/** The token at the end of /invite/<token> or /reset/<token>, or ?token= in preview. */
export function tokenFromPath() {
  const query = new URLSearchParams(location.search).get('token');
  if (query) return query;
  const last = location.pathname.split('/').filter(Boolean).pop() || '';
  return last.endsWith('.html') ? (preview ? 'preview-token' : '') : decodeURIComponent(last);
}

/** Where the dashboard lives relative to this page. */
export function dashboardUrl(hash = '') {
  const base = new URL('./', import.meta.url);
  if (preview) return `${base.href}index.html${location.search}${hash}`;
  return `${base.href}${hash}`;
}

export function frame({ title, text, body, foot }) {
  const app = document.getElementById('app');
  clear(app);
  app.append(h('main.solo', { id: 'main' },
    h('div.solo-card',
      h('div.solo-head', h('span.mark.lg', icon('cassette')), h('h1', title), text ? h('p', text) : null),
      body,
      foot ? h('p.solo-foot', foot) : null)));
}

/** Password and confirmation fields with matching validation. */
export function passwordFields(label = 'Password') {
  const password = input({ type: 'password', autocomplete: 'new-password', required: true });
  const repeat = input({ type: 'password', autocomplete: 'new-password', required: true });
  const fields = [
    field(label, password),
    field('Repeat', repeat),
  ];
  return {
    fields,
    password,
    validate() {
      setFieldError(password, '');
      setFieldError(repeat, '');
      if (password.value !== repeat.value) {
        setFieldError(repeat, 'Does not match.');
        repeat.focus();
        return false;
      }
      return true;
    },
  };
}

export function submitForm(children, label, onSubmit) {
  const error = formError();
  const submit = button(label, { kind: 'primary', type: 'submit', size: 'large' });
  submit.classList.add('block');
  const form = h('form.panel.form', {
    novalidate: true,
    on: {
      submit: async (event) => {
        event.preventDefault();
        error.hide();
        await busy(submit, () => onSubmit(error));
      },
    },
  }, children, error.el, submit);
  return form;
}

export function usernameInput() {
  return input({ autocomplete: 'username', autocapitalize: 'none', spellcheck: 'false', required: true, maxlength: '64', pattern: '[A-Za-z0-9._-]+' });
}

export function checkUsername(control) {
  setFieldError(control, '');
  const value = control.value.trim();
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(value)) {
    setFieldError(control, 'Letters, numbers, . - _ only.');
    control.focus();
    return null;
  }
  return value;
}
