// Twyla slide deck controller. The slides are the `.slide` divs emitted by
// templates/slides.typ (one per frame). Exactly one is "active" at a time; the
// active slide number lives in the URL hash so reload and deep-links work.
//
// Shipped as a real .js file (referenced via `asset.file`), so unlike the inline
// theme script it is free to use `<`, `>`, `&` and the rest of JavaScript.
(function () {
  const deck = document.querySelector('.deck');
  if (!deck) return;

  const slides = Array.prototype.slice.call(deck.querySelectorAll('.slide'));
  if (slides.length === 0) return;

  const fill = deck.querySelector('.deck-progress-fill');
  const counter = deck.querySelector('.deck-counter');
  const prevBtn = deck.querySelector('.deck-prev');
  const nextBtn = deck.querySelector('.deck-next');

  let index = 0;

  const clamp = (i) => Math.max(0, Math.min(slides.length - 1, i));

  function indexFromHash() {
    const n = parseInt((location.hash || '').replace('#', ''), 10);
    return Number.isNaN(n) ? 0 : clamp(n - 1);
  }

  function render() {
    slides.forEach((slide, i) => slide.classList.toggle('is-active', i === index));
    if (fill) fill.style.width = ((index + 1) / slides.length) * 100 + '%';
    if (counter) counter.textContent = (index + 1) + ' / ' + slides.length;
  }

  function go(target) {
    index = clamp(target);
    const hash = '#' + (index + 1);
    if (location.hash !== hash) history.replaceState(null, '', hash);
    render();
  }

  const next = () => go(index + 1);
  const prev = () => go(index - 1);

  function toggleFullscreen() {
    if (document.fullscreenElement) {
      document.exitFullscreen();
    } else if (document.documentElement.requestFullscreen) {
      document.documentElement.requestFullscreen();
    }
  }

  document.addEventListener('keydown', (e) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    switch (e.key) {
      case 'ArrowRight':
      case 'ArrowDown':
      case 'PageDown':
      case ' ':
      case 'l':
      case 'j':
        e.preventDefault(); next(); break;
      case 'ArrowLeft':
      case 'ArrowUp':
      case 'PageUp':
      case 'h':
      case 'k':
        e.preventDefault(); prev(); break;
      case 'Home':
        e.preventDefault(); go(0); break;
      case 'End':
        e.preventDefault(); go(slides.length - 1); break;
      case 'f':
        e.preventDefault(); toggleFullscreen(); break;
      default:
        break;
    }
  });

  if (prevBtn) prevBtn.addEventListener('click', prev);
  if (nextBtn) nextBtn.addEventListener('click', next);

  // Touch swipe navigation.
  let touchX = null;
  deck.addEventListener('touchstart', (e) => { touchX = e.changedTouches[0].clientX; }, { passive: true });
  deck.addEventListener('touchend', (e) => {
    if (touchX === null) return;
    const dx = e.changedTouches[0].clientX - touchX;
    if (Math.abs(dx) > 40) (dx < 0 ? next : prev)();
    touchX = null;
  }, { passive: true });

  window.addEventListener('hashchange', () => go(indexFromHash()));

  go(indexFromHash());
})();
