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

function renderFilters(cards) {
    const filters = [null, ...Object.keys(STATUS_MEANINGS)];
    const buttons = filters.map((status) => {
        const keeps = (card) => status === null || card.dataset.status === status;
        const count = cards.filter(keeps).length;

        const button = element("button", "filter");
        button.type = "button";
        button.disabled = count === 0;
        // The page opens with every card in sight, which is what the first button stands for.
        button.ariaPressed = String(status === null);
        if (status === null) {
            button.append("All");
        } else {
            const label = element("span", "label", STATUS_LABELS[status]);
            button.append(renderDot(status), label);
        }
        button.append(element("span", "count", count));
        button.addEventListener("click", () => {
            for (const other of buttons) {
                other.ariaPressed = String(other === button);
            }
            for (const card of cards) {
                card.hidden = !keeps(card);
            }
        });
        return button;
    });
    return buttons;
}

function setUpLegend() {
    const legend = document.querySelector(".legend");
    document.getElementById("legend").append(...renderLegend());
    document.addEventListener("click", (event) => {
        if (!legend.contains(event.target)) {
            legend.open = false;
        }
    });
    document.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
            legend.open = false;
        }
    });
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
    document.getElementById("filters").append(...renderFilters(cards));
    container.append(...cards);
}

setUpLegend();
showGames().catch((error) => showError(`Could not load the games: ${error.message}`));
