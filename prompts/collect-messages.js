// Run with `playwright-cli --raw run-code --filename`. Returns, oldest first, the messages every
// frame of the page received since capture-messages.js ran, with whatever looks like a token
// redacted, since they are shown on the dashboard.
async (page) => {
  const secretKey = /token|session|auth|key|pass|secret|ticket|jwt|cookie|signature/i;
  const secretValue = /^[A-Za-z0-9+/=._-]{32,}$/;
  const withoutQuery = (text) => text.replace(/^(https?:\/\/[^?#\s]*)[?#]\S*$/, "$1");
  // Many games post their messages as JSON text, whose keys need the same redaction.
  const parsed = (text) => {
    if (!/^\s*[[{]/.test(text)) {
      return undefined;
    }
    try {
      const value = JSON.parse(text);
      return value !== null && typeof value === "object" ? value : undefined;
    } catch {
      return undefined;
    }
  };
  const redact = (value, depth) => {
    if (typeof value === "string") {
      const json = parsed(value);
      const text =
        json === undefined ? withoutQuery(value) : JSON.stringify(redact(json, depth));
      if (secretValue.test(text)) {
        return "[redacted]";
      }
      return text.length > 2000 ? `${text.slice(0, 2000)}…` : text;
    }
    if (value === null || typeof value !== "object") {
      return value;
    }
    if (depth > 6) {
      return "[nested]";
    }
    if (Array.isArray(value)) {
      return value.slice(0, 100).map((item) => redact(item, depth + 1));
    }
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [
        key,
        secretKey.test(key) ? "[redacted]" : redact(item, depth + 1),
      ]),
    );
  };
  const messages = [];
  for (const frame of page.frames()) {
    let received = [];
    try {
      received = await frame.evaluate(() => window.__testsonMessages ?? []);
    } catch {
      continue;
    }
    const direction = frame === page.mainFrame() ? "to_lobby" : "to_frame";
    const frameUrl = frame.url().split(/[?#]/)[0];
    for (const message of received) {
      messages.push({
        at: message.at,
        direction,
        origin: message.origin,
        frame_url: frameUrl,
        data: redact(message.data, 0),
      });
    }
  }
  messages.sort((a, b) => a.at.localeCompare(b.at));
  return messages;
}
