const port = Number(Bun.env.PORT ?? 3000);
Bun.serve({ port, hostname: "127.0.0.1", fetch: () => new Response("ciao bun example\n") });
console.log(`listening on ${port}`);
