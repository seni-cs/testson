// The latest tests of each game, oldest first, grouped under their game as the server sends them.
function groupByGame(tests) {
    const games = new Map();
    for (const test of tests) {
        if (!games.has(test.game_id)) {
            games.set(test.game_id, { id: test.game_id, name: test.name, tests: [] });
        }
        games.get(test.game_id).tests.push(test);
    }
    return [...games.values()];
}

// The share of the tests that reached a verdict on the game and found it working, or null when
// none did. A broken test says nothing about the game, so it does not count.
function uptime(tests) {
    const verdicts = tests.filter((test) => test.status !== "test_failed");
    if (verdicts.length === 0) {
        return "–";
    }
    const share = verdicts.filter((test) => test.status === "working").length / verdicts.length;
    // Two decimals tell 99.99 % from 100 %, which one decimal would round together.
    return `${(share * 100).toLocaleString(LOCALE, { maximumFractionDigits: 2 })} %`;
}

// A game stands, as on the list of games, where the latest test of each mode left it.
function statusOf(game) {
    const latest = Object.fromEntries(game.tests.map((test) => [test.mode, test]));
    return gameStatus(latest);
}

function renderStat(label, value) {
    const tile = element("div", "card stat");
    tile.append(element("span", "label", label), element("strong", "value", value));
    return tile;
}

// A row of bars, one per test, each linking to the test. A game with fewer tests than the
// longest row starts with empty bars, so the latest tests of every game line up on the right.
function renderGameRow(game, columns) {
    const name = element("a", "row-name", game.name);
    name.href = `/game/${game.id}`;
    const bars = element("div", "bars");
    for (let index = game.tests.length; index < columns; index++) {
        bars.append(element("span", "bar untested"));
    }
    for (const test of game.tests) {
        const bar = element("a", `bar ${test.status}`);
        bar.href = `/game/${game.id}#test-${test.id}`;
        bar.title = `${formatDay(test.started_at)} · ${MODE_LABELS[test.mode]} · ${STATUS_LABELS[test.status]}`;
        bar.ariaLabel = bar.title;
        bars.append(bar);
    }
    const row = element("li", "game-row");
    row.append(name, bars, element("span", "row-uptime", uptime(game.tests)));
    return row;
}

async function showStats() {
    const games = groupByGame(await fetchJson("/api/stats"));
    if (games.length === 0) {
        document.getElementById("overall").append(renderEmpty("No tests yet", "Stats show here once games are tested."));
        return;
    }

    const failing = games.filter((game) => statusOf(game) === "failing").length;
    const status = failing === 0 ? "working" : "failing";
    const banner = element("div", `banner ${status}`);
    banner.append(
        renderDot(status),
        element("strong", "", failing === 0 ? "All games are working" : `${failing} of ${games.length} games are failing`),
    );
    const stats = element("div", "stats");
    stats.append(
        renderStat("Uptime", uptime(games.flatMap((game) => game.tests))),
        renderStat("Games", games.length),
        renderStat("Failing", failing),
    );
    document.getElementById("overall").append(banner, stats);

    document.getElementById("legend").append(
        ...SEVERITY.map((status) => {
            const item = element("li");
            item.append(renderDot(status), STATUS_LABELS[status]);
            return item;
        }),
    );
    const columns = Math.max(...games.map((game) => game.tests.length));
    document.getElementById("games").append(...games.map((game) => renderGameRow(game, columns)));
    document.getElementById("board").hidden = false;
}

showStats().catch((error) => showError(`Could not load the stats: ${error.message}`));
