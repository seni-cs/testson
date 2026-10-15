// Presses the submit button of the Betsson login form, which testson has filled.
async (page) => {
  await page
    .locator('dialog[data-test-id="dialog-content"]')
    .getByRole('button', { name: 'Iniciar sesión', exact: true })
    .click({ timeout: 15000 });
  return 'submitted';
}
