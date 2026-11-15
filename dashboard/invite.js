import { call, describe } from './api.js';
import { field, input, setFieldError } from './ui.js';
import { frame, passwordFields, submitForm, usernameInput, checkUsername, tokenFromPath, dashboardUrl, serverName } from './solo.js';

async function main() {
  const token = tokenFromPath();
  const name = await serverName();
  document.title = `Join ${name}`;
  let invite = null;
  try {
    invite = token ? await call('GET /invites/{token}', { values: { token }, quietAuth: true }) : null;
  } catch (err) {
    if (err.code !== 'not_found') {
      frame({ title: `Join ${name}`, text: describe(err) });
      return;
    }
  }
  if (!invite?.valid) {
    frame({
      title: 'This invite has ended',
      text: 'Ask for a new link.',
    });
    return;
  }

  const display = input({ autocomplete: 'name', required: true });
  const username = usernameInput();
  const passwords = passwordFields();
  const form = submitForm([
    field('Your Name', display),
    field('Username', username),
    ...passwords.fields,
  ], 'Join', async (error) => {
    const user = checkUsername(username);
    if (!user || !passwords.validate()) return;
    try {
      await call('POST /invites/{token}/accept', { values: { token }, body: {
        username: user,
        display_name: display.value.trim() || user,
        password: passwords.password.value,
      }, quietAuth: true });
      location.replace(dashboardUrl('#/account/pair'));
    } catch (err) {
      if (err.code === 'conflict') { setFieldError(username, 'Taken.'); username.focus(); return; }
      if (err.code === 'not_found') { error.show('This invite has ended.'); return; }
      error.show(describe(err));
    }
  });

  frame({
    title: `Join ${name}`,
    body: form,
  });
  display.focus();
}

main();
