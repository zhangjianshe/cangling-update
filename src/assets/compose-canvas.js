(function (global) {
  "use strict";
  const SW = 164, SH = 54, VW = 148, VH = 34;
  const LAYOUT_BEGIN = "# cangling-canvas-layout:begin";
  const LAYOUT_END = "# cangling-canvas-layout:end";
  const LINK_CURSOR = `url("data:image/svg+xml,${encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="28" height="28" viewBox="0 0 28 28"><path d="M3 2l14 13-7 .7-3.7 6.1z" fill="white" stroke="#24292f" stroke-width="1.5" stroke-linejoin="round"/><circle cx="19" cy="19" r="7" fill="#0d99ff" stroke="white" stroke-width="1.5"/><path d="M19 15v8m-4-4h8" stroke="white" stroke-width="1.6" stroke-linecap="round"/></svg>')}" ) 3 2, crosshair`;
  const indent = line => (line.match(/^\s*/) || [""])[0].replace(/\t/g, "  ").length;
  const clean = value => String(value || "").trim().replace(/^['"]/, "").replace(/['"]$/, "");
  const unique = values => [...new Set((values || []).map(v => String(v).trim()).filter(Boolean))];
  const inlineList = value => {
    const text = clean(value);
    return text.startsWith("[") && text.endsWith("]") ? text.slice(1, -1).split(",").map(clean).filter(Boolean) : [];
  };

  function readLayout(yaml) {
    const lines = String(yaml || "").split(/\r?\n/), begin = lines.findIndex(line => line.trim() === LAYOUT_BEGIN);
    if (begin < 0) return null;
    const end = lines.findIndex((line, index) => index > begin && line.trim() === LAYOUT_END);
    if (end < 0) return null;
    try {
      const data = JSON.parse(lines.slice(begin + 1, end).map(line => line.replace(/^\s*#\s?/, "")).join(""));
      return data && data.version === 1 && data.positions && typeof data.positions === "object" ? data.positions : null;
    } catch (_) { return null; }
  }

  function writeLayout(yaml, positions) {
    const source = String(yaml || ""), eol = source.includes("\r\n") ? "\r\n" : "\n", lines = source.split(/\r?\n/);
    const begin = lines.findIndex(line => line.trim() === LAYOUT_BEGIN);
    if (begin >= 0) {
      const end = lines.findIndex((line, index) => index > begin && line.trim() === LAYOUT_END);
      lines.splice(begin, (end >= 0 ? end : begin) - begin + 1);
    }
    while (lines.length && !lines[lines.length - 1].trim()) lines.pop();
    const saved = {};
    Object.entries(positions || {}).forEach(([name, point]) => {
      const x = Math.round(Number(point && point.x)), y = Math.round(Number(point && point.y));
      if (Number.isFinite(x) && Number.isFinite(y)) saved[name] = { x, y };
    });
    lines.push("", LAYOUT_BEGIN, "# " + JSON.stringify({ version: 1, positions: saved }), LAYOUT_END, "");
    return lines.join(eol);
  }

  function parse(yaml) {
    const model = { services: [], networks: [], volumes: [] };
    let section = "", service = null, nested = "";
    for (const raw of String(yaml || "").split(/\r?\n/)) {
      const line = raw.replace(/\s+#.*$/, "");
      if (!line.trim()) continue;
      const n = indent(line), text = line.trim();
      if (n === 0 && /^[\w.-]+:\s*$/.test(text)) {
        section = text.slice(0, -1); service = null; nested = ""; continue;
      }
      if (section === "services" && n === 2 && /^[^:]+:\s*$/.test(text)) {
        service = { name: clean(text.slice(0, -1)), image: "", containerName: "", command: "", restart: "", depends: [], networks: [], volumes: [], ports: [] };
        model.services.push(service); nested = ""; continue;
      }
      if (section === "services" && service) {
        if (n === 4) {
          const at = text.indexOf(":");
          if (at < 0) continue;
          const key = text.slice(0, at).trim(), value = clean(text.slice(at + 1));
          nested = key;
          if (key === "image") service.image = value;
          else if (key === "container_name") service.containerName = value;
          else if (key === "command") service.command = value;
          else if (key === "restart") service.restart = value;
          else if (key === "depends_on") service.depends.push(...inlineList(value));
          else if (key === "networks") service.networks.push(...inlineList(value));
          else if (key === "volumes") service.volumes.push(...inlineList(value));
          else if (key === "ports") service.ports.push(...inlineList(value));
          continue;
        }
        if (n >= 6 && text.startsWith("- ")) {
          const value = clean(text.slice(2));
          if (nested === "depends_on") service.depends.push(value);
          else if (nested === "networks") service.networks.push(value);
          else if (nested === "volumes") service.volumes.push(value);
          else if (nested === "ports") service.ports.push(value);
        } else if (n === 6 && (nested === "depends_on" || nested === "networks")) {
          const key = clean(text.split(":", 1)[0]);
          if (key) service[nested === "depends_on" ? "depends" : "networks"].push(key);
        }
      } else if ((section === "networks" || section === "volumes") && n === 2) {
        const name = clean(text.split(":", 1)[0]);
        if (name) model[section].push(name);
      }
    }
    const names = new Set(model.services.map(s => s.name));
    for (const item of model.services) {
      item.depends = unique(item.depends.filter(name => names.has(name)));
      item.networks = unique(item.networks.map(v => v.split(":")[0]));
      item.volumes = unique(item.volumes); item.ports = unique(item.ports);
    }
    model.networks = unique(model.networks); model.volumes = unique(model.volumes);
    return model;
  }

  const quote = value => /[:#{}[\],&*!|>'"%@`\s]/.test(String(value || "")) ? JSON.stringify(String(value)) : String(value);
  function locateService(lines, name) {
    let services = lines.findIndex(line => indent(line) === 0 && line.trim() === "services:");
    if (services < 0) return null;
    let start = -1, end = lines.length;
    for (let i = services + 1; i < lines.length; i += 1) {
      const text = lines[i].trim(), n = indent(lines[i]);
      if (text && n === 0) break;
      if (n === 2 && clean(text.replace(/:\s*$/, "")) === name) { start = i; break; }
    }
    if (start < 0) return null;
    for (let i = start + 1; i < lines.length; i += 1) {
      if (lines[i].trim() && indent(lines[i]) <= 2) { end = i; break; }
    }
    return { start, end };
  }

  function replaceKey(block, key, value, list) {
    let start = -1, end = block.length;
    for (let i = 1; i < block.length; i += 1) {
      if (indent(block[i]) === 4 && block[i].trim().startsWith(key + ":")) { start = i; break; }
    }
    if (start >= 0) for (let i = start + 1; i < block.length; i += 1) {
      if (block[i].trim() && indent(block[i]) <= 4) { end = i; break; }
    }
    const values = list ? unique(value) : String(value || "").trim();
    const next = list
      ? (values.length ? ["    " + key + ":", ...values.map(v => "      - " + quote(v))] : [])
      : (values ? ["    " + key + ": " + quote(values)] : []);
    if (start >= 0) block.splice(start, end - start, ...next);
    else if (next.length) block.splice(1, 0, ...next);
  }

  function updateServiceYaml(yaml, name, values) {
    const hadNewline = String(yaml || "").endsWith("\n");
    const lines = String(yaml || "").split(/\r?\n/), range = locateService(lines, name);
    if (!range) throw new Error("未能在 Compose 源文件中定位服务 " + name);
    const block = lines.slice(range.start, range.end);
    const fields = [
      ["image", "image", false], ["container_name", "containerName", false],
      ["command", "command", false], ["restart", "restart", false],
      ["depends_on", "depends", true], ["ports", "ports", true],
      ["volumes", "volumes", true], ["networks", "networks", true],
    ];
    fields.forEach(([yamlKey, field, list]) => {
      if (Object.prototype.hasOwnProperty.call(values, field)) replaceKey(block, yamlKey, values[field], list);
    });
    lines.splice(range.start, range.end - range.start, ...block);
    let result = lines.join("\n");
    if (hadNewline && !result.endsWith("\n")) result += "\n";
    return result;
  }

  const color = (name, fallback) => getComputedStyle(document.documentElement).getPropertyValue(name).trim() || fallback;
  function roundRect(ctx, x, y, w, h, r) {
    ctx.beginPath(); ctx.moveTo(x + r, y); ctx.arcTo(x + w, y, x + w, y + h, r);
    ctx.arcTo(x + w, y + h, x, y + h, r); ctx.arcTo(x, y + h, x, y, r); ctx.arcTo(x, y, x + w, y, r); ctx.closePath();
  }
  function clip(ctx, text, width) {
    let value = String(text || "");
    while (value && ctx.measureText(value).width > width) value = value.slice(0, -1);
    return value === text ? value : value + "…";
  }
  const html = value => String(value == null ? "" : value).replace(/[&<>"']/g, c => ({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;","'":"&#39;"})[c]);

  class Editor {
    constructor(options) {
      Object.assign(this, { selected: "", linkFrom: "", drag: null, pointer: { x: 0, y: 0 } }, options);
      this.yaml = String(options.yaml || ""); this.model = parse(this.yaml);
      this.positions = readLayout(this.yaml);
      if (!this.positions) {
        try { this.positions = JSON.parse(localStorage.getItem(this.storageKey) || "{}"); } catch (_) { this.positions = {}; }
      }
      this.down = e => this.pointerDown(e); this.move = e => this.pointerMove(e); this.up = e => this.pointerUp(e);
      this.canvas.addEventListener("pointerdown", this.down); this.canvas.addEventListener("pointermove", this.move);
      this.canvas.addEventListener("pointerup", this.up); this.canvas.addEventListener("pointercancel", this.up);
      this.renderInspector(); this.render();
    }
    destroy() {
      this.canvas.removeEventListener("pointerdown", this.down); this.canvas.removeEventListener("pointermove", this.move);
      this.canvas.removeEventListener("pointerup", this.up); this.canvas.removeEventListener("pointercancel", this.up);
    }
    ensurePositions() {
      const width = Math.max(720, this.canvas.parentElement ? this.canvas.parentElement.clientWidth : 0);
      const cols = Math.max(1, Math.floor((width - 230) / 220));
      this.model.services.forEach((s, i) => {
        const point = this.positions[s.name];
        if (!point || !Number.isFinite(Number(point.x)) || !Number.isFinite(Number(point.y))) this.positions[s.name] = { x: 210 + i % cols * 220, y: 55 + Math.floor(i / cols) * 120 };
      });
    }
    serviceBox(s) { const p = this.positions[s.name]; return { x: p.x, y: p.y, w: SW, h: SH }; }
    volumeBox(i) { return { x: 24, y: 68 + i * 50, w: VW, h: VH }; }
    point(e) { const r = this.canvas.getBoundingClientRect(); return { x: e.clientX - r.left, y: e.clientY - r.top }; }
    hitService(p) {
      return [...this.model.services].reverse().find(s => { const b = this.serviceBox(s); return p.x >= b.x && p.x <= b.x + b.w && p.y >= b.y && p.y <= b.y + b.h; }) || null;
    }
    hitLinkHandle(p) {
      return [...this.model.services].reverse().find(s => {
        const b = this.serviceBox(s);
        return Math.hypot(p.x - b.x, p.y - (b.y + b.h / 2)) <= 10;
      }) || null;
    }
    hitVolume(p) {
      for (let i = 0; i < this.model.volumes.length; i += 1) { const b = this.volumeBox(i); if (p.x >= b.x && p.x <= b.x + b.w && p.y >= b.y && p.y <= b.y + b.h) return this.model.volumes[i]; }
      return "";
    }
    pointerDown(e) {
      const p = this.point(e), handle = this.hitLinkHandle(p), service = this.hitService(p), volume = this.hitVolume(p); this.pointer = p;
      if (handle) {
        this.selected = handle.name; this.linkFrom = handle.name;
        this.drag = { type: "link", name: handle.name };
        this.canvas.style.cursor = LINK_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.onStatus(`拖到目标服务，为 ${handle.name} 添加依赖`); this.render(); return;
      }
      if (service && this.linkFrom) {
        if (service.name !== this.linkFrom) this.addDependency(this.linkFrom, service.name);
        this.linkFrom = ""; this.onStatus("依赖连接完成"); this.render(); return;
      }
      if (service) {
        this.selected = service.name; const pos = this.positions[service.name];
        this.drag = { type: "service", name: service.name, dx: p.x - pos.x, dy: p.y - pos.y, moved: false };
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.render();
      } else if (volume) {
        this.drag = { type: "volume", name: volume }; this.canvas.setPointerCapture(e.pointerId); this.render();
      }
    }
    pointerMove(e) {
      const p = this.point(e); this.pointer = p;
      if (!this.drag) {
        this.canvas.style.cursor = this.hitLinkHandle(p) ? LINK_CURSOR : (this.hitService(p) || this.hitVolume(p) ? "grab" : "default");
        return;
      }
      this.canvas.style.cursor = this.drag.type === "link" ? LINK_CURSOR : "grabbing";
      if (this.drag.type === "service") {
        this.positions[this.drag.name] = { x: Math.max(190, p.x - this.drag.dx), y: Math.max(35, p.y - this.drag.dy) };
        this.drag.moved = true;
      }
      this.render();
    }
    pointerUp(e) {
      if (!this.drag) return; const p = this.point(e);
      if (this.drag.type === "volume") { const service = this.hitService(p); if (service) this.attachVolume(this.drag.name, service.name); }
      else if (this.drag.type === "link") {
        const target = this.hitService(p), from = this.drag.name;
        if (target && target.name !== from) this.addDependency(from, target.name);
        else this.onStatus("未连接：请在另一个服务上松开鼠标");
        this.linkFrom = "";
      } else if (this.drag.moved) {
        try { localStorage.setItem(this.storageKey, JSON.stringify(this.positions)); } catch (_) {}
        this.yaml = writeLayout(this.yaml, this.positions);
        this.onChange(this.yaml, "已更新 Canvas 布局"); this.onStatus("已更新 Canvas 布局");
      }
      this.drag = null; this.render();
      this.canvas.style.cursor = this.hitLinkHandle(p) ? LINK_CURSOR : (this.hitService(p) || this.hitVolume(p) ? "grab" : "default");
    }
    startLink() {
      if (!this.selected) { this.onStatus("请先点击需要添加依赖的服务"); return; }
      this.linkFrom = this.selected; this.onStatus("请点击它所依赖的目标服务"); this.render();
    }
    addDependency(from, to) {
      const service = this.model.services.find(s => s.name === from);
      if (service && !service.depends.includes(to)) { service.depends.push(to); this.commit(service, { depends: service.depends }, `已添加依赖 ${from} → ${to}`); }
    }
    attachVolume(volume, name) {
      const service = this.model.services.find(s => s.name === name);
      if (!service || service.volumes.some(v => v.split(":")[0] === volume)) { this.onStatus(`${volume} 已挂载到 ${name}`); return; }
      service.volumes.push(`${volume}:/mnt/${volume}`); this.commit(service, { volumes: service.volumes }, `已将卷 ${volume} 挂载到 ${name}`);
    }
    commit(service, values, message) {
      this.yaml = updateServiceYaml(this.yaml, service.name, values); this.model = parse(this.yaml);
      this.onChange(this.yaml, message); this.onStatus(message); this.renderInspector(); this.render();
    }
    renderInspector() {
      const service = this.model.services.find(s => s.name === this.selected);
      if (!service) { this.inspector.innerHTML = '<div class="compose-inspector-empty">点击服务节点编辑属性</div>'; return; }
      const field = (label, key, value, area) => `<label>${label}${area ? `<textarea data-field="${key}" rows="3">${html((value || []).join("\n"))}</textarea>` : `<input data-field="${key}" type="text" value="${html(value || "")}" />`}</label>`;
      this.inspector.innerHTML = `<div class="compose-inspector-title">${html(service.name)}</div>${field("镜像","image",service.image)}${field("容器名称","containerName",service.containerName)}${field("启动命令","command",service.command)}${field("重启策略","restart",service.restart)}${field("依赖服务（每行一个）","depends",service.depends,true)}${field("端口（每行一个）","ports",service.ports,true)}${field("挂载（每行一个）","volumes",service.volumes,true)}${field("网络（每行一个）","networks",service.networks,true)}<button type="button" class="btn primary" data-apply>应用到草稿</button>`;
      this.inspector.querySelector("[data-apply]").onclick = () => {
        const patch = {};
        this.inspector.querySelectorAll("[data-field]").forEach(input => {
          const key = input.dataset.field, value = input.tagName === "TEXTAREA" ? unique(input.value.split(/\r?\n/)) : input.value.trim();
          if (JSON.stringify(value) !== JSON.stringify(service[key])) patch[key] = value;
        });
        if (!Object.keys(patch).length) { this.onStatus("服务属性没有变化"); return; }
        this.commit(service, patch, `已更新服务 ${service.name} 的属性`);
      };
    }
    render() {
      this.ensurePositions(); let width = Math.max(820, this.canvas.parentElement ? this.canvas.parentElement.clientWidth : 0), height = Math.max(360, 110 + this.model.volumes.length * 50);
      this.model.services.forEach(s => { const b = this.serviceBox(s); width = Math.max(width, b.x + b.w + 40); height = Math.max(height, b.y + b.h + 40); });
      const dpr = Math.max(1, global.devicePixelRatio || 1); this.canvas.width = width * dpr; this.canvas.height = height * dpr; this.canvas.style.width = width + "px"; this.canvas.style.height = height + "px";
      const ctx = this.canvas.getContext("2d"); ctx.scale(dpr, dpr);
      const c = { bg: color("--bg-2","#161b22"), card: color("--bg-3","#21262d"), text: color("--text","#f0f6fc"), muted: color("--muted","#8b949e"), line: color("--line","#30363d"), accent: color("--accent","#58a6ff"), active: color("--bg-active","#1f6feb33") };
      ctx.fillStyle = c.bg; ctx.fillRect(0, 0, width, height); ctx.fillStyle = c.muted; ctx.font = "600 12px system-ui"; ctx.fillText("命名卷（拖到服务）", 24, 40);
      this.drawMounts(ctx, c); this.drawLinks(ctx, c); this.model.volumes.forEach((v,i) => this.drawVolume(ctx,v,i,c)); this.model.services.forEach(s => this.drawService(ctx,s,c));
      if (this.drag && this.drag.type === "volume") { ctx.globalAlpha=.8; this.drawPill(ctx,this.pointer.x-VW/2,this.pointer.y-VH/2,VW,VH,this.drag.name,c); ctx.globalAlpha=1; }
      if (this.drag && this.drag.type === "link") {
        const service=this.model.services.find(s=>s.name===this.drag.name),b=service&&this.serviceBox(service);
        if(b){ctx.save();ctx.strokeStyle=c.accent;ctx.lineWidth=1.5;ctx.setLineDash([5,4]);ctx.beginPath();ctx.moveTo(b.x,b.y+b.h/2);ctx.lineTo(b.x-15,b.y+b.h/2);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore();}
      }
    }
    drawMounts(ctx,c) {
      if (!this.selected) return;
      ctx.save();ctx.strokeStyle=c.muted;ctx.lineWidth=1.25;ctx.setLineDash([6,5]);
      this.model.services.filter(service => service.name === this.selected).forEach(service => service.volumes.forEach(mount => {
        const source=mount.split(":")[0],index=this.model.volumes.indexOf(source);if(index<0)return;
        const volume=this.volumeBox(index),target=this.serviceBox(service);
        ctx.beginPath();ctx.moveTo(volume.x+volume.w,volume.y+volume.h/2);ctx.lineTo(target.x,target.y+target.h/2);ctx.stroke();
      }));
      ctx.restore();
    }
    drawLinks(ctx,c) {
      ctx.strokeStyle=c.accent; ctx.fillStyle=c.accent; ctx.lineWidth=1.5;
      this.model.services.forEach(sourceService => sourceService.depends.forEach(name => {
        const targetService=this.model.services.find(s=>s.name===name); if(!targetService)return;
        const a=this.serviceBox(sourceService),b=this.serviceBox(targetService),sx=a.x,sy=a.y+a.h/2,cx=b.x+b.w/2,cy=b.y+b.h/2;let dx=cx-sx,dy=cy-sy;if(Math.abs(dx)+Math.abs(dy)<.01)dx=1;
        const xr=Math.abs(dx)/(b.w/2),yr=Math.abs(dy)/(b.h/2),edgeScale=1/Math.max(xr,yr);
        const tx=cx-dx*edgeScale,ty=cy-dy*edgeScale,nx=xr>=yr?(dx>0?-1:1):0,ny=xr>=yr?0:(dy>0?-1:1),px=tx+nx*20,py=ty+ny*20,arrowAng=Math.atan2(ty-py,tx-px);
        ctx.beginPath();ctx.moveTo(sx,sy);ctx.lineTo(sx-15,sy);ctx.lineTo(px,py);ctx.lineTo(tx,ty);ctx.stroke();ctx.beginPath();ctx.moveTo(tx,ty);ctx.lineTo(tx-Math.cos(arrowAng-.45)*9,ty-Math.sin(arrowAng-.45)*9);ctx.lineTo(tx-Math.cos(arrowAng+.45)*9,ty-Math.sin(arrowAng+.45)*9);ctx.closePath();ctx.fill();
      }));
    }
    drawService(ctx,s,c) {
      const b=this.serviceBox(s);roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=s.name===this.selected?c.active:c.card;ctx.fill();ctx.strokeStyle=(s.name===this.selected||s.name===this.linkFrom)?c.accent:c.line;ctx.lineWidth=(s.name===this.selected||s.name===this.linkFrom)?2:1;ctx.stroke();ctx.fillStyle=c.text;ctx.font="600 14px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(clip(ctx,s.name,b.w-24),b.x+b.w/2,b.y+b.h/2);ctx.textAlign="left";ctx.textBaseline="alphabetic";
      ctx.beginPath();ctx.arc(b.x,b.y+b.h/2,6,0,Math.PI*2);ctx.fillStyle=c.bg;ctx.fill();ctx.strokeStyle=c.accent;ctx.lineWidth=2;ctx.stroke();
    }
    drawPill(ctx,x,y,w,h,text,c) { roundRect(ctx,x,y,w,h,12);ctx.fillStyle=c.card;ctx.fill();ctx.strokeStyle=c.line;ctx.lineWidth=1;ctx.stroke();ctx.fillStyle=c.text;ctx.font="12px ui-monospace";ctx.textBaseline="middle";ctx.fillText(clip(ctx,text,w-20),x+10,y+h/2);ctx.textBaseline="alphabetic"; }
    drawVolume(ctx,v,i,c) { const b=this.volumeBox(i);this.drawPill(ctx,b.x,b.y,b.w,b.h,v,c); }
  }

  global.ComposeCanvas = { parse, readLayout, writeLayout, updateServiceYaml, create: options => new Editor(options) };
})(window);
