// Chart tooltips: any element with data-tip="Title|Name: value|..." shows
// them on hover and keyboard focus. Text is inserted with textContent only.
(function () {
  'use strict';
  function tip() { return document.getElementById('tip'); }

  function show(el, x, y) {
    var t = tip();
    if (!t) return;
    var parts = (el.getAttribute('data-tip') || '').split('|');
    t.replaceChildren();
    var title = document.createElement('div');
    title.className = 't';
    title.textContent = parts[0];
    t.appendChild(title);
    parts.slice(1).forEach(function (p) {
      var i = p.indexOf(': ');
      var row = document.createElement('div');
      row.className = 'row';
      var value = document.createElement('b');
      var name = document.createElement('span');
      value.textContent = i >= 0 ? p.slice(i + 2) : p;
      name.textContent = i >= 0 ? ' ' + p.slice(0, i) : '';
      row.appendChild(value);
      row.appendChild(name);
      t.appendChild(row);
    });
    t.hidden = false;
    var w = t.offsetWidth, h = t.offsetHeight;
    var left = Math.min(window.innerWidth - w - 8, x + 14);
    var top = y - h - 12 < 0 ? y + 16 : y - h - 12;
    t.style.left = Math.max(8, left) + 'px';
    t.style.top = top + 'px';
  }

  function hide() { var t = tip(); if (t) t.hidden = true; }

  document.addEventListener('pointermove', function (e) {
    var el = e.target.closest && e.target.closest('[data-tip]');
    if (el) show(el, e.clientX, e.clientY); else hide();
  });
  document.addEventListener('focusin', function (e) {
    var el = e.target.closest && e.target.closest('[data-tip]');
    if (!el) return hide();
    var r = el.getBoundingClientRect();
    show(el, r.left + r.width / 2, r.top);
  });
  document.addEventListener('focusout', hide);
  document.addEventListener('htmx:beforeSwap', hide);
})();
