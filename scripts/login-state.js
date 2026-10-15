// Whether the page holds a Betsson login yet, and the error the login form shows, if any.
async (page) => {
  const loggedIn = await page.evaluate(() =>
    ['session', 'authSession', 'authReducer'].some((key) => !!localStorage.getItem(key)),
  );
  const alert = page
    .locator('dialog[data-test-id="dialog-content"] [role="alert"]')
    .filter({ visible: true })
    .first();
  const message = (await alert.isVisible()) ? (await alert.innerText()).trim() : '';
  return { loggedIn, message };
}
