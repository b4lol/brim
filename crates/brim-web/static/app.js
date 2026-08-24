/* Brim SPA client: live search, stats, installs and upgrades. Vanilla JS only. */

(function () {
    "use strict";

    const searchInput = document.getElementById("search");
    const results = document.getElementById("results");
    const upgradeBtn = document.getElementById("upgrade-all");
    const toasts = document.getElementById("toasts");

    // Map brim-core's serde variant names to the API's lowercase source keys
    // and to the CSS badge classes defined in style.css.
    const SOURCES = {
        FedoraOfficial: { api: "fedora", label: "Fedora", badge: "badge-fedora" },
        Copr: { api: "copr", label: "COPR", badge: "badge-copr" },
        Flatpak: { api: "flatpak", label: "Flatpak", badge: "badge-flatpak" },
    };

    // Per-session API token, delivered in the URL fragment by the server
    // (see main.rs); without it every /api/* call is rejected with 403.
    const token = new URLSearchParams(location.hash.slice(1)).get("token") || "";

    function toast(message, ok) {
        const el = document.createElement("div");
        el.className = "toast glass " + (ok ? "toast-ok" : "toast-err");
        el.textContent = message;
        toasts.appendChild(el);
        setTimeout(() => el.classList.add("fade"), 4200);
        setTimeout(() => el.remove(), 4800);
    }

    function stars(rating) {
        const pct = Math.max(0, Math.min(100, (Number(rating) / 5) * 100));
        return (
            '<span class="stars" title="Rating ' + Number(rating).toFixed(1) + ' / 5">' +
            "★★★★★" +
            '<span class="stars-fill" style="width:' + pct + '%">★★★★★</span>' +
            "</span>"
        );
    }

    function esc(text) {
        const div = document.createElement("div");
        div.textContent = text == null ? "" : String(text);
        return div.innerHTML;
    }

    function renderPackages(packages) {
        if (!packages.length) {
            results.innerHTML = '<p class="hint">No packages found.</p>';
            return;
        }
        results.innerHTML = "";
        for (const pkg of packages) {
            const src = SOURCES[pkg.source] || { api: null, label: pkg.source, badge: "" };
            const isRemove = pkg.status === "Installed";
            const label =
                pkg.status === "Installed"
                    ? "Remove"
                    : pkg.status === "UpdateAvailable"
                      ? "Update"
                      : "Install";
            const card = document.createElement("div");
            card.className = "card glass";
            card.innerHTML =
                '<div class="card-head">' +
                '<span class="card-name">' + esc(pkg.name) + "</span>" +
                '<span class="badge ' + src.badge + '">' + esc(src.label) + "</span>" +
                "</div>" +
                '<p class="card-summary">' + esc(pkg.summary || pkg.description || "No description.") + "</p>" +
                '<div class="card-meta">' +
                "<span>" + esc(pkg.version || "?") + "</span>" +
                "<span>" + esc(pkg.category || "") + "</span>" +
                (pkg.size_mb ? "<span>" + Number(pkg.size_mb).toFixed(1) + " MB</span>" : "") +
                "</div>" +
                '<div class="card-foot">' +
                stars(pkg.rating) +
                '<button class="btn ' + (isRemove ? "btn-remove" : "btn-install") + '" type="button">' +
                label +
                "</button>" +
                "</div>";

            const btn = card.querySelector(".btn");
            if (isRemove) {
                btn.addEventListener("click", () => removePkg(pkg, src, btn));
            } else {
                btn.addEventListener("click", () => install(pkg, src, btn));
            }
            results.appendChild(card);
        }
    }

    async function search(query) {
        if (!query) {
            results.innerHTML =
                '<p class="hint">Type in the search box to browse packages across all sources.</p>';
            return;
        }
        try {
            const res = await fetch("/api/packages?q=" + encodeURIComponent(query), {
                headers: { "x-brim-token": token },
            });
            if (!res.ok) throw new Error("HTTP " + res.status);
            renderPackages(await res.json());
        } catch (err) {
            toast("Search failed: " + err.message, false);
        }
    }

    // Debounced live search: wait 300 ms after the last keystroke.
    let searchTimer = null;
    searchInput.addEventListener("input", () => {
        clearTimeout(searchTimer);
        const query = searchInput.value.trim();
        searchTimer = setTimeout(() => search(query), 300);
    });

    async function refresh() {
        // Stats and installed card states changed; reload both.
        loadStats();
        search(searchInput.value.trim());
    }

    async function install(pkg, src, btn) {
        btn.disabled = true;
        btn.textContent = "Installing…";
        try {
            const res = await fetch("/api/install", {
                method: "POST",
                headers: { "Content-Type": "application/json", "x-brim-token": token },
                body: JSON.stringify({ id: pkg.id, source: src.api }),
            });
            const data = await res.json();
            if (res.ok && data.success) {
                toast("Installed " + pkg.name, true);
                btn.textContent = "Installed";
                refresh();
            } else {
                toast("Install failed: " + (data.error || data.message || res.status), false);
                btn.textContent = "Install";
                btn.disabled = false;
            }
        } catch (err) {
            toast("Install failed: " + err.message, false);
            btn.textContent = "Install";
            btn.disabled = false;
        }
    }

    async function removePkg(pkg, src, btn) {
        if (!confirm("Remove " + pkg.name + "?")) return;
        btn.disabled = true;
        btn.textContent = "Removing…";
        try {
            const res = await fetch("/api/remove", {
                method: "POST",
                headers: { "Content-Type": "application/json", "x-brim-token": token },
                body: JSON.stringify({ id: pkg.id, source: src.api }),
            });
            const data = await res.json();
            if (res.ok && data.success) {
                toast("Removed " + pkg.name, true);
                refresh();
            } else {
                toast("Remove failed: " + (data.error || data.message || res.status), false);
                btn.textContent = "Remove";
                btn.disabled = false;
            }
        } catch (err) {
            toast("Remove failed: " + err.message, false);
            btn.textContent = "Remove";
            btn.disabled = false;
        }
    }

    upgradeBtn.addEventListener("click", async () => {
        if (!confirm("Upgrade all packages on every source?")) return;
        upgradeBtn.disabled = true;
        upgradeBtn.textContent = "Upgrading…";
        try {
            const res = await fetch("/api/upgrade", {
                method: "POST",
                headers: { "x-brim-token": token },
            });
            const data = await res.json();
            toast(data.message || (data.success ? "Upgrade finished" : "Upgrade failed"),
                res.ok && data.success);
            if (res.ok && data.success) refresh();
        } catch (err) {
            toast("Upgrade failed: " + err.message, false);
        } finally {
            upgradeBtn.disabled = false;
            upgradeBtn.textContent = "Upgrade All";
        }
    });

    async function loadStats() {
        try {
            const res = await fetch("/api/stats", { headers: { "x-brim-token": token } });
            if (!res.ok) throw new Error("HTTP " + res.status);
            const stats = await res.json();
            document.getElementById("stat-installed").textContent = stats.installed;
            document.getElementById("stat-updates").textContent = stats.updates_pending;
            const box = document.getElementById("stat-sources");
            box.innerHTML = "";
            for (const s of stats.sources) {
                const src = SOURCES[s.source] || { label: s.source, badge: "" };
                const el = document.createElement("span");
                el.className = "stat-source";
                el.innerHTML =
                    '<span class="badge ' + src.badge + '">' + esc(src.label) + "</span>" +
                    esc(s.installed + " installed · " + s.updates + " updates");
                box.appendChild(el);
            }
        } catch (err) {
            toast("Could not load stats: " + err.message, false);
        }
    }

    loadStats();
})();
