;(function () {
  'use strict'
  // Turn every `<div class="asciinema" data-cast="…">` on the page into a
  // player. The 207 KB player script is loaded lazily — only on pages that
  // actually embed a cast — so the rest of the docs pay nothing for it. The
  // player CSS is small and ships in the <head> (no flash of unstyled player).
  var casts = document.querySelectorAll('.asciinema[data-cast]')
  if (!casts.length) return

  var script = document.createElement('script')
  script.src = (window.uiRootPath || '.') + '/js/vendor/asciinema-player.min.js'
  script.onload = function () {
    Array.prototype.forEach.call(casts, function (el) {
      window.AsciinemaPlayer.create(el.dataset.cast, el, {
        cols: parseInt(el.dataset.cols, 10) || 90,
        rows: parseInt(el.dataset.rows, 10) || 28,
        autoPlay: false,
        poster: 'npt:0:2',
        fit: 'width',
        theme: 'asciinema',
        idleTimeLimit: 2,
        terminalFontFamily: "'IBM Plex Mono', 'DejaVu Sans Mono', monospace"
      })
    })
  }
  document.head.appendChild(script)
})()
