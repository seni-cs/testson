// The language and region whose conventions dates and numbers are written in.
const LOCALE = "en-GB";

// What the latest test of a game found, or `untested` for a game without tests.
const STATUS_LABELS = {
    working: "Working",
    failing: "Failing",
    test_failed: "Test failed",
    untested: "Untested",
};

// The statuses from the one that most needs looking at to the one that least does.
const SEVERITY = ["failing", "test_failed", "untested", "working"];

// A game stands where the latest test of each mode left it, whatever the earlier ones found, and
// is filtered and counted by the status of the two that most needs looking at.
function gameStatus(game) {
    const statuses = MODES.map((mode) => game[mode]?.status ?? "untested");
    return SEVERITY.find((status) => statuses.includes(status));
}

// The account a test played with. Tests from before there were two accounts were played in demo
// mode.
const MODE_LABELS = {
    real_money: "Real money",
    no_balance: "No balance",
    demo: "Demo",
};

// The modes every game is tested in, in the order they are listed.
const MODES = ["real_money", "no_balance"];

// What drove the browser in a test: the AI agent, or the script an agent run left.
const RUNNER_LABELS = { agent: "AI", script: "Script" };

// Build the mark of who ran a test.
function renderRunner(runner) {
    const mark = element("span", `runner ${runner}`, RUNNER_LABELS[runner]);
    mark.title = runner === "script" ? "Replayed from the script, without AI" : "Tested by the AI";
    return mark;
}

// The start of an ETag, enough to tell two versions of a game apart.
function shortVersion(etag) {
    return etag.replace(/^W\//, "").replaceAll('"', "").slice(0, 8);
}

// Write the day of a date, as it is in `timeZone` or, without one, where the page is read. A day
// stored without a time, such as the one a game was added on, belongs to no time zone: it is
// written in the UTC it is read in, to stay the same day.
function formatDay(date, timeZone) {
    return new Date(date).toLocaleDateString(LOCALE, { dateStyle: "medium", timeZone });
}

function element(tag, className = "", text = "") {
    const node = document.createElement(tag);
    node.className = className;
    node.textContent = text;
    return node;
}

// Build the dot of a status, for beside the name of the status.
function renderDot(status) {
    return element("span", `dot ${status}`);
}

// Build the dot of a status for where space is short and its name is not written. Only screen
// readers get the name.
function renderLabelledDot(status) {
    const dot = renderDot(status);
    dot.role = "img";
    dot.ariaLabel = STATUS_LABELS[status];
    return dot;
}

// Build the square image of a game. Its box is as big as the styles say from the start, whether
// the image has arrived or not, so nothing around it moves when it does.
function renderCover(imageUrl) {
    const image = element("img");
    // The name of the game is always written beside its image.
    image.alt = "";
    image.decoding = "async";
    image.src = imageUrl;
    // An image that cannot be had leaves its box empty, instead of drawn as broken.
    image.addEventListener("error", () => image.remove());

    const cover = element("div", "cover");
    cover.append(image);
    return cover;
}

// Build a labelled fact, whose value is made of texts and elements.
function renderFact(label, ...value) {
    const description = element("dd");
    description.append(...value);
    const fact = element("div");
    fact.append(element("dt", "", label), description);
    return fact;
}

// Build the notice that stands where a list has nothing to show yet.
function renderEmpty(title, explanation) {
    const notice = element("div", "empty");
    notice.append(element("strong", "", title), element("span", "muted", explanation));
    return notice;
}

function showError(message) {
    const banner = document.getElementById("error");
    banner.textContent = message;
    banner.hidden = false;
}

async function fetchJson(url) {
    const response = await fetch(url);
    if (!response.ok) {
        throw new Error(`the server responded with status ${response.status}`);
    }
    return response.json();
}
