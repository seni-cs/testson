const CONSOLE_LEVELS = { error: "Error", warning: "Warning" };

const DIRECTIONS = { to_lobby: "To lobby", to_frame: "To game" };

const SCRIPT_OUTCOMES = {
    completed: "Completed",
    step_failed: "A step of the script failed",
    failed_after_spin: "A step failed after the spin",
    version_changed: "New version of the game",
    blocked: "Blocked by the account or the stake",
};

const RETIRED_REASONS = {
    version_changed: "the game changed version",
    step_failed: "a step stopped working",
    replaced: "the AI left a new one",
};

function formatDateTime(timestamp) {
    return new Date(timestamp).toLocaleString(LOCALE, { dateStyle: "medium", timeStyle: "short" });
}

function formatTestTime(timestamp) {
    return new Date(timestamp).toLocaleTimeString(LOCALE, { timeStyle: "short" });
}

function formatShortDateTime(timestamp) {
    return new Date(timestamp).toLocaleString(LOCALE, { dateStyle: "short", timeStyle: "short" });
}

function formatDuration(milliseconds) {
    const seconds = (milliseconds / 1000).toLocaleString(LOCALE, {
        minimumFractionDigits: 1,
        maximumFractionDigits: 1,
    });
    return `${seconds} s`;
}

function formatCount(count, singular, plural) {
    return `${count} ${count === 1 ? singular : plural}`;
}

function openViewer(media, caption) {
    const figure = document.getElementById("media");
    figure.replaceChildren(media);
    if (caption) {
        figure.append(element("figcaption", "", caption));
    }
    document.getElementById("viewer").showModal();
}

function setUpViewer() {
    const viewer = document.getElementById("viewer");
    // A click whose target is the dialog itself fell on the backdrop around its content.
    viewer.addEventListener("click", (event) => {
        if (event.target === viewer) {
            viewer.close();
        }
    });
    // Emptying the viewer stops a video that is still playing.
    viewer.addEventListener("close", () => document.getElementById("media").replaceChildren());
}

function renderError(error) {
    // A stack usually starts with the message, but the schema does not guarantee it.
    const stack = error.stack ?? "";
    const text = stack.includes(error.message) ? stack : `${error.message}\n${stack}`;
    return element("pre", "failure", text.trim());
}

function renderProperties(test) {
    const properties = element("dl", "properties");
    // Tests stored before there were scripts were all run by the agent.
    const runner = test.runner ?? "agent";
    const ranBy = [renderRunner(runner)];
    if (runner === "script" && test.script_id != null) {
        ranBy.push(` ${test.script_id}`);
    }
    properties.append(
        renderFact("Result", STATUS_LABELS[test.status]),
        renderFact("Run by", ...ranBy),
        renderFact("Duration", formatDuration(test.duration_ms)),
    );
    if (test.script_outcome) {
        properties.append(renderFact("Script", SCRIPT_OUTCOMES[test.script_outcome]));
    }
    if (test.game_version) {
        const version = element("code", "", shortVersion(test.game_version.etag));
        version.title = `${test.game_version.build_url} · ${test.game_version.etag}`;
        properties.append(renderFact("Version", version));
    }

    const { browser, viewport } = test.environment ?? {};
    if (browser) {
        properties.append(renderFact("Browser", browser));
    }
    if (viewport) {
        properties.append(renderFact("Resolution", `${viewport.width}×${viewport.height}`));
    }
    if (test.game_url) {
        const game = element("a", "", "Open");
        game.href = test.game_url;
        game.target = "_blank";
        game.rel = "noreferrer";
        properties.append(renderFact("Game", game));
    }
    return properties;
}

function renderScreenshot(screenshot) {
    const caption = screenshot.caption ?? "";
    // The image is all the button holds, so its description is also what names the button.
    const description = caption || "Screenshot";

    const preview = element("img");
    // The loading mode only takes effect when it is set before the source.
    preview.loading = "lazy";
    preview.src = screenshot.path;
    preview.alt = description;

    const thumbnail = element("button", "thumbnail");
    thumbnail.type = "button";
    thumbnail.append(preview);
    thumbnail.addEventListener("click", () => {
        const image = element("img");
        image.src = screenshot.path;
        image.alt = description;
        openViewer(image, caption);
    });

    const figure = element("figure");
    figure.append(thumbnail);
    if (caption) {
        figure.append(element("figcaption", "", caption));
    }
    return figure;
}

