// The workbench owns the preference. Observe its applied class, rather than
// creating a second preference store or following the OS independently.
(() => {
  if (window.top !== window) return;
  const observe = () => {
    const root = document.documentElement;
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
