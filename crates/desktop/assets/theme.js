// The workbench owns the preference. Observe its applied class, rather than
// creating a second preference store or following the OS independently.
(() => {
  if (window.top !== window) return;
  const observe = () => {
    const root = document.documentElement;
    // Presentation hint only: the native caption already carries the app's
    // identity, so the workbench must not render a second logo/title row.
    root.setAttribute("data-ia2-desktop", "");
    let previous;
    const sync = () => {
      const theme = root.classList.contains("dark") ? "dark" : "light";
      if (theme === previous) return;
      previous = theme;
      window.ipc.postMessage(JSON.stringify({ type: "ia2-theme", theme }));
    };
    new MutationObserver(sync).observe(root, {
      attributes: true,
      attributeFilter: ["class"],
    });
    sync();
  };
  if (document.documentElement) observe();
  else document.addEventListener("DOMContentLoaded", observe, { once: true });
})();
