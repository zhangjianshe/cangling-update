(function (global) {
  "use strict";

  function indentOf(line) {
    return (line.match(/^\s*/) || [""])[0].replace(/\t/g, "  ").length;
  }

  function cleanValue(value) {
    return String(value || "").trim().replace(/^['"]/, "").replace(/['"]$/, "");
  }

  function inlineList(value) {
    const text = cleanValue(value);
    if (!text.startsWith("[") || !text.endsWith("]")) return [];
    return text.slice(1, -1).split(",").map(cleanValue).filter(Boolean);
  }

  function parse(yaml) {
    const model = { services: [], networks: [], volumes: [] };
    const serviceMap = new Map();
    let section = "";
    let service = null;
    let nested = "";

    for (const raw of String(yaml || "").split(/\r?\n/)) {
      const noComment = raw.replace(/\s+#.*$/, "");
      if (!noComment.trim()) continue;
      const indent = indentOf(noComment);
      const text = noComment.trim();
      if (indent === 0 && /^[\w.-]+:\s*$/.test(text)) {
        section = text.slice(0, -1);
        service = null;
        nested = "";
        continue;
      }
      if (section === "services" && indent === 2 && /^[^:]+:\s*$/.test(text)) {
        const name = cleanValue(text.slice(0, -1));
        service = { name, image: "", container: "", depends: [], networks: [], volumes: [], ports: [] };
        model.services.push(service);
        serviceMap.set(name, service);
        nested = "";
        continue;
      }
      if (section === "services" && service) {
        if (indent === 4) {
          const split = text.indexOf(":");
          if (split < 0) continue;
          const key = text.slice(0, split).trim();
          const value = cleanValue(text.slice(split + 1));
          nested = key;
          if (key === "image") service.image = value;
          else if (key === "container_name") service.container = value;
          else if (key === "depends_on") service.depends.push(...inlineList(value));
          else if (key === "networks") service.networks.push(...inlineList(value));
          else if (key === "volumes") service.volumes.push(...inlineList(value));
          else if (key === "ports") service.ports.push(...inlineList(value));
          continue;
        }
        if (indent >= 6 && text.startsWith("- ")) {
          const value = cleanValue(text.slice(2));
          if (nested === "depends_on") service.depends.push(value);
          else if (nested === "networks") service.networks.push(value);
          else if (nested === "volumes") service.volumes.push(value);
          else if (nested === "ports") service.ports.push(value);
          continue;
        }
        if (indent === 6 && (nested === "depends_on" || nested === "networks")) {
          const key = cleanValue(text.split(":", 1)[0]);
          if (key) service[nested === "depends_on" ? "depends" : "networks"].push(key);
        }
      } else if ((section === "networks" || section === "volumes") && indent === 2) {
        const name = cleanValue(text.split(":", 1)[0]);
        if (name) model[section].push(name);
      }
    }

    for (const item of model.services) {
      item.depends = [...new Set(item.depends.filter(name => serviceMap.has(name)))];
      item.networks = [...new Set(item.networks.map(value => value.split(":")[0]).filter(Boolean))];
      item.volumes = [...new Set(item.volumes.filter(Boolean))];
      item.ports = [...new Set(item.ports.filter(Boolean))];
    }
    model.networks = [...new Set(model.networks)];
    model.volumes = [...new Set(model.volumes)];
    return model;
  }

  function css(name, fallback) {
    const value = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return value || fallback;
  }

  function roundedRect(ctx, x, y, w, h, r) {
    const radius = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.roundRect(x, y, w, h, radius);
  }

  function clipText(ctx, text, maxWidth) {
    let value = String(text || "");
    if (ctx.measureText(value).width <= maxWidth) return value;
    while (value.length && ctx.measureText(value + "…").width > maxWidth) value = value.slice(0, -1);
    return value + "…";
  }

  function drawArrow(ctx, from, to, color) {
    const x1 = from.x + from.w / 2;
    const y1 = from.y + from.h;
    const x2 = to.x + to.w / 2;
    const y2 = to.y;
    const mid = y1 + (y2 - y1) / 2;
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.moveTo(x1, y1);
    ctx.bezierCurveTo(x1, mid, x2, mid, x2, y2 - 7);
    ctx.stroke();
    ctx.fillStyle = color;
    ctx.beginPath();
    ctx.moveTo(x2, y2);
    ctx.lineTo(x2 - 4, y2 - 8);
    ctx.lineTo(x2 + 4, y2 - 8);
    ctx.closePath();
    ctx.fill();
  }

  function render(canvas, yaml) {
    const model = parse(yaml);
    const parentWidth = Math.max(680, canvas.parentElement ? canvas.parentElement.clientWidth : 0);
    const cardW = 250;
    const cardH = 142;
    const gapX = 36;
    const gapY = 72;
    const cols = Math.max(1, Math.min(4, Math.floor((parentWidth - 48) / (cardW + gapX))));
    const rows = Math.max(1, Math.ceil(model.services.length / cols));
    const logicalW = Math.max(parentWidth, 48 + cols * cardW + (cols - 1) * gapX);
    const logicalH = 70 + rows * cardH + Math.max(0, rows - 1) * gapY + 120;
    const ratio = Math.max(1, global.devicePixelRatio || 1);
    canvas.width = logicalW * ratio;
    canvas.height = logicalH * ratio;
    canvas.style.width = logicalW + "px";
    canvas.style.height = logicalH + "px";
    const ctx = canvas.getContext("2d");
    ctx.scale(ratio, ratio);

    const bg = css("--bg-2", "#161b22");
    const card = css("--bg-3", "#21262d");
    const text = css("--text", "#f0f6fc");
    const muted = css("--muted", "#8b949e");
    const line = css("--line", "#30363d");
    const accent = css("--accent", "#58a6ff");
    ctx.fillStyle = bg;
    ctx.fillRect(0, 0, logicalW, logicalH);

    ctx.fillStyle = text;
    ctx.font = "600 16px system-ui, sans-serif";
    ctx.fillText("Docker Compose 服务拓扑", 24, 30);
    ctx.fillStyle = muted;
    ctx.font = "12px system-ui, sans-serif";
    ctx.fillText(model.services.length + " 个服务 · 箭头表示 depends_on", 24, 50);

    const boxes = new Map();
    model.services.forEach((service, index) => {
      const col = index % cols;
      const row = Math.floor(index / cols);
      boxes.set(service.name, {
        x: 24 + col * (cardW + gapX),
        y: 68 + row * (cardH + gapY),
        w: cardW,
        h: cardH,
      });
    });
    for (const service of model.services) {
      const to = boxes.get(service.name);
      for (const dependency of service.depends) {
        const from = boxes.get(dependency);
        if (from && to) drawArrow(ctx, from, to, accent);
      }
    }

    model.services.forEach(service => {
      const box = boxes.get(service.name);
      roundedRect(ctx, box.x, box.y, box.w, box.h, 8);
      ctx.fillStyle = card;
      ctx.fill();
      ctx.strokeStyle = line;
      ctx.lineWidth = 1;
      ctx.stroke();
      ctx.fillStyle = accent;
      ctx.fillRect(box.x, box.y, 4, box.h);
      ctx.fillStyle = text;
      ctx.font = "600 15px system-ui, sans-serif";
      ctx.fillText(clipText(ctx, service.name, cardW - 28), box.x + 16, box.y + 25);
      ctx.fillStyle = muted;
      ctx.font = "12px ui-monospace, monospace";
      const details = [
        ["镜像", service.image || "—"],
        ["端口", service.ports.join(", ") || "—"],
        ["网络", service.networks.join(", ") || "default"],
        ["挂载", service.volumes.length ? service.volumes.length + " 项" : "—"],
      ];
      details.forEach((item, i) => {
        ctx.fillStyle = muted;
        ctx.fillText(item[0], box.x + 16, box.y + 51 + i * 21);
        ctx.fillStyle = text;
        ctx.fillText(clipText(ctx, item[1], cardW - 70), box.x + 58, box.y + 51 + i * 21);
      });
    });

    const footerY = logicalH - 68;
    ctx.fillStyle = muted;
    ctx.font = "12px system-ui, sans-serif";
    ctx.fillText("网络: " + (model.networks.join(", ") || "default"), 24, footerY);
    ctx.fillText("命名卷: " + (model.volumes.join(", ") || "—"), 24, footerY + 24);
    if (!model.services.length) {
      ctx.fillStyle = muted;
      ctx.font = "14px system-ui, sans-serif";
      ctx.fillText("Compose 文件中未解析到 services", 24, 92);
    }
    canvas.title = model.services.map(s => s.name + (s.image ? " · " + s.image : "")).join("\n");
    return model;
  }

  global.ComposeCanvas = { parse, render };
})(window);
