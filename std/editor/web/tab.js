// The game running IN this page: brood's interpreter built for wasm (`brood-tab`), the
// game's release bundle mounted in it, and `app.js` painting what it posts. No server
// beyond the static files: a build writes everything this loads beside it (b2d's
// `web/build-tab.sh` is one).
//
// The runtime is cooperative: nothing runs unless the page pumps it. A pump runs what is
// runnable and the receive timeouts that are due — a game's frame deadline among them —
// and says when the next one is; the page pumps again then, and after every input.

import init, * as brood from "./pkg/brood_tab.js";

const status = document.getElementById("status");

function fail(message) {
  status.textContent = message;
  console.error(message);
}

try {
  await init();
  const response = await fetch("game.bundle");
  if (!response.ok) throw new Error(`game.bundle: ${response.status}`);
  brood.mount(new Uint8Array(await response.arrayBuffer()));
} catch (error) {
  fail(`could not load the game: ${error}`);
  throw error;
}

// The most one pump may hold the page: a game slower than its frame rate still leaves
// the browser time to paint and take input between pumps.
const PUMP_BUDGET_MS = 12;

let dispatch = null;
let timer = null;

// Pump, hand the posted events to the painter, and come back when the next timeout is due.
function run() {
  clearTimeout(timer);
  const next = brood.pump(PUMP_BUDGET_MS);
  const events = brood.events();
  if (events) {
    const parts = events.split("\0");
    for (let i = 0; i + 1 < parts.length; i += 2) dispatch(parts[i], parts[i + 1]);
  }
  const ended = brood.ended();
  if (ended !== undefined) {
    if (ended) fail(`the game stopped: ${ended}`);
    return;
  }
  // out of budget with work left: carry on once the browser has painted
  if (next === 0) requestAnimationFrame(run);
  else if (next > 0) timer = setTimeout(run, next);
}

window.BROOD_TAB = {
  connect(handler) {
    dispatch = handler;
    brood.start(["--tab"]);
    run();
  },
  input(form) {
    brood.input(form);
    run();
  },
};

const painter = document.createElement("script");
painter.src = "app.js";
document.body.appendChild(painter);
