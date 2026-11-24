import { call, describe } from './api.js';
import { field, input, setFieldError, date } from './ui.js';
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
      frame({ title: `Join ${name}`, text: `${describe(err)} Try the link again in a moment.` });
      return;
    }
  }
  if (!invite?.valid) {
    frame({
      title: 'This invite has ended',
      text: 'It was used, cancelled or ran out. Ask the person who sent it for a new link.',
    });
    return;
  }

  const display = input({ autocomplete: 'name', required: true });
  const username = usernameInput();
  const passwords = passwordFields();
  const form = submitForm([
    field('Your name', display),
    field('Username', username, { hint: 'What you type to sign in' }),
    ...passwords.fields,
  ], 'Create account', async (error) => {
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
      if (err.code === 'conflict') { setFieldError(username, 'Someone already has that username. Try another.'); username.focus(); return; }
      if (err.code === 'not_found') { error.show('This invite has ended. Ask for a new link.'); return; }
      error.show(describe(err));
    }
  });

  frame({
    title: `Join ${name}`,
    text: `You are invited to listen to the music on ${name}. Make an account here, then connect the Tracks app on your phone.`,
    body: form,
    foot: invite.expires_at ? `This invite works until ${date(invite.expires_at)}.` : null,
  });
  display.focus();
}

main();
