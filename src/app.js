// Muse Windows app shell.
// Uses the Tauri v2 global API (window.__TAURI__), enabled by
// "withGlobalTauri": true in tauri.conf.json — no bundler, no npm step.
// Plain JS on purpose: the shell is a thin UI over the Rust backend.

(function () {
  "use strict";

  var tauri = window.__TAURI__ || null;
  var invoke = tauri ? tauri.core.invoke : null;
  var listen = tauri ? tauri.event.listen : null;

  var scrollback = document.getElementById("terminal-scrollback");
  var input = document.getElementById("terminal-input");
  var museView = document.getElementById("muse-view");
  var history = [];
  var historyIndex = -1;

  function appendLine(text, cssClass) {
    var div = document.createElement("div");
    div.className = cssClass || "line-stdout";
    div.textContent = text;
    scrollback.appendChild(div);
    // Keep the scrollback bounded (last 500 lines) so a chatty command
    // cannot grow the DOM without limit.
    while (scrollback.children.length > 500) {
      scrollback.removeChild(scrollback.firstChild);
    }
    scrollback.scrollTop = scrollback.scrollHeight;
  }

  // --- Muse view rectangle: tell Rust where to put the child WebView. ---
  function sendMuseRect() {
    if (!invoke) return;
    var rect = museView.getBoundingClientRect();
    invoke("set_muse_view_rect", {
      x: rect.left,
      y: rect.top,
      width: rect.width,
      height: rect.height,
    }).catch(function (err) {
      console.error("set_muse_view_rect failed:", err);
    });
  }

  // --- Agents pane (live crew feed — see README "Sub-agent pane"). ---
  // Initial load comes from get_agents; afterwards the Rust backend emits
  // a crew-update event whenever a `crew`-room post changes the pane.
  function renderAgents(agents) {
    var list = document.getElementById("agent-list");
    list.innerHTML = "";
    agents.forEach(function (agent) {
      var li = document.createElement("li");
      if (agent.working) {
        li.className = "working";
      } else if (agent.status === "done") {
        li.className = "done";
      } else if (agent.status === "failed") {
        li.className = "failed";
      }
      if (agent.working) {
        var spinner = document.createElement("span");
        spinner.className = "spinner";
        li.appendChild(spinner);
      }
      var name = document.createElement("span");
      name.className = "agent-name";
      name.textContent = agent.name;
      var status = document.createElement("span");
      status.className = "agent-status";
      status.textContent = agent.status;
      li.appendChild(name);
      li.appendChild(status);
      if (agent.task) {
        var task = document.createElement("span");
        task.className = "agent-task";
        task.textContent = agent.task;
        li.appendChild(task);
      }
      list.appendChild(li);
    });
  }

  function loadAgents() {
    if (!invoke) return;
    invoke("get_agents")
      .then(renderAgents)
      .catch(function (err) {
        console.error("get_agents failed:", err);
      });
  }

  // --- Zoom control (the reason this app exists — 130% without the browser). ---
  var slider = document.getElementById("zoom-slider");
  var zoomLabel = document.getElementById("zoom-label");

  function applyZoom(zoom) {
    // zoom is a factor (1.0 = 100%); the slider is a percentage.
    var pct = Math.round(zoom * 100);
    slider.value = String(pct);
    zoomLabel.textContent = pct + "%";
    document.getElementById("zoom-reset").textContent = pct + "%";
  }

  function setZoom(zoom) {
    if (!invoke) return;
    invoke("set_zoom", { zoom: zoom })
      .then(applyZoom)
      .catch(function (err) {
        console.error("set_zoom failed:", err);
      });
  }

  function currentZoom() {
    return parseInt(slider.value, 10) / 100;
  }

  document.getElementById("zoom-out").addEventListener("click", function () {
    setZoom(currentZoom() - 0.1);
  });
  document.getElementById("zoom-in").addEventListener("click", function () {
    setZoom(currentZoom() + 0.1);
  });
  document.getElementById("zoom-reset").addEventListener("click", function () {
    setZoom(1.0);
  });
  slider.addEventListener("change", function () {
    setZoom(currentZoom());
  });

  // --- Hub status. ---
  function renderHubStatus(status) {
    var dot = document.getElementById("hub-dot");
    var text = document.getElementById("hub-text");
    if (status.configured) {
      dot.className = "dot on";
      text.textContent = status.last_error
        ? "hub: " + status.last_error
        : "hub: watching command room";
    } else {
      dot.className = "dot off";
      text.textContent = status.last_error
        ? "hub: " + status.last_error
        : "hub: not configured";
    }
  }

  function refreshHubStatus() {
    if (!invoke) return;
    invoke("get_hub_status")
      .then(renderHubStatus)
      .catch(function (err) {
        console.error("get_hub_status failed:", err);
      });
  }

  // --- Terminal. ---
  function runHumanCommand(command) {
    appendLine("PS> " + command, "line-cmd");
    if (!invoke) return;
    invoke("run_terminal_command", { command: command, source: "human" }).catch(
      function (err) {
        appendLine("error: " + err, "line-stderr");
      }
    );
  }

  input.addEventListener("keydown", function (event) {
    if (event.key === "Enter") {
      var command = input.value.trim();
      if (command) {
        history.push(command);
        historyIndex = history.length;
        runHumanCommand(command);
      }
      input.value = "";
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (history.length > 0 && historyIndex > 0) {
        historyIndex -= 1;
        input.value = history[historyIndex];
      }
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      if (historyIndex < history.length - 1) {
        historyIndex += 1;
        input.value = history[historyIndex];
      } else {
        historyIndex = history.length;
        input.value = "";
      }
    }
  });

  // Terminal output from the Rust backend (human AND agent commands).
  if (listen) {
    listen("terminal-output", function (event) {
      var payload = event.payload;
      var cssClass = payload.stream === "stderr" ? "line-stderr" : "line-stdout";
      if (payload.source === "agent") cssClass = "line-agent";
      appendLine(payload.line, cssClass);
    });
    listen("terminal-exit", function (event) {
      var code = event.payload.code;
      var who = event.payload.source === "agent" ? "[agent] " : "";
      appendLine(
        who + "(exit" + (code === null ? " — killed" : " " + code) + ")",
        "line-stderr"
      );
    });
    listen("hub-status", function (event) {
      renderHubStatus(event.payload);
    });
    listen("crew-update", function (event) {
      renderAgents(event.payload);
    });
    listen("app-ready", function () {
      sendMuseRect();
    });
  }

  // --- Startup. ---
  if (!tauri) {
    appendLine(
      "Tauri API not available (withGlobalTauri is off?) — the shell will not work.",
      "line-stderr"
    );
  } else {
    loadAgents();
    invoke("get_zoom").then(applyZoom).catch(function () {});
    refreshHubStatus();
    setInterval(refreshHubStatus, 10000);
    // Measure the muse view area once the layout settles, then on every
    // layout change (window resize, terminal resize, etc.).
    sendMuseRect();
    if (window.ResizeObserver) {
      new ResizeObserver(function () {
        sendMuseRect();
      }).observe(museView);
    }
    window.addEventListener("resize", sendMuseRect);
    appendLine("Muse Windows app ready. Type a command below, or watch agent commands run here.", "line-stdout");
  }
})();
