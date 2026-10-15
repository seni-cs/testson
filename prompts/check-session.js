async (page) => {
  /*
   * Reads whether the page is logged in. It only looks: it never clicks, types, or navigates.
   * The session lives in browser storage, so the first render can show the logged-out header
   * before the site has validated the token. The script waits for the network to settle, then
   * samples the site header once a second until three samples in a row agree, for at most
   * 20 seconds. loggedIn is true when only logged-in controls show in the header, false when
   * only logged-out controls show, and null when neither or both show or the header never
   * settled. Neither false nor null is a login: the agent confirms with a snapshot.
   */
  const deadline = Date.now() + 20000;
  const loggedOutNames = /^\s*(iniciar sesión|crear cuenta)\s*$/i;
  const loggedInNames = /^\s*(depositar|mi cuenta|cerrar sesión)\s*$/i;
  const header = page.getByRole('banner').first();
  const controls = (name) =>
    header.getByRole('button', { name }).or(header.getByRole('link', { name }));
  const visibleNames = async (locator) => {
    const names = [];
    for (const element of await locator.all().catch(() => [])) {
      if (await element.isVisible().catch(() => false)) {
        const snapshot = await element.ariaSnapshot().catch(() => '');
        names.push(snapshot.split(/\r?\n/)[0].replace(/^- /, '').replace(/:$/, ''));
      }
    }
    return [...new Set(names)].sort();
  };
  const sample = async () => {
    const loggedOutMarkers = await visibleNames(controls(loggedOutNames));
    const loggedInMarkers = await visibleNames(controls(loggedInNames));
    let loggedIn = null;
    if (loggedInMarkers.length > 0 && loggedOutMarkers.length === 0) {
      loggedIn = true;
    } else if (loggedOutMarkers.length > 0 && loggedInMarkers.length === 0) {
      loggedIn = false;
    }
    return { loggedIn, loggedOutMarkers, loggedInMarkers };
  };
  await page.waitForLoadState('networkidle', { timeout: 8000 }).catch(() => {});
  let previous = null;
  let agreeing = 0;
  let samples = 0;
  while (true) {
    const current = await sample();
    samples += 1;
    agreeing = previous && JSON.stringify(previous) === JSON.stringify(current) ? agreeing + 1 : 1;
    previous = current;
    if (agreeing >= 3) {
      return { ...current, stable: true, samples };
    }
    if (Date.now() + 1000 > deadline) {
      return { ...current, loggedIn: null, stable: false, samples };
    }
    await page.waitForTimeout(1000);
  }
}