function renderVideo(path) {
    const thumbnail = element("button", "thumbnail video");
    thumbnail.type = "button";
    thumbnail.ariaLabel = "Play the recording";
    // The video element is only created on request, so the recording is not fetched before.
    thumbnail.addEventListener("click", () => {
        const video = element("video");
        video.src = path;
        video.controls = true;
        video.autoplay = true;
        openViewer(video, "Recording of the test");
    });

    const figure = element("figure");
    figure.append(thumbnail, element("figcaption", "", "Recording"));
    return figure;
}

function renderLogRow(badge, text, source) {
    const body = element("div");
    body.append(element("span", "", text));
    if (source) {
        body.append(element("small", "", source));
    }

    const row = element("li");
    row.append(badge, body);
    return row;
}

function renderLog(title, badges, rows) {
    const summary = element("summary", "", title);
    summary.append(...badges);
    const list = element("ul");
    list.append(...rows);

    const log = element("details", "log");
    log.append(summary, list);
    return log;
}

function renderConsole(messages) {
    const errors = messages.filter((message) => message.level === "error").length;
    const warnings = messages.length - errors;
    const badges = [];
    if (errors > 0) {
        badges.push(element("span", "badge error", formatCount(errors, "error", "errors")));
    }
    if (warnings > 0) {
        badges.push(element("span", "badge warning", formatCount(warnings, "warning", "warnings")));
    }

    const rows = messages.map((message) => {
        const badge = element("span", `badge ${message.level}`, CONSOLE_LEVELS[message.level]);
        return renderLogRow(badge, message.text, message.source_url);
    });
    return renderLog("Console", badges, rows);
}

function renderFailedRequests(requests) {
    const rows = requests.map((request) => {
        const badge = element("span", "badge failed", request.status_code ?? request.failure);
        return renderLogRow(badge, `${request.method} ${request.url}`);
    });
    const count = element("span", "badge failed", requests.length);
    return renderLog("Failed requests", [count], rows);
}

function renderPostMessages(messages) {
    const rows = messages.map((message) => {
        const badge = element("span", "badge", DIRECTIONS[message.direction]);
        const time = new Date(message.at).toLocaleTimeString(LOCALE);
        const source = [time, message.origin].filter(Boolean).join(" · ");
        return renderLogRow(badge, JSON.stringify(message.data), source);
    });
    const count = element("span", "badge", messages.length);
    return renderLog("postMessage messages", [count], rows);
}

function renderStep(step) {
    const item = element("li", `step ${step.status}`);
    const heading = element("p", "step-heading");
    // The Playwright action is for whoever replays the test; the description is for everyone.
    if (step.action) {
        heading.append(element("code", "action", step.action), " ");
    }
    heading.append(step.description);
    item.append(heading);
    if (step.error) {
        item.append(renderError(step.error));
    }
    if (step.screenshots?.length) {
        const media = element("div", "media");
        media.append(...step.screenshots.map(renderScreenshot));
        item.append(media);
    }
    if (step.console_messages?.length) {
        item.append(renderConsole(step.console_messages));
    }
    if (step.failed_requests?.length) {
        item.append(renderFailedRequests(step.failed_requests));
    }
    return item;
}

function renderDocument(title, test) {
    const facts = element("div");
    facts.append(title);
    // Tests stored before the summary was part of the document have none.
    if (test.summary) {
        facts.append(element("p", "summary", test.summary));
    }
    facts.append(renderProperties(test));
    const overview = element("div", "overview");
    overview.append(facts);
    if (test.video_path) {
        overview.append(renderVideo(test.video_path));
    }

    const timeline = element("ol", "timeline");
    timeline.append(...test.steps.map(renderStep));

    const content = [overview];
    if (test.error) {
        const failure = renderError(test.error);
        failure.classList.add(test.status);
        content.push(failure);
    }
    if (test.post_messages?.length) {
        content.push(renderPostMessages(test.post_messages));
    }
    content.push(timeline);
    return content;
}

