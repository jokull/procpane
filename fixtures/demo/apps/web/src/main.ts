import { greet } from "@demo/shared";

const g = document.getElementById("g")!;
g.textContent = greet("browser");

const log = document.getElementById("log")!;
async function tick() {
  try {
    const r = await fetch("http://localhost:8742/hello/web");
    const j = await r.json();
    log.textContent = `${new Date().toISOString()} ${JSON.stringify(j)}\n` + log.textContent;
    console.log("[web] fetched", j);
  } catch (e) {
    console.error("[web] fetch failed", e);
  }
}
setInterval(tick, 3000);
tick();
