// Opens the Betsson login form, or finds the page already logged in. testson runs it with
// `playwright-cli run-code` and puts the page URL in place of BETSSON_URL as a JSON string.
async (page) => {
  const loggedIn = () =>
    page.evaluate(() =>
      ['session', 'authSession', 'authReducer'].some((key) => !!localStorage.getItem(key)),
    );
  await page.goto({{BETSSON_URL}}, { waitUntil: 'load' });
  await page
    .getByRole('button', { name: 'Aceptar todas las cookies' })
    .click({ timeout: 10000 })
    .catch(() => {});
  // The header has a hidden copy of the button for narrow screens.
  const open = page
    .getByRole('button', { name: 'Iniciar sesión', exact: true })
    .filter({ visible: true })
    .first();
  await open.waitFor({ timeout: 15000 }).catch(() => {});
  if ((await loggedIn()) && !(await open.isVisible())) {
    return 'logged_in';
  }
  await open.click({ timeout: 15000 });
  await page.locator('#password-input').waitFor({ timeout: 15000 });
  return 'form';
}
