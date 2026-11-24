import { call, describe } from './api.js';
import { h } from './ui.js';
import { frame, passwordFields, submitForm, tokenFromPath, dashboardUrl } from './solo.js';

async function main() {
  const token = tokenFromPath();
  document.title = 'Choose a new password';
  try {
    if (!token) throw Object.assign(new Error(), { code: 'not_found' });
    await call('GET /password-resets/{token}', { values: { token }, quietAuth: true });
  } catch (err) {
    frame({
      title: 'This link has ended',
      text: err.code === 'not_found' ? 'It was used or ran out. Ask whoever runs this server for a new link.' : describe(err),
    });
    return;
  }
  const passwords = passwordFields('New password');
  const form = submitForm(passwords.fields, 'Change password', async (error) => {
    if (!passwords.validate()) return;
    try {
      await call('POST /password-resets/{token}', { values: { token }, body: { password: passwords.password.value }, quietAuth: true });
      frame({
        title: 'Password changed',
        text: 'Sign in with your new password.',
        body: h('a.btn.primary.large.block', { href: dashboardUrl() }, 'Sign in'),
      });
    } catch (err) {
      error.show(err.code === 'not_found' ? 'This link has run out. Ask for a new one.' : describe(err));
    }
  });
  frame({ title: 'Choose a new password', body: form });
  passwords.password.focus();
}

main();
