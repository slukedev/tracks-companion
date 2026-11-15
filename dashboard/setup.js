import { call, describe } from './api.js';
import { field, input, setFieldError } from './ui.js';
import { frame, passwordFields, submitForm, usernameInput, checkUsername, dashboardUrl } from './solo.js';

async function main() {
  const status = await call('GET /setup/status').catch(() => ({ setup_required: true }));
  if (!status.setup_required) {
    location.replace(dashboardUrl());
    return;
  }
  document.title = 'Set Up Trackside';
  const token = input({ autocomplete: 'off', autocapitalize: 'characters', spellcheck: 'false', required: true, mono: true });
  const name = input({ autocomplete: 'name', required: true });
  const username = usernameInput();
  const passwords = passwordFields();

  const form = submitForm([
    field('Setup Token', token, { hint: 'In the server log' }),
    field('Your Name', name),
    field('Username', username),
    ...passwords.fields,
  ], 'Create Admin Account', async (error) => {
    setFieldError(token, '');
    if (!token.value.trim()) { setFieldError(token, 'Paste the token.'); token.focus(); return; }
    const user = checkUsername(username);
    if (!user || !passwords.validate()) return;
    try {
      await call('POST /setup', { body: {
        setup_token: token.value.replace(/\s+/g, ''),
        username: user,
        display_name: name.value.trim() || user,
        password: passwords.password.value,
      }, quietAuth: true });
      location.replace(dashboardUrl('#/overview'));
    } catch (err) {
      if (err.code === 'unauthenticated') { setFieldError(token, 'Wrong token.'); token.focus(); return; }
      if (err.code === 'conflict') { location.replace(dashboardUrl()); return; }
      error.show(describe(err));
    }
  });

  frame({
    title: 'Set Up Trackside',
    text: null,
    body: form,
  });
  token.focus();
}

main();
