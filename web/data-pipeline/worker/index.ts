interface Env {
  ASSETS: {
    fetch(request: Request): Promise<Response>;
  };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const response = await env.ASSETS.fetch(request);
    const contentType = response.headers.get("content-type") ?? "";

    if (!contentType.includes("text/html")) return response;

    const socialImage = new URL("/og.png", request.url).href;
    const html = (await response.text()).replaceAll(
      'content="/og.png"',
      `content="${socialImage}"`,
    );
    return new Response(html, {
      status: response.status,
      statusText: response.statusText,
      headers: response.headers,
    });
  },
};