// Build what a test shows under its `title`: its stored document or, failing that, why it is not
// there. A test that did not arrive and one that arrived but cannot be drawn are reported apart.
async function loadTest(test, title) {
    let stored;
    try {
        stored = await fetchJson(`/api/test/${test.id}`);
    } catch (error) {
        const retry = element("button", "button", "Retry");
        retry.type = "button";
        retry.addEventListener("click", () => showTest(test));
        const message = `Could not load the test: ${error.message}`;
        return [title, element("p", "failure", message), retry];
    }

    try {
        return renderDocument(title, stored);
    } catch (error) {
        // The message on the page is for its reader; the console keeps where it came from.
        console.error(error);
        return [title, element("p", "failure", `Could not show the test: ${error.message}`)];
    }
}

// The test on screen, and how many tests have been asked for, to tell the answer to the latest
// request from those to earlier ones.
let shownTestId = null;
let requests = 0;

async function showTest(test) {
    const article = document.getElementById("test");
    const title = element("div", "test-title");
    title.append(
        renderLabelledDot(test.status),
        element("h2", "", `${MODE_LABELS[test.mode]} · ${formatDateTime(test.started_at)}`),
    );
    shownTestId = test.id;
    requests += 1;
    const request = requests;
    article.replaceChildren(title);
    article.hidden = false;

    const content = await loadTest(test, title);
    // A slow answer must not land on what was asked for after it, be it another test or the
    // same one again.
    if (request === requests) {
        article.replaceChildren(...content);
    }
}

function testHash(test) {
    return `#test-${test.id}`;
}

function renderTestLink(test) {
    const link = element("a", "test-link");
    link.href = testHash(test);
    const runner = test.runner ?? "agent";
    link.ariaLabel = `${MODE_LABELS[test.mode]} · ${formatDateTime(test.started_at)} · ${STATUS_LABELS[test.status]} · ${RUNNER_LABELS[runner]} · Test ${test.id}`;
    const label = element("span", "test-mode-label", MODE_LABELS[test.mode]);
    // Only the tests the AI ran are marked, the exception among the replays.
    if (runner === "agent") {
        label.append(renderRunner(runner));
    }
    link.append(renderLabelledDot(test.status), label);
    return link;
}

