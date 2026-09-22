// Rendered on the server at build time so the static adapter emits a real
// settings.html. Tauri loads this window directly by path, and it has no router
// to resolve a client-side route.
export const prerender = true;
