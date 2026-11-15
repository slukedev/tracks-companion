import { call, describe } from './api.js';
import { h } from './ui.js';
import { frame, passwordFields, submitForm, tokenFromPath, dashboardUrl } from './solo.js';

async function main() {
  const token = tokenFromPath();
  document.title = 'Reset Password';
  try {
    if (!token) throw Object.assign(new Error(), { code: 'not_found' });
    await call('GET /password-resets/{token}', { values: { token }, quietAuth: true });
  } catch (err) {
    frame({
      title: 'This link has ended',
      text: err.code === 'not_found' ? 'Ask your admin for a new link.' : describe(err),
    });
    return;
  }
  const passwords = passwordFields('New Password');
  const form = submitForm(passwords.fields, 'Change Password', async (error) => {
    if (!passwords.validate()) return;
    try {
      await call('POST /password-resets/{token}', { values: { token }, body: { password: passwords.password.value }, quietAuth: true });
      frame({
        title: 'Password changed',
        text: null,
        body: h('a.btn.primary.large.block', { href: dashboardUrl() }, 'Sign In'),
      });
    } catch (err) {
      error.show(err.code === 'not_found' ? 'This link has expired.' : describe(err));
    }
  });
  frame({ title: 'New Password', body: form });
  passwords.password.focus();
}

main();
