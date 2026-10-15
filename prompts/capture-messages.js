// Run with `playwright-cli run-code --filename` before the game opens. Every document the browser
// loads from then on, the lobby and each of its frames, keeps the messages it receives, for
// collect-messages.js to read back.
async (page) => {
  await page.context().addInitScript(() => {
    if (window.__testsonMessages) {
      return;
    }
    const received = (window.__testsonMessages = []);
    window.addEventListener("message", (event) => {
      if (received.length >= 500) {
        return;
      }
      let data;
      try {
        data = JSON.parse(JSON.stringify(event.data ?? null));
      } catch {
        data = String(event.data);
      }
      received.push({ at: new Date().toISOString(), origin: event.origin, data });
    });
  });
  return { capturing: true };
}
