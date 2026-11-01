// The command palette: Cmd/Ctrl+K (or the header's search button) opens a dialog listing
// every project, environment, service and page the user can see, fetched from /palette
// each time it opens, and filters it as they type. Arrow keys move, Enter opens, Escape closes.
//
// Spinikus's own code, not vendored. The page's CSP allows only scripts from this site,
// with no eval, so this file is all there is.
(() => {
  "use strict";

  const dialog = () => document.getElementById("palette");

  function options(d) {
    return Array.from(d.querySelectorAll("#palette-results [role=option]"));
  }

  function visible(d) {
    return options(d).filter((option) => !option.parentElement.hidden);
  }

  function select(d, option) {
    for (const other of options(d)) other.removeAttribute("aria-selected");
    if (option) {
      option.setAttribute("aria-selected", "true");
      option.scrollIntoView({ block: "nearest" });
    }
  }

  function filter(d) {
    const terms = d.querySelector("#palette-input").value.toLowerCase().split(/\s+/).filter(Boolean);
    for (const option of options(d)) {
      const text = option.dataset.search || "";
      option.parentElement.hidden = !terms.every((term) => text.includes(term));
    }
    const shown = visible(d);
    d.querySelector("#palette-empty").hidden = shown.length > 0 || !d.dataset.loaded;
    select(d, shown[0]);
  }

  async function open() {
    const d = dialog();
    if (!d || d.open) return;
    const input = d.querySelector("#palette-input");
    const list = d.querySelector("#palette-results");
    input.value = "";
    delete d.dataset.loaded;
    d.querySelector("#palette-empty").hidden = true;
    d.showModal();
    input.focus();
    try {
      const response = await fetch("/palette", { credentials: "same-origin", headers: { Accept: "text/html" } });
      if (!response.ok) return;
      list.innerHTML = await response.text();
      // Boost the new links like the rest of the page.
      if (window.htmx) window.htmx.process(list);
      d.dataset.loaded = "true";
      filter(d);
    } catch {
      // Offline or signed out: the palette stays empty.
    }
  }

  function go(d, option) {
    if (!option) return;
    d.close();
    option.click();
  }

  document.addEventListener("keydown", (event) => {
    if ((event.metaKey || event.ctrlKey) && !event.altKey && event.key.toLowerCase() === "k") {
      if (!dialog()) return;
      event.preventDefault();
      open();
      return;
    }
    const d = dialog();
    if (!d || !d.open || event.target.id !== "palette-input") return;
    const shown = visible(d);
    const current = shown.findIndex((option) => option.getAttribute("aria-selected") === "true");
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      const step = event.key === "ArrowDown" ? 1 : -1;
      select(d, shown[(current + step + shown.length) % shown.length]);
    } else if (event.key === "Enter") {
      event.preventDefault();
      go(d, shown[current]);
    } else if (event.key === "Escape") {
      // A search field would only clear itself on the first Escape.
      event.preventDefault();
      d.close();
    }
  });

  document.addEventListener("input", (event) => {
    if (event.target.id === "palette-input") filter(dialog());
  });

  document.addEventListener("click", (event) => {
    const d = dialog();
    if (event.target.closest("[data-palette-open]")) {
      open();
    } else if (d && event.target === d) {
      // A click on the backdrop.
      d.close();
    } else if (d && d.open && event.target.closest("#palette-results a")) {
      d.close();
    }
  });
})();
