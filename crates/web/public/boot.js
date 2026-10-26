// Dynamic import keeps a missing JS/WASM asset recoverable instead of hiding SSR.
async function boot() {
  try {
    const { default: init, hydrate } = await import('/assets/app.js');
    await init();
    // Also supports an entry point that hydrates in #[wasm_bindgen(start)].
    if (typeof hydrate === 'function') hydrate();
    document.documentElement.dataset.hydrated = 'true';
  } catch (_error) {
    const banner = document.createElement('section');
    banner.className = 'boot-error';
    banner.setAttribute('role', 'alert');
    banner.setAttribute('data-testid', 'boot-error');
    banner.textContent = 'The interactive planner could not start. Your saved plan has not been changed. Reload to try again; sign-in and sandbox links remain available below.';
    const retry = document.createElement('a');
    retry.href = window.location.href;
    retry.textContent = 'Reload page';
    banner.append(retry);
    document.body.prepend(banner);
    document.documentElement.dataset.hydrated = 'failed';
  }
}

void boot();
