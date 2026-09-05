#!/usr/bin/env node
/**
 * Drive Qoder (1.28.0+) over the Chrome DevTools Protocol to exercise a
 * custom-route E2E regression:
 *
 *   1. open the model selector, switch to the "自定义" tab
 *   2. select the injected BYOK carrier
 *   3. type a message and send it
 *
 * Requires Qoder started with --remote-debugging-port=9222 and the Q Switch
 * adapter already owning .info.json (so the request goes to the local
 * gateway instead of Qoder's cloud).
 */
import http from "node:http";

const CDP_PORT = process.env.QODER_CDP_PORT || "9222";
const CARRIER_NAME = process.env.QODER_E2E_CARRIER_NAME || "E2E-Carrier";
const MESSAGE = process.env.QODER_E2E_MESSAGE || "回复两个字：OK";
const FORMAT = process.env.QSWITCH_E2E_API_FORMAT || "openai_chat";
const EXPECTED_REPLY = {
  openai_chat: "MOCK-CHAT",
  anthropic_messages: "MOCK-ANTHROPIC",
  openai_responses: "MOCK-RESPONSES",
}[FORMAT];

function getJSON(url) {
  return new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let d = "";
      res.on("data", (c) => (d += c));
      res.on("end", () => resolve(JSON.parse(d)));
    }).on("error", reject);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  const targets = await getJSON(`http://127.0.0.1:${CDP_PORT}/json`);
  const target = targets.find((t) => t.url.includes("agents-window"));
  if (!target) throw new Error("agents-window target not found; is Qoder running with CDP?");

  const ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((res, rej) => { ws.onopen = res; ws.onerror = rej; });
  let id = 0;
  const pending = new Map();
  ws.onmessage = (ev) => {
    const msg = JSON.parse(ev.data);
    if (msg.id && pending.has(msg.id)) { pending.get(msg.id)(msg); pending.delete(msg.id); }
  };
  const send = (method, params = {}) => new Promise((resolve) => {
    const mid = ++id;
    pending.set(mid, resolve);
    ws.send(JSON.stringify({ id: mid, method, params }));
  });
  const click = async (x, y) => {
    await send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
    await send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
  };
  const evl = async (expression) => {
    const r = await send("Runtime.evaluate", { expression, returnByValue: true });
    return r.result && r.result.result ? r.result.result.value : JSON.stringify(r);
  };

  // open the model selector with a real input event (DOM click does not
  // always reach the React handler in 1.28.0)
  const pos = JSON.parse(await evl(`(function(){var el=document.querySelector('.footer-model-selector, .footer-model-selector-wrapper'); if(!el) return '{}'; var r=el.getBoundingClientRect(); return JSON.stringify({x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)});})()`));
  await click(pos.x || 625, pos.y || 398);
  await sleep(2500);

  await evl(`(function(){var el=[...document.querySelectorAll('.model-selector-tab')].find(e=>(e.innerText||'').trim()==='自定义'); if(el) el.click(); return 'ok';})()`);
  await sleep(2000);

  let sel = "MISSING";
  for (let attempt = 0; attempt < 40 && sel === "MISSING"; attempt += 1) {
    sel = await evl(`(function(){var tab=[...document.querySelectorAll('.model-selector-tab')].find(e=>(e.innerText||'').trim()==='自定义'); if(tab) tab.click(); var el=[...document.querySelectorAll('.model-selector-model-item')].find(e=>(e.innerText||'').includes(${JSON.stringify(CARRIER_NAME)})); if(!el) return 'MISSING'; el.click(); return 'selected';})()`);
    if (sel === "MISSING") await sleep(500);
  }
  if (sel === "MISSING") throw new Error(`carrier ${CARRIER_NAME} not found in model selector`);
  await sleep(3000);

  const inputPos = JSON.parse(await evl(`(function(){var w=document.querySelector('.agentchat-input-wrapper'); var ed=w.querySelector('[contenteditable=true], textarea, input'); if(!ed) return '{}'; var r=ed.getBoundingClientRect(); return JSON.stringify({x:Math.round(r.x+r.width/2),y:Math.round(r.y+r.height/2)});})()`));
  await click(inputPos.x || 856, inputPos.y || 344);
  await sleep(400);
  await send("Input.insertText", { text: MESSAGE });
  await sleep(400);
  await send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 36 });
  await send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, nativeVirtualKeyCode: 36 });
  console.log("message sent; waiting for upstream reply...");
  await sleep(12000);

  const body = await evl("document.body.innerText");
  const replyVisible = body.includes(EXPECTED_REPLY);
  console.log("REPLY_VISIBLE:", replyVisible, EXPECTED_REPLY);
  ws.close();
  if (!replyVisible) {
    throw new Error(`expected mock reply ${EXPECTED_REPLY} was not visible in Qoder`);
  }
}

main().catch((e) => { console.error("drive failed:", e.message); process.exit(1); });
