import { serve } from "@hono/node-server";
import { Hono } from "hono";
import { greet, nowIso } from "@demo/shared";

const app = new Hono();

app.get("/", (c) => c.text(greet("turbo-agent")));
app.get("/hello/:name", (c) => c.json({ message: greet(c.req.param("name")), at: nowIso() }));
app.get("/boom", () => {
  throw new Error("intentional API error for test");
});

const port = Number(process.env.PORT ?? 8742);
serve({ fetch: app.fetch, port }, (info) => {
  console.log(`[api] listening on http://localhost:${info.port}`);
});

setInterval(() => {
  console.log(`[api] heartbeat ${nowIso()}`);
}, 2000);
