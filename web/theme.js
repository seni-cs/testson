// Puts on the page the theme the reader picked with the toggle in the top bar or, without a pick,
// the one their system prefers, and follows the system while nothing is picked. Loaded in the
// head, so a page never flashes in the other theme. The block keeps its names out of the scripts
// of the pages.
{
    const root = document.documentElement;
    const system = matchMedia("(prefers-color-scheme: dark)");
    let picked = null;
    try {
        picked = localStorage.getItem("theme");
    } catch {
        // Without storage, a pick lasts until the page is left.
    }
    const follow = () => {
        root.dataset.theme = ["light", "dark"].includes(picked) ? picked : system.matches ? "dark" : "light";
    };
    follow();
    system.addEventListener("change", follow);

    document.addEventListener("DOMContentLoaded", () => {
        const toggle = document.getElementById("theme-toggle");
        toggle.ariaLabel = toggle.title = "Switch between light and dark mode";
        toggle.addEventListener("click", () => {
            picked = root.dataset.theme === "dark" ? "light" : "dark";
            follow();
            try {
                localStorage.setItem("theme", picked);
            } catch {
                // As above.
            }
        });
    });
}
