// Settings must remain operable even if a Dioxus hydration error prevents
// component event handlers from attaching. The server remains authoritative:
// read the current snapshot, write exactly one feature change, then reload.
document.addEventListener("click", async (event) => {
  const control = event.target.closest("[data-settings-toggle]");
  if (!control || control.dataset.saving === "true") return;
  event.preventDefault();
  control.dataset.saving = "true";
  try {
    const settings = await fetch("/api/settings", { credentials: "same-origin" }).then((r) => r.json());
    const path = control.dataset.settingsToggle.split(".");
    let target = settings;
    for (const key of path.slice(0, -1)) target = target[key];
    target[path.at(-1)] = control.getAttribute("aria-checked") !== "true";
    if (path.join(".") === "adapters.hqplayer") settings.hide_hqp_page = !target[path.at(-1)];
    const response = await fetch("/api/settings", { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify(settings) });
    if (!response.ok || (await response.json()).ok !== true) throw new Error("UHC could not apply that change.");
    location.reload();
  } catch (error) {
    control.dataset.saving = "false";
    alert(error instanceof Error ? error.message : "UHC could not apply that change.");
  }
});
