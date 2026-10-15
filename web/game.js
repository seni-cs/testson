const CONSOLE_LEVELS = { error: "Error", warning: "Aviso" };

function formatDateTime(timestamp) {
    return new Date(timestamp).toLocaleString(LOCALE, { dateStyle: "medium", timeStyle: "short" });
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

// Show an image or a video over the page until the viewer is closed.
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
    properties.append(
        renderFact("Resultado", STATUS_LABELS[test.status]),
        renderFact("Duración", formatDuration(test.duration_ms)),
    );

    const { browser, viewport } = test.environment ?? {};
    if (browser) {
        properties.append(renderFact("Navegador", browser));
    }
    if (viewport) {
        properties.append(renderFact("Resolución", `${viewport.width}×${viewport.height}`));
    }
    if (test.game_url) {
        const game = element("a", "", "Abrir");
        game.href = test.game_url;
        game.target = "_blank";
        game.rel = "noreferrer";
        properties.append(renderFact("Juego", game));
    }
    if (test.trace_path) {
        const trace = element("a", "", "Descargar traza");
        trace.href = test.trace_path;
        properties.append(renderFact("Traza", trace));
    }
    return properties;
}

function renderScreenshot(screenshot) {
    const caption = screenshot.caption ?? "";
    // The image is all the button holds, so its description is also what names the button.
    const description = caption || "Captura de pantalla";

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
    thumbnail.ariaLabel = "Reproducir la grabación";
    // The video element is only created on request, so the recording is not fetched before.
    thumbnail.addEventListener("click", () => {
        const video = element("video");
        video.src = path;
        video.controls = true;
        video.autoplay = true;
        openViewer(video, "Grabación del test");
    });

    const figure = element("figure");
    figure.append(thumbnail, element("figcaption", "", "Grabación"));
    return figure;
}

// Build a row of a log: its badge, its text and, underneath, where it came from if known.
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

// Build a collapsed list of rows, whose always visible summary holds the title and the badges.
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
        badges.push(element("span", "badge error", formatCount(errors, "error", "errores")));
    }
    if (warnings > 0) {
        badges.push(element("span", "badge warning", formatCount(warnings, "aviso", "avisos")));
    }

    const rows = messages.map((message) => {
        const badge = element("span", `badge ${message.level}`, CONSOLE_LEVELS[message.level]);
        return renderLogRow(badge, message.text, message.source_url);
    });
    return renderLog("Consola", badges, rows);
}

function renderFailedRequests(requests) {
    const rows = requests.map((request) => {
        const badge = element("span", "badge failed", request.status_code ?? request.failure);
        return renderLogRow(badge, `${request.method} ${request.url}`);
    });
    const count = element("span", "badge failed", requests.length);
    return renderLog("Peticiones fallidas", [count], rows);
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

// Build the view of a test from its stored document, under the `title` already on screen.
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

    if (!test.error) {
        return [overview, timeline];
    }
    const failure = renderError(test.error);
    failure.classList.add(test.status);
    return [overview, failure, timeline];
}

// Build what a test shows under its `title`: its stored document or, failing that, why it is not
// there. A test that did not arrive and one that arrived but cannot be drawn are reported apart.
async function loadTest(test, title) {
    let stored;
    try {
        stored = await fetchJson(`/api/test/${test.id}`);
    } catch (error) {
        const retry = element("button", "button", "Reintentar");
        retry.type = "button";
        retry.addEventListener("click", () => showTest(test));
        const message = `No se pudo cargar el test: ${error.message}`;
        return [title, element("p", "failure", message), retry];
    }

    try {
        return renderDocument(title, stored);
    } catch (error) {
        // The message on the page is for its reader; the console keeps where it came from.
        console.error(error);
        return [title, element("p", "failure", `No se pudo mostrar el test: ${error.message}`)];
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
        element("h2", "", formatDateTime(test.started_at)),
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

// The part of an address that selects a test, so that the address of the page says which one is
// shown.
function testHash(test) {
    return `#test-${test.id}`;
}

function renderTestLink(test) {
    const link = element("a", "test-link");
    link.href = testHash(test);
    link.append(
        element("span", "", formatDateTime(test.started_at)),
        renderLabelledDot(test.status),
    );
    return link;
}

// Scroll the list of tests, and nothing but the list, just enough to bring one of them in sight.
function revealTestLink(link) {
    const list = link.parentElement;
    const bottom = link.offsetTop + link.offsetHeight;
    if (link.offsetTop < list.scrollTop) {
        list.scrollTop = link.offsetTop;
    } else if (bottom > list.scrollTop + list.clientHeight) {
        list.scrollTop = bottom - list.clientHeight;
    }
}

// Show the test that the address of the page selects, or the latest one if it selects none.
function showSelectedTest(tests) {
    const named = tests.find((test) => location.hash === testHash(test));
    // An address that names a test this game does not have is corrected to name none, which
    // is the address of the latest test.
    if (!named && location.hash) {
        history.replaceState(null, "", location.pathname);
    }
    // Tests come newest first.
    const selected = named ?? tests[0];
    for (const link of document.getElementById("tests").children) {
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

// Build what the banner of a game holds: its image, its name and, in a row under the name, what
// is known about it.
function renderBanner(game) {
    // Whether the game can be played for fun is a yes or a no, so it is a mark of its own.
    const demo = game.is_available_for_fun
        ? element("span", "chip available", "Modo demo")
        : element("span", "chip unavailable", "Sin modo demo");
    const meta = element("div", "meta");
    meta.append(element("span", "provider", game.provider));
    if (game.primary_type) {
        meta.append(element("span", "chip", game.primary_type));
    }
    meta.append(demo, element("span", "chip", `Añadido el ${formatDay(game.added_on, "UTC")}`));

    const about = element("div");
    about.append(element("h1", "", game.name), meta);
    return [renderCover(game.image_url), about];
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
                "Este juego aún no tiene tests",
                "Sus resultados aparecerán aquí en cuanto se ejecute el primero.",
            ),
        );
        article.hidden = false;
        return;
    }

    const links = game.tests.map(renderTestLink);
    document.getElementById("count").textContent = links.length;
    document.getElementById("tests").append(...links);
    document.getElementById("sidebar").hidden = false;
    window.addEventListener("hashchange", () => showSelectedTest(game.tests));
    showSelectedTest(game.tests);
}

setUpViewer();
showGame().catch((error) => showError(`No se pudo cargar el juego: ${error.message}`));
