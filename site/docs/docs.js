// Shared behavior for docs pages: theme toggle, mobile sidebar, icon sync.
(function () {
  function getTheme() { return document.documentElement.getAttribute('data-theme') || 'dark'; }
  function syncIcon() {
    var icon = document.getElementById('themeIcon');
    if (!icon) return;
    icon.textContent = getTheme() === 'dark' ? '\u263C' : '\u263E'; // ☼ / ☾
  }
  window.toggleDocsTheme = function () {
    var next = getTheme() === 'dark' ? 'light' : 'dark';
    document.documentElement.setAttribute('data-theme', next);
    try { localStorage.setItem('atomcode-theme', next); } catch (e) {}
    syncIcon();
  };
  document.addEventListener('DOMContentLoaded', function () {
    syncIcon();
    var toggle = document.getElementById('sidebarToggle');
    var sidebar = document.getElementById('docsSidebar');
    if (toggle && sidebar) {
      toggle.addEventListener('click', function () { sidebar.classList.toggle('open'); });
    }
  });
})();