// Group a game's tests by the same local calendar day used to display their dates.
function renderTestGroups(tests) {
    const days = new Map();
    const newestFirst = [...tests].sort(
        (a, b) => new Date(b.started_at) - new Date(a.started_at) || b.id - a.id,
    );
    for (const test of newestFirst) {
        const date = new Date(test.started_at);
        const key = `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
        if (!days.has(key)) {
            days.set(key, []);
        }
        days.get(key).push(test);
    }

    return Array.from(days.values(), (day) => {
        const group = element("section", "test-day");
        const heading = element("h3", "test-day-heading", formatDay(day[0].started_at));
        const earliest = day[day.length - 1].started_at;
        const time = element("time", "when", formatTestTime(earliest));
        time.dateTime = earliest;
        heading.append(time);
        const columns = element("div", "test-modes");
        // Preserve reports from before the two account modes as their own column.
        const modes = day.some((test) => test.mode !== "demo") ? [...MODES] : [];
        if (day.some((test) => test.mode === "demo")) {
            modes.push("demo");
        }
        for (const mode of modes) {
            const matching = day.filter((test) => test.mode === mode);
            if (!matching.length) {
                continue;
            }
            const column = element("div", `test-mode ${mode}`);
            const list = element("div", "test-mode-list");
            list.append(...matching.map(renderTestLink));
            column.append(list);
            columns.append(column);
        }
        group.append(heading, columns);
        return group;
    });
}

// Scroll the list of tests, and nothing but the list, just enough to bring one of them in sight.
function revealTestLink(link) {
    const list = document.getElementById("tests");
    const bounds = list.getBoundingClientRect();
    const selected = link.getBoundingClientRect();
    if (selected.top < bounds.top) {
        list.scrollTop += selected.top - bounds.top;
    } else if (selected.bottom > bounds.bottom) {
        list.scrollTop += selected.bottom - bounds.bottom;
    }
}

function showSelectedTest(tests) {
    const named = tests.find((test) => location.hash === testHash(test));
    // An address that names a test this game does not have is corrected to name none, which
    // is the address of the latest test.
    if (!named && location.hash) {
        history.replaceState(null, "", location.pathname);
    }
    // Tests come newest first.
    const selected = named ?? tests[0];
    for (const link of document.querySelectorAll("#tests .test-link")) {
        const isSelected = link.hash === testHash(selected);
        link.ariaCurrent = String(isSelected);
        // The list only has room for some tests, so it is scrolled to the selected one.
        if (isSelected) {
            revealTestLink(link);
        }
    }
    // The address can change without the selection changing, as when the latest test is clicked.
    if (shownTestId !== selected.id) {
        showTest(selected);
    }
}

function renderBanner(game) {
    const demo = game.is_available_for_fun
        ? element("span", "chip available", "Demo mode")
        : element("span", "chip unavailable", "No demo mode");
    const meta = element("div", "meta");
    meta.append(element("span", "provider", game.provider));
    if (game.primary_type) {
        meta.append(element("span", "chip", game.primary_type));
    }
    meta.append(demo, element("span", "chip", `Added on ${formatDay(game.added_on, "UTC")}`));

    const about = element("div");
    about.append(element("h1", "", game.name), meta);
    return [renderCover(game.image_url), about];
}

function historyEvents(game) {
    const events = [];
    const add = (at, mode, text, testId) => events.push({ at, mode, text, testId });
    for (const mode of MODES) {
        // Tests come newest first; their history is told from the oldest.
        const tests = game.tests.filter((test) => test.mode === mode).reverse();
        let version = null;
        let verdict = null;
        tests.forEach((test, index) => {
            if (index === 0) {
                add(test.started_at, mode, `First test, by ${RUNNER_LABELS[test.runner]}`, test.id);
            }
            if (test.etag && version && test.etag !== version) {
                const change = `${shortVersion(version)} → ${shortVersion(test.etag)}`;
                add(test.started_at, mode, `New version of the game: ${change}`, test.id);
            }
            version = test.etag ?? version;
            if (test.script_outcome && test.script_outcome !== "completed") {
                const next = {
                    blocked: "",
                    failed_after_spin: ", the AI tests it at the next test",
                }[test.script_outcome] ?? ", the AI tests it";
                const text = `Script ${test.script_id}: ${SCRIPT_OUTCOMES[test.script_outcome]}${next}`;
                add(test.started_at, mode, text, test.id);
            }
            if (test.status === "working" || test.status === "failing") {
                if (verdict && verdict !== test.status) {
                    const change = `${STATUS_LABELS[verdict]} → ${STATUS_LABELS[test.status]}`;
                    add(test.started_at, mode, `Status: ${change}`, test.id);
                }
                verdict = test.status;
            }
        });
    }
    for (const script of game.scripts) {
        const created = `Script ${script.id} created by the AI, version ${shortVersion(script.etag)}`;
        add(script.created_at, script.mode, created, script.created_by_test);
        if (script.retired_at) {
            const why = RETIRED_REASONS[script.retired_reason];
            add(script.retired_at, script.mode, `Script ${script.id} retired: ${why}`, null);
        }
    }
    return events.sort((a, b) => b.at.localeCompare(a.at));
}

function renderHistory(game) {
    return historyEvents(game).map((event) => {
        const text = element(event.testId ? "a" : "span", "", event.text);
        if (event.testId) {
            text.href = `#test-${event.testId}`;
        }
        const item = element("li");
        item.append(
            element("span", "when", formatShortDateTime(event.at)),
            element("span", "mode", MODE_LABELS[event.mode]),
            text,
        );
        return item;
    });
}

async function showGame() {
    const gameId = location.pathname.split("/").pop();
    const game = await fetchJson(`/api/game/${gameId}`);
    document.title = `${game.name} · testson`;
    document.getElementById("game").append(...renderBanner(game));

    if (game.tests.length === 0) {
        const article = document.getElementById("test");
        article.append(
            renderEmpty(
                "This game has no tests yet",
                "Its results will show here as soon as the first one runs.",
            ),
        );
        article.hidden = false;
        return;
    }

    document.getElementById("count").textContent = game.tests.length;
    document.getElementById("tests").append(...renderTestGroups(game.tests));
    document.getElementById("sidebar").hidden = false;
    document.getElementById("events").append(...renderHistory(game));
    document.getElementById("history").hidden = false;
    window.addEventListener("hashchange", () => showSelectedTest(game.tests));
    showSelectedTest(game.tests);
}

setUpViewer();
showGame().catch((error) => showError(`Could not load the game: ${error.message}`));
