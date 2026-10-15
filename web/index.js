// What each status of a game means, in the order the legend and the filters list them.
const STATUS_MEANINGS = {
    working: "El último test completó todos sus pasos.",
    failing: "En el último test el juego no se comportó como se esperaba.",
    test_failed: "El último test no pudo completarse, así que no dice nada sobre el juego.",
    untested: "Todavía no se ha ejecutado ningún test.",
};

function renderGame(game) {
    // A game stands where its latest test left it, whatever the earlier ones found.
    const status = game.status ?? "untested";
    const card = element("a", "card game");
    card.href = `/game/${game.id}`;
    card.dataset.status = status;

    const tested = game.tested_at ? formatDay(game.tested_at) : "Nunca";

    const facts = element("dl");
    facts.append(
        renderFact("Último test", renderLabelledDot(status), tested),
        renderFact("Añadido", formatDay(game.added_on, "UTC")),
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

// Build a button that keeps every card, followed by one per status that keeps the cards in it.
// Each shows how many cards it keeps.
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
            button.append("Todos");
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
    // A click anywhere else on the page closes the legend, and so does the Escape key.
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
            renderEmpty("Aún no hay juegos", "Aparecerán aquí en cuanto se añada el primero."),
        );
        return;
    }

    const cards = games.map(renderGame);
    document.getElementById("filters").append(...renderFilters(cards));
    container.append(...cards);
}

setUpLegend();
showGames().catch((error) => showError(`No se pudieron cargar los juegos: ${error.message}`));
