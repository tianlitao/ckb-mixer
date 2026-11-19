// Pages Functions, advanced mode: every request to this site arrives here.
//
// `/aggregator/*` is forwarded to the aggregator Worker, so the page talks to
// its own origin. That is what keeps the Worker's account subdomain out of
// what a visitor sees: the URL bar, the activity log and the network panel all
// show this site's host. Everything else is handed straight to the static
// assets.
//
// Advanced mode means `_routes.json` does not apply: static asset requests pass
// through this function too (one Pages Function invocation each, 100k/day on
// the free plan). The asset directory is small and long-lived in the cache.

const AGGREGATOR = "https://ckb-mixer-aggregator.tianlitao3399.workers.dev";

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (!url.pathname.startsWith("/aggregator/")) return env.ASSETS.fetch(request);

    const target = new URL(url.pathname.slice("/aggregator".length) + url.search, AGGREGATOR);
    const headers = new Headers(request.headers);
    // The Worker builds its own response; the browser's origin is not
    // something it should be told about.
    headers.delete("host");
    headers.delete("origin");
    const body = request.method === "GET" || request.method === "HEAD" ? undefined : request.body;
    const response = await fetch(new Request(target, { method: request.method, headers, body }));
    return new Response(response.body, { status: response.status, headers: response.headers });
  },
};
