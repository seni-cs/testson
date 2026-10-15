const STATUS_MEANINGS = {
    working: "The latest real-money and no-balance tests completed every step.",
    failing: "In one of the latest tests the game did not behave as expected.",
    test_failed: "One of the latest tests could not complete, so it says nothing about the game.",
    untested: "One of the two tests is still missing.",
};

function renderGame(game) {
    const card = element("a", "card game");
    card.href = `/game/${game.id}`;
    card.dataset.status = gameStatus(game);
    card.dataset.provider = game.provider;

    const facts = element("dl");
    facts.append(
        ...MODES.map((mode) => {
            const latest = game[mode];
            const status = latest?.status ?? "untested";
            const tested = latest ? formatDay(latest.started_at) : "Never";
            const value = [renderLabelledDot(status), tested];
            if (latest) {
                value.push(renderRunner(latest.runner));
            }
            return renderFact(MODE_LABELS[mode], ...value);
        }),
    );
    const about = element("div", "about");
    about.append(element("h2", "", game.name), facts);
    card.append(renderCover(game.image_url), about);
    return card;
}

function renderLegend() {
    return Object.entries(STATUS_MEANINGS).map(([status, meaning]) => {
        const text = element("div");
        text.append(element("strong", "", STATUS_LABELS[status]), element("span", "muted", meaning));
        const item = element("li");
        item.append(renderDot(status), text);
        return item;
    });
}

// What the page shows: the games of the providers chosen, or of all of them when none is, and of
// one status, or of all. Each filter keeps its choice while the other changes.
const shown = { status: null, providers: new Set() };

function providerShown(card) {
    return shown.providers.size === 0 || shown.providers.has(card.dataset.provider);
}

function statusShown(card, status = shown.status) {
    return status === null || card.dataset.status === status;
}

function renderFilters(update) {
    const filters = [null, ...Object.keys(STATUS_MEANINGS)];
    return filters.map((status) => {
        const button = element("button", "filter");
        button.type = "button";
        button.dataset.status = status ?? "";
        if (status === null) {
            button.append("All");
        } else {
            const label = element("span", "label", STATUS_LABELS[status]);
            button.append(renderDot(status), label);
        }
        button.append(element("span", "count"));
        button.addEventListener("click", () => {
            shown.status = status;
            update();
        });
        return button;
    });
}

// Sets which filter is pressed, what each counts among the games of the providers shown, and
// which cards show.
function updateFilters(buttons, cards) {
    const ofProviders = cards.filter(providerShown);
    for (const button of buttons) {
        const status = button.dataset.status || null;
        const count = ofProviders.filter((card) => statusShown(card, status)).length;
        button.querySelector(".count").textContent = count;
        button.ariaPressed = String(status === shown.status);
        // The pressed filter stays enabled, so that it can be seen to hold no game.
        button.disabled = count === 0 && status !== shown.status;
    }
    for (const card of cards) {
        card.hidden = !(providerShown(card) && statusShown(card));
    }
}

function providerLabel(provider) {
    return provider || "Unknown";
}

function renderProviders(games, update) {
    const counts = new Map();
    for (const game of games) {
        counts.set(game.provider, (counts.get(game.provider) ?? 0) + 1);
    }
    const providers = [...counts.keys()].sort((one, other) =>
        providerLabel(one).localeCompare(providerLabel(other)),
    );
    const boxes = providers.map((provider) => {
        const box = element("input");
        box.type = "checkbox";
        box.value = provider;
        box.addEventListener("change", () => {
            if (box.checked) {
                shown.providers.add(provider);
            } else {
                shown.providers.delete(provider);
            }
            update();
        });
        const label = element("label");
        label.append(box, element("span", "", providerLabel(provider)));
        label.append(element("span", "count", counts.get(provider)));
        const item = element("li");
        item.append(label);
        return item;
    });
    document.getElementById("providers-list").append(...boxes);
    document.getElementById("providers-all").addEventListener("click", () => {
        shown.providers.clear();
        for (const box of document.querySelectorAll("#providers-list input")) {
            box.checked = false;
        }
        update();
    });
    document.getElementById("providers").hidden = false;
}

function updateProviders() {
    const chosen = [...shown.providers].map(providerLabel).sort();
    let text = "All";
    if (chosen.length === 1) {
        text = chosen[0];
    } else if (chosen.length > 1) {
        text = `${chosen.length} chosen`;
    }
    document.getElementById("providers-chosen").textContent = text;
    document.getElementById("providers-all").disabled = chosen.length === 0;
}

// Closes a menu that hangs from a button when the click or the Escape key is elsewhere.
function closeOutside(menu) {
    document.addEventListener("click", (event) => {
        if (!menu.contains(event.target)) {
            menu.open = false;
        }
    });
    document.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
            menu.open = false;
        }
    });
}

function setUpLegend() {
    document.getElementById("legend").append(...renderLegend());
    closeOutside(document.querySelector(".legend"));
    closeOutside(document.getElementById("providers"));
}

async function showGames() {
    const games = await fetchJson("/api/games");
    const container = document.getElementById("games");
    if (games.length === 0) {
        container.append(
            renderEmpty("No games yet", "They will show here as soon as the first one is added."),
        );
        return;
    }

    const cards = games.map(renderGame);
    const buttons = renderFilters(() => update());
    const update = () => {
        updateProviders();
        updateFilters(buttons, cards);
    };
    document.getElementById("filters").append(...buttons);
    renderProviders(games, update);
    update();
    container.append(...cards);
}

setUpLegend();
showGames().catch((error) => showError(`Could not load the games: ${error.message}`));
