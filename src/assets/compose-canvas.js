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
    const model = { services: [], networks: [], volumes: [], volumeDetails: {} };
    let section = "", service = null, volumeDef = null, nested = "";
    for (const raw of String(yaml || "").split(/\r?\n/)) {
      const line = raw.replace(/\s+#.*$/, "");
      if (!line.trim()) continue;
      const n = indent(line), text = line.trim();
      if (n === 0 && /^[\w.-]+:\s*$/.test(text)) {
        section = text.slice(0, -1); service = null; volumeDef = null; nested = ""; continue;
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
        if (name) {
          model[section].push(name);
          if (section === "volumes") volumeDef = model.volumeDetails[name] = { name, driver: "", external: false };
        }
      } else if (section === "volumes" && volumeDef && n === 4) {
        const at=text.indexOf(":");if(at<0)continue;const key=text.slice(0,at).trim(),value=clean(text.slice(at+1));
        if(key==="driver")volumeDef.driver=value;else if(key==="external")volumeDef.external=value.toLowerCase()==="true";
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

  function locateTopEntry(lines, sectionName, entryName) {
    const section=lines.findIndex(line=>indent(line)===0&&line.trim()===sectionName+":");if(section<0)return null;
    let sectionEnd=lines.length,start=-1,end=lines.length;
    for(let i=section+1;i<lines.length;i+=1){if(lines[i].trim()&&indent(lines[i])===0){sectionEnd=i;break;}if(indent(lines[i])===2&&clean(lines[i].trim().replace(/:\s*$/, ""))===entryName){start=i;break;}}
    if(start<0)return{section,sectionEnd,start:-1,end:-1};
    for(let i=start+1;i<lines.length;i+=1){if(lines[i].trim()&&indent(lines[i])<=2){end=i;break;}}
    return{section,sectionEnd,start,end};
  }

  function updateVolumeYaml(yaml, oldName, values) {
    const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/);let range=locateTopEntry(lines,"volumes",oldName),block=["  "+quote(values.name)+":"];
    if(range&&range.start>=0){block=lines.slice(range.start,range.end);block[0]="  "+quote(values.name)+":";}
    const setKey=(key,value)=>{let at=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim().startsWith(key+":")),end=at<0?at:block.length;if(at>=0)for(let i=at+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){end=i;break;}}const next=value?["    "+key+": "+value]:[];if(at>=0)block.splice(at,end-at,...next);else if(next.length)block.push(...next);};
    setKey("driver",values.driver?quote(values.driver):"");setKey("external",values.external?"true":"");
    if(!range){let at=lines.findIndex(line=>line.trim()===LAYOUT_BEGIN);if(at<0)at=lines.length;lines.splice(at,0,"volumes:",...block);}
    else if(range.start<0)lines.splice(range.sectionEnd,0,...block);
    else lines.splice(range.start,range.end-range.start,...block);
    return lines.join(eol);
  }

  function removeVolumeYaml(yaml, name) {
    const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),range=locateTopEntry(lines,"volumes",name);
    if(range&&range.start>=0)lines.splice(range.start,range.end-range.start);
    return lines.join(eol);
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
      Object.assign(this, { selected: "", selectedVolume: "", selectedLink: null, hoverLink: null, selectedMount: null, hoverMount: null, mountTarget: "", linkFrom: "", linkTarget: "", drag: null, pointer: { x: 0, y: 0 } }, options);
      this.yaml = String(options.yaml || ""); this.model = parse(this.yaml);
      this.positions = readLayout(this.yaml);
      if (!this.positions) {
        try { this.positions = JSON.parse(localStorage.getItem(this.storageKey) || "{}"); } catch (_) { this.positions = {}; }
      }
      this.down = e => this.pointerDown(e); this.move = e => this.pointerMove(e); this.up = e => this.pointerUp(e); this.key = e => this.keyDown(e);
      this.canvas.tabIndex = 0;
      this.canvas.addEventListener("pointerdown", this.down); this.canvas.addEventListener("pointermove", this.move);
      this.canvas.addEventListener("pointerup", this.up); this.canvas.addEventListener("pointercancel", this.up);
      this.canvas.addEventListener("keydown", this.key);
      this.renderInspector(); this.render();
    }
    destroy() {
      this.canvas.removeEventListener("pointerdown", this.down); this.canvas.removeEventListener("pointermove", this.move);
      this.canvas.removeEventListener("pointerup", this.up); this.canvas.removeEventListener("pointercancel", this.up);
      this.canvas.removeEventListener("keydown", this.key);
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
    volumeTop() { let bottom=80;this.model.services.forEach(service=>{const b=this.serviceBox(service);bottom=Math.max(bottom,b.y+b.h);});return bottom+72; }
    volumeBox(i) { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20)));return{x:24+(i%cols)*(VW+20),y:this.volumeTop()+Math.floor(i/cols)*50,w:VW,h:VH}; }
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
    mountHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2-15,y:b.y+b.h-8,w:30,h:16}; }
    hitMountHandle(p) {
      return [...this.model.services].reverse().find(service=>{const b=this.mountHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null;
    }
    hitVolume(p) {
      for (let i = 0; i < this.model.volumes.length; i += 1) { const b = this.volumeBox(i); if (p.x >= b.x && p.x <= b.x + b.w && p.y >= b.y && p.y <= b.y + b.h) return this.model.volumes[i]; }
      return "";
    }
    dependencyGeometry(from, to) {
      const source=this.model.services.find(s=>s.name===from),target=this.model.services.find(s=>s.name===to);if(!source||!target)return null;
      const a=this.serviceBox(source),b=this.serviceBox(target),sx=a.x,sy=a.y+a.h/2,cx=b.x+b.w/2,cy=b.y+b.h/2;let dx=cx-sx,dy=cy-sy;if(Math.abs(dx)+Math.abs(dy)<.01)dx=1;
      const xr=Math.abs(dx)/(b.w/2),yr=Math.abs(dy)/(b.h/2),edgeScale=1/Math.max(xr,yr),tx=cx-dx*edgeScale,ty=cy-dy*edgeScale,nx=xr>=yr?(dx>0?-1:1):0,ny=xr>=yr?0:(dy>0?-1:1),px=tx+nx*20,py=ty+ny*20;
      return { from, to, points:[{x:sx,y:sy},{x:sx-15,y:sy},{x:px,y:py},{x:tx,y:ty}], arrowAng:Math.atan2(ty-py,tx-px) };
    }
    sameLink(a,b) { return !!a && !!b && a.from===b.from && a.to===b.to; }
    pointSegmentDistance(p,a,b) {
      const dx=b.x-a.x,dy=b.y-a.y,length=dx*dx+dy*dy,t=length?Math.max(0,Math.min(1,((p.x-a.x)*dx+(p.y-a.y)*dy)/length)):0,x=a.x+t*dx,y=a.y+t*dy;
      return Math.hypot(p.x-x,p.y-y);
    }
    hitDependency(p) {
      const links=[];this.model.services.forEach(s=>s.depends.forEach(to=>links.push(this.dependencyGeometry(s.name,to))));
      return links.reverse().find(link=>link&&link.points.slice(1).some((point,index)=>this.pointSegmentDistance(p,link.points[index],point)<=7))||null;
    }
    linkMidpoint(link) {
      const lengths=link.points.slice(1).map((point,index)=>Math.hypot(point.x-link.points[index].x,point.y-link.points[index].y)),half=lengths.reduce((a,b)=>a+b,0)/2;let walked=0;
      for(let i=0;i<lengths.length;i+=1){if(walked+lengths[i]>=half){const t=(half-walked)/lengths[i],a=link.points[i],b=link.points[i+1];return{x:a.x+(b.x-a.x)*t,y:a.y+(b.y-a.y)*t};}walked+=lengths[i];}
      return link.points[1];
    }
    hitDelete(p) { if(!this.selectedLink)return false;const at=this.linkMidpoint(this.selectedLink);return Math.hypot(p.x-at.x,p.y-at.y)<=11; }
    mountGeometry(serviceName,volumeName) {
      const service=this.model.services.find(s=>s.name===serviceName),index=this.model.volumes.indexOf(volumeName);if(!service||index<0)return null;
      const handle=this.mountHandleBox(service),volume=this.volumeBox(index);return{service:serviceName,volume:volumeName,points:[{x:handle.x+handle.w/2,y:handle.y+handle.h/2},{x:volume.x+volume.w/2,y:volume.y}]};
    }
    sameMount(a,b) { return !!a&&!!b&&a.service===b.service&&a.volume===b.volume; }
    mountLinks() { const links=[];this.model.services.forEach(service=>service.volumes.forEach(mount=>{const volume=mount.split(":")[0],link=this.mountGeometry(service.name,volume);if(link)links.push(link);}));return links; }
    hitMount(p) { return this.mountLinks().filter(link=>link.service===this.selected).reverse().find(link=>this.pointSegmentDistance(p,link.points[0],link.points[1])<=7)||null; }
    mountMidpoint(link) { return{x:(link.points[0].x+link.points[1].x)/2,y:(link.points[0].y+link.points[1].y)/2}; }
    hitMountDelete(p) { if(!this.selectedMount)return false;const at=this.mountMidpoint(this.selectedMount);return Math.hypot(p.x-at.x,p.y-at.y)<=11; }
    pointerDown(e) {
      const p = this.point(e), handle = this.hitLinkHandle(p), mountHandle=this.hitMountHandle(p), service = this.hitService(p), volume = this.hitVolume(p); this.pointer = p;
      if (this.hitDelete(p)) { this.removeSelectedDependency(); return; }
      if (this.hitMountDelete(p)) { this.removeSelectedMount(); return; }
      if (handle) {
        this.selectedLink = null; this.selectedMount=null; this.selectedVolume = "";
        this.selected = handle.name; this.linkFrom = handle.name; this.linkTarget = "";
        this.drag = { type: "link", name: handle.name };
        this.canvas.style.cursor = LINK_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.onStatus(`拖到目标服务，为 ${handle.name} 添加依赖`); this.render(); return;
      }
      if(mountHandle){this.selected=mountHandle.name;this.selectedVolume="";this.selectedLink=null;this.selectedMount=null;this.mountTarget="";this.drag={type:"mount",name:mountHandle.name};this.canvas.style.cursor=LINK_CURSOR;this.canvas.setPointerCapture(e.pointerId);this.renderInspector();this.onStatus(`拖到命名卷，为 ${mountHandle.name} 创建挂载`);this.render();return;}
      if (service && this.linkFrom) {
        if (service.name !== this.linkFrom) this.addDependency(this.linkFrom, service.name);
        this.linkFrom = ""; this.onStatus("依赖连接完成"); this.render(); return;
      }
      if (service) {
        this.selected = service.name; this.selectedVolume = ""; this.selectedLink = null; this.selectedMount=null; const pos = this.positions[service.name];
        this.drag = { type: "service", name: service.name, dx: p.x - pos.x, dy: p.y - pos.y, moved: false };
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.render();
      } else if (volume) {
        this.selected = ""; this.selectedVolume = volume; this.selectedLink = null; this.selectedMount=null; this.drag = null; this.canvas.focus(); this.renderInspector(); this.render();
      } else {
        const mount=this.hitMount(p),link=mount?null:this.hitDependency(p);this.selectedMount=mount;this.selectedLink=link;this.canvas.focus();
        this.onStatus(mount?`已选择挂载 ${mount.service} → ${mount.volume}，按 Delete 删除`:(link ? `已选择依赖 ${link.from} → ${link.to}，按 Delete 删除` : "")); this.render();
      }
    }
    pointerMove(e) {
      const p = this.point(e); this.pointer = p;
      if (!this.drag) {
        const previous=this.hoverLink,previousMount=this.hoverMount,handle=this.hitLinkHandle(p),mountHandle=this.hitMountHandle(p),node=this.hitService(p),volume=this.hitVolume(p);this.hoverMount=handle||mountHandle||node||volume?null:this.hitMount(p);this.hoverLink=handle||mountHandle||node||volume||this.hoverMount?null:this.hitDependency(p);
        this.canvas.style.cursor = this.hitDelete(p)||this.hitMountDelete(p)||this.hoverLink||this.hoverMount||volume?"pointer":(handle||mountHandle?LINK_CURSOR:(node?"grab":"default"));
        if(!this.sameLink(previous,this.hoverLink)||!this.sameMount(previousMount,this.hoverMount))this.render();
        return;
      }
      this.canvas.style.cursor = this.drag.type === "link"||this.drag.type==="mount" ? LINK_CURSOR : "grabbing";
      if (this.drag.type === "link") {
        const target = this.hitService(p);
        this.linkTarget = target && target.name !== this.drag.name ? target.name : "";
      }
      if(this.drag.type==="mount")this.mountTarget=this.hitVolume(p)||"";
      if (this.drag.type === "service") {
        this.positions[this.drag.name] = { x: Math.max(190, p.x - this.drag.dx), y: Math.max(35, p.y - this.drag.dy) };
        this.drag.moved = true;
      }
      this.render();
    }
    pointerUp(e) {
      if (!this.drag) return; const p = this.point(e);
      if (this.drag.type === "link") {
        const target = e.type === "pointercancel" ? null : (this.model.services.find(service => service.name === this.linkTarget) || this.hitService(p)), from = this.drag.name;
        if (target && target.name !== from) this.addDependency(from, target.name);
        else this.onStatus("未连接：请在另一个服务上松开鼠标");
        this.linkFrom = ""; this.linkTarget = "";
      } else if(this.drag.type==="mount"){
        const volume=e.type==="pointercancel"?"":(this.mountTarget||this.hitVolume(p));if(volume)this.attachVolume(volume,this.drag.name);else this.onStatus("未连接：请在命名卷上松开鼠标");this.mountTarget="";
      } else if (this.drag.moved) {
        try { localStorage.setItem(this.storageKey, JSON.stringify(this.positions)); } catch (_) {}
        this.yaml = writeLayout(this.yaml, this.positions);
        this.onChange(this.yaml, "已更新 Canvas 布局"); this.onStatus("已更新 Canvas 布局");
      }
      this.drag = null; this.render();
      this.canvas.style.cursor = this.hitLinkHandle(p)||this.hitMountHandle(p) ? LINK_CURSOR : (this.hitService(p) ? "grab" : (this.hitVolume(p)?"pointer":"default"));
    }
    startLink() {
      if (!this.selected) { this.onStatus("请先点击需要添加依赖的服务"); return; }
      this.linkFrom = this.selected; this.onStatus("请点击它所依赖的目标服务"); this.render();
    }
    keyDown(e) {
      if ((e.key === "Delete" || e.key === "Backspace") && this.selectedLink) { e.preventDefault(); this.removeSelectedDependency(); }
      else if ((e.key === "Delete" || e.key === "Backspace") && this.selectedMount) { e.preventDefault(); this.removeSelectedMount(); }
      else if ((e.key === "Delete" || e.key === "Backspace") && this.selectedVolume) { e.preventDefault(); this.removeSelectedVolume(); }
      else if (e.key === "Escape" && (this.selectedLink || this.selectedMount || this.selectedVolume)) { e.preventDefault(); this.selectedLink = null; this.selectedMount=null; this.selectedVolume = ""; this.onStatus("已取消选择"); this.renderInspector(); this.render(); }
    }
    removeSelectedDependency() {
      const link=this.selectedLink;if(!link)return;
      const service=this.model.services.find(s=>s.name===link.from);if(!service)return;
      const depends=service.depends.filter(name=>name!==link.to);this.selectedLink=null;this.hoverLink=null;
      this.commit(service,{depends},`已删除依赖 ${link.from} → ${link.to}`);
    }
    removeSelectedMount() {
      const link=this.selectedMount;if(!link)return;const service=this.model.services.find(s=>s.name===link.service);if(!service)return;
      const volumes=service.volumes.filter(mount=>mount.split(":")[0]!==link.volume);this.selectedMount=null;this.hoverMount=null;
      this.commit(service,{volumes},`已删除挂载 ${link.service} → ${link.volume}`);
    }
    addDependency(from, to) {
      const service = this.model.services.find(s => s.name === from);
      if (service && !service.depends.includes(to)) { service.depends.push(to); this.commit(service, { depends: service.depends }, `已添加依赖 ${from} → ${to}`); }
    }
    addVolume() {
      let index=1,name="volume-1";while(this.model.volumes.includes(name))name="volume-"+(++index);
      this.yaml=updateVolumeYaml(this.yaml,"",{name,driver:"local",external:false});this.model=parse(this.yaml);this.selected="";this.selectedVolume=name;
      this.onChange(this.yaml,`已添加命名卷 ${name}`);this.onStatus(`已添加命名卷 ${name}`);this.renderInspector();this.render();
    }
    updateSelectedVolume(values) {
      const oldName=this.selectedVolume,newName=String(values.name||"").trim();if(!oldName)return;
      if(!/^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(newName)){this.onStatus("卷名称只能包含字母、数字、点、下划线和连字符");return;}
      if(newName!==oldName&&this.model.volumes.includes(newName)){this.onStatus(`命名卷 ${newName} 已存在`);return;}
      let yaml=updateVolumeYaml(this.yaml,oldName,{name:newName,driver:String(values.driver||"").trim(),external:!!values.external});
      if(newName!==oldName)this.model.services.forEach(service=>{const mounts=service.volumes.map(mount=>{const parts=mount.split(":");if(parts[0]===oldName)parts[0]=newName;return parts.join(":");});if(JSON.stringify(mounts)!==JSON.stringify(service.volumes))yaml=updateServiceYaml(yaml,service.name,{volumes:mounts});});
      this.yaml=yaml;this.model=parse(yaml);this.selectedVolume=newName;this.onChange(yaml,`已更新命名卷 ${newName}`);this.onStatus(`已更新命名卷 ${newName}`);this.renderInspector();this.render();
    }
    removeSelectedVolume() {
      const name=this.selectedVolume;if(!name)return;const affected=this.model.services.filter(service=>service.volumes.some(mount=>mount.split(":")[0]===name));
      const suffix=affected.length?`\n同时会移除以下服务的挂载：${affected.map(service=>service.name).join("、")}`:"";
      if(!global.confirm(`确定删除命名卷 ${name}？${suffix}`))return;
      let yaml=removeVolumeYaml(this.yaml,name);affected.forEach(service=>{const mounts=service.volumes.filter(mount=>mount.split(":")[0]!==name);yaml=updateServiceYaml(yaml,service.name,{volumes:mounts});});
      this.yaml=yaml;this.model=parse(yaml);this.selectedVolume="";this.onChange(yaml,`已删除命名卷 ${name}`);this.onStatus(`已删除命名卷 ${name}`);this.renderInspector();this.render();
    }
    attachVolume(volume, name) {
      const service = this.model.services.find(s => s.name === name);
      if (!service || service.volumes.some(v => v.split(":")[0] === volume)) { this.onStatus(`${volume} 已挂载到 ${name}`); return; }
      service.volumes.push(`${volume}:/mnt/${volume}`); this.selected=name;this.selectedVolume="";this.commit(service, { volumes: service.volumes }, `已将卷 ${volume} 挂载到 ${name}，可在右侧修改路径或添加 :ro`);
    }
    commit(service, values, message) {
      this.yaml = updateServiceYaml(this.yaml, service.name, values); this.model = parse(this.yaml);
      this.onChange(this.yaml, message); this.onStatus(message); this.renderInspector(); this.render();
    }
    renderInspector() {
      if (this.selectedVolume) {
        const volume=this.model.volumeDetails[this.selectedVolume]||{name:this.selectedVolume,driver:"",external:false};
        this.inspector.innerHTML=`<div class="compose-inspector-title">命名卷 ${html(volume.name)}</div><label>卷名称<input data-volume-name type="text" value="${html(volume.name)}" /></label><label>Driver<input data-volume-driver type="text" value="${html(volume.driver||"")}" placeholder="local" /></label><label style="flex-direction:row;align-items:center"><input data-volume-external type="checkbox" style="width:auto" ${volume.external?"checked":""} /> External volume</label><div style="display:flex;gap:8px"><button type="button" class="btn primary" data-volume-apply>应用到草稿</button><button type="button" class="btn danger" data-volume-delete>删除</button></div>`;
        this.inspector.querySelector("[data-volume-apply]").onclick=()=>this.updateSelectedVolume({name:this.inspector.querySelector("[data-volume-name]").value,driver:this.inspector.querySelector("[data-volume-driver]").value,external:this.inspector.querySelector("[data-volume-external]").checked});
        this.inspector.querySelector("[data-volume-delete]").onclick=()=>this.removeSelectedVolume();return;
      }
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
      this.ensurePositions(); let width = Math.max(820, this.canvas.parentElement ? this.canvas.parentElement.clientWidth : 0), height = 360;
      this.model.services.forEach(s => { const b = this.serviceBox(s); width = Math.max(width, b.x + b.w + 40); height = Math.max(height, b.y + b.h + 40); });
      this.model.volumes.forEach((v,i)=>{const b=this.volumeBox(i);height=Math.max(height,b.y+b.h+32);});
      const dpr = Math.max(1, global.devicePixelRatio || 1); this.canvas.width = width * dpr; this.canvas.height = height * dpr; this.canvas.style.width = width + "px"; this.canvas.style.height = height + "px";
      const ctx = this.canvas.getContext("2d"); ctx.scale(dpr, dpr);
      const c = { bg: color("--bg-2","#161b22"), card: color("--bg-3","#21262d"), text: color("--text","#f0f6fc"), muted: color("--muted","#8b949e"), line: color("--line","#30363d"), accent: color("--accent","#58a6ff"), active: color("--bg-active","#1f6feb33") };
      ctx.fillStyle = c.bg; ctx.fillRect(0, 0, width, height); ctx.fillStyle = c.muted; ctx.font = "600 12px system-ui"; ctx.fillText("命名卷", 24, this.volumeTop()-20);
      this.drawMounts(ctx, c); this.drawLinks(ctx, c); this.model.volumes.forEach((v,i) => this.drawVolume(ctx,v,i,c)); this.model.services.forEach(s => this.drawService(ctx,s,c));
      if (this.drag && this.drag.type === "link") {
        const service=this.model.services.find(s=>s.name===this.drag.name),b=service&&this.serviceBox(service);
        if(b){ctx.save();ctx.strokeStyle=c.accent;ctx.lineWidth=1.5;ctx.setLineDash([5,4]);ctx.beginPath();ctx.moveTo(b.x,b.y+b.h/2);ctx.lineTo(b.x-15,b.y+b.h/2);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore();}
      }
      if(this.drag&&this.drag.type==="mount"){const service=this.model.services.find(s=>s.name===this.drag.name),b=service&&this.mountHandleBox(service);if(b){ctx.save();ctx.strokeStyle=c.accent;ctx.lineWidth=1.5;ctx.setLineDash([6,5]);ctx.beginPath();ctx.moveTo(b.x+b.w/2,b.y+b.h/2);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore();}}
    }
    drawMounts(ctx,c) {
      if (!this.selected) return;
      this.mountLinks().filter(link=>link.service===this.selected).forEach(link=>{const active=this.sameMount(link,this.selectedMount)||this.sameMount(link,this.hoverMount);ctx.save();ctx.strokeStyle=active?c.accent:c.muted;ctx.lineWidth=active?3:1.25;ctx.setLineDash([6,5]);if(active){ctx.shadowColor=c.accent;ctx.shadowBlur=7;}ctx.beginPath();ctx.moveTo(link.points[0].x,link.points[0].y);ctx.lineTo(link.points[1].x,link.points[1].y);ctx.stroke();ctx.restore();});
      if(this.selectedMount){const at=this.mountMidpoint(this.selectedMount);ctx.save();ctx.beginPath();ctx.arc(at.x,at.y,10,0,Math.PI*2);ctx.fillStyle="#d1242f";ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.stroke();ctx.beginPath();ctx.moveTo(at.x-3.5,at.y-3.5);ctx.lineTo(at.x+3.5,at.y+3.5);ctx.moveTo(at.x+3.5,at.y-3.5);ctx.lineTo(at.x-3.5,at.y+3.5);ctx.stroke();ctx.restore();}
    }
    drawLinks(ctx,c) {
      this.model.services.forEach(sourceService => sourceService.depends.forEach(name => {
        const link=this.dependencyGeometry(sourceService.name,name);if(!link)return;const active=this.sameLink(link,this.selectedLink)||this.sameLink(link,this.hoverLink),points=link.points,target=points[points.length-1];
        ctx.save();ctx.strokeStyle=c.accent;ctx.fillStyle=c.accent;ctx.lineWidth=active?3:1.5;if(active){ctx.shadowColor=c.accent;ctx.shadowBlur=7;}ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);points.slice(1).forEach(point=>ctx.lineTo(point.x,point.y));ctx.stroke();ctx.beginPath();ctx.moveTo(target.x,target.y);ctx.lineTo(target.x-Math.cos(link.arrowAng-.45)*9,target.y-Math.sin(link.arrowAng-.45)*9);ctx.lineTo(target.x-Math.cos(link.arrowAng+.45)*9,target.y-Math.sin(link.arrowAng+.45)*9);ctx.closePath();ctx.fill();ctx.restore();
      }));
      if(this.selectedLink){const at=this.linkMidpoint(this.selectedLink);ctx.save();ctx.beginPath();ctx.arc(at.x,at.y,10,0,Math.PI*2);ctx.fillStyle="#d1242f";ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.stroke();ctx.strokeStyle="#fff";ctx.lineWidth=1.7;ctx.beginPath();ctx.moveTo(at.x-3.5,at.y-3.5);ctx.lineTo(at.x+3.5,at.y+3.5);ctx.moveTo(at.x+3.5,at.y-3.5);ctx.lineTo(at.x-3.5,at.y+3.5);ctx.stroke();ctx.restore();}
    }
    drawService(ctx,s,c) {
      const b=this.serviceBox(s),isTarget=s.name===this.linkTarget,count=s.volumes.filter(mount=>this.model.volumes.includes(mount.split(":")[0])).length,linked=count>0;ctx.save();if(isTarget){ctx.shadowColor=c.accent;ctx.shadowBlur=12;}roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=(s.name===this.selected||isTarget)?c.active:c.card;ctx.fill();ctx.strokeStyle=(s.name===this.selected||s.name===this.linkFrom||isTarget||linked)?c.accent:c.line;ctx.lineWidth=isTarget?3:((s.name===this.selected||s.name===this.linkFrom)?2:1);ctx.stroke();ctx.restore();ctx.fillStyle=c.text;ctx.font="600 14px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(clip(ctx,s.name,b.w-24),b.x+b.w/2,b.y+b.h/2);ctx.textAlign="left";ctx.textBaseline="alphabetic";
      ctx.beginPath();ctx.arc(b.x,b.y+b.h/2,6,0,Math.PI*2);ctx.fillStyle=c.bg;ctx.fill();ctx.strokeStyle=c.accent;ctx.lineWidth=2;ctx.stroke();
      const mh=this.mountHandleBox(s);roundRect(ctx,mh.x,mh.y,mh.w,mh.h,3);ctx.fillStyle=c.card;ctx.fill();ctx.strokeStyle=linked?c.accent:c.line;ctx.lineWidth=1.5;ctx.stroke();ctx.fillStyle=linked?c.text:c.muted;ctx.font="600 10px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(String(count),mh.x+mh.w/2,mh.y+mh.h/2);ctx.textAlign="left";ctx.textBaseline="alphabetic";
    }
    drawPill(ctx,x,y,w,h,text,c) { roundRect(ctx,x,y,w,h,12);ctx.fillStyle=c.card;ctx.fill();ctx.strokeStyle=c.line;ctx.lineWidth=1;ctx.stroke();ctx.fillStyle=c.text;ctx.font="12px ui-monospace";ctx.textBaseline="middle";ctx.fillText(clip(ctx,text,w-20),x+10,y+h/2);ctx.textBaseline="alphabetic"; }
    drawVolume(ctx,v,i,c) { const b=this.volumeBox(i),active=v===this.selectedVolume||v===this.mountTarget;this.drawPill(ctx,b.x,b.y,b.w,b.h,v,c);if(active){ctx.save();if(v===this.mountTarget){ctx.shadowColor=c.accent;ctx.shadowBlur=12;}roundRect(ctx,b.x,b.y,b.w,b.h,12);ctx.strokeStyle=c.accent;ctx.lineWidth=v===this.mountTarget?3:2.5;ctx.stroke();ctx.restore();} }
  }

  global.ComposeCanvas = { parse, readLayout, writeLayout, updateServiceYaml, updateVolumeYaml, removeVolumeYaml, create: options => new Editor(options) };
})(window);
