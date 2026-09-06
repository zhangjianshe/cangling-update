(function (global) {
  "use strict";
  const SW = 164, SH = 54, VW = 148, VH = 34;
  const LAYOUT_BEGIN = "# cangling-canvas-layout:begin";
  const LAYOUT_END = "# cangling-canvas-layout:end";
  const SELECT_CURSOR = `url("data:image/svg+xml,${encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="22" height="22" viewBox="0 0 22 22"><path d="M3 3l14 6.5-6.2 2.6L8 18z" fill="#24292f" stroke="white" stroke-width="1.4" stroke-linejoin="round"/></svg>')}" ) 3 3, default`;
  const LINK_CURSOR = `url("data:image/svg+xml,${encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" width="28" height="28" viewBox="0 0 28 28"><path d="M3 2l14 13-7 .7-3.7 6.1z" fill="white" stroke="#24292f" stroke-width="1.5" stroke-linejoin="round"/><circle cx="19" cy="19" r="7" fill="#0d99ff" stroke="white" stroke-width="1.5"/><path d="M19 15v8m-4-4h8" stroke="white" stroke-width="1.6" stroke-linecap="round"/></svg>')}" ) 3 2, crosshair`;
  const indent = line => (line.match(/^\s*/) || [""])[0].replace(/\t/g, "  ").length;
  const clean = value => String(value || "").trim().replace(/^['"]/, "").replace(/['"]$/, "");
  const unique = values => [...new Set((values || []).map(v => String(v).trim()).filter(Boolean))];
  const parseVolumeMount = mount => {
    const parts = String(mount || "").split(":"), source = parts[0] || "",anonymousOptions=parts.length===2&&parts[1].split(",").every(option=>["ro","rw","z","Z"].includes(option));
    if (parts.length === 1) return { kind: "anonymous", source: "", target: source, mode: "RW" };
    if(anonymousOptions)return{kind:"anonymous",source:"",target:source,mode:parts[1].split(",").includes("ro")?"RO":"RW"};
    const options = parts.slice(2).join(":").split(",").filter(Boolean);
    return { kind: "bind", source, target: parts[1] || "", mode: options.includes("ro") ? "RO" : "RW" };
  };
  const toggleVolumeMountMode = mount => {
    const item=parseVolumeMount(mount),parts=String(mount||"").split(":"),readOnly=item.mode==="RO";
    if(item.kind==="anonymous")parts.splice(1,parts.length-1,readOnly?"rw":"ro");
    else{const options=(parts[2]||"").split(",").filter(Boolean).filter(option=>option!=="ro"&&option!=="rw");options.unshift(readOnly?"rw":"ro");parts.splice(2,parts.length-2,options.join(","));}
    return parts.join(":");
  };
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
    const model = { services: [], networks: [], volumes: [], volumeDetails: {}, networkDetails: {} };
    let section = "", service = null, volumeDef = null, nested = "", volumeNested = "";
    for (const raw of String(yaml || "").split(/\r?\n/)) {
      const line = raw.replace(/\s+#.*$/, "");
      if (!line.trim()) continue;
      const n = indent(line), text = line.trim();
      if (n === 0 && /^[\w.-]+:\s*$/.test(text)) {
        section = text.slice(0, -1); service = null; volumeDef = null; nested = ""; volumeNested = ""; continue;
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
          if (section === "volumes") { volumeDef = model.volumeDetails[name] = { name, driver: "", external: false, driverOpts: { type: "", o: "", device: "" } }; volumeNested = ""; }
        }
      } else if (section === "volumes" && volumeDef && n === 4) {
        const at=text.indexOf(":");if(at<0)continue;const key=text.slice(0,at).trim(),value=clean(text.slice(at+1));
        volumeNested=key;if(key==="driver")volumeDef.driver=value;else if(key==="external")volumeDef.external=value.toLowerCase()==="true";
      } else if(section==="volumes"&&volumeDef&&volumeNested==="driver_opts"&&n===6){
        const at=text.indexOf(":");if(at<0)continue;const key=text.slice(0,at).trim(),value=clean(text.slice(at+1));if(Object.prototype.hasOwnProperty.call(volumeDef.driverOpts,key))volumeDef.driverOpts[key]=value;
      }
    }
    const names = new Set(model.services.map(s => s.name));
    for (const item of model.services) {
      item.depends = unique(item.depends.filter(name => names.has(name)));
      item.networks = unique(item.networks.map(v => v.split(":")[0]));
      item.volumes = unique(item.volumes); item.ports = unique(item.ports);
    }
    model.networks = unique(model.networks); model.volumes = unique(model.volumes);
    const yamlLines=String(yaml||"").split(/\r?\n/);model.networks.forEach(name=>{const range=locateTopEntry(yamlLines,"networks",name),detail=model.networkDetails[name]={name,driver:"",external:false,internal:false,attachable:false,enableIpv6:false,driverOpts:{},ipam:{driver:"",subnet:"",ipRange:"",gateway:""}};if(!range||range.start<0)return;let nested="",inConfig=false;for(const line of yamlLines.slice(range.start+1,range.end)){const n=indent(line),text=line.trim(),at=text.indexOf(":");if(at<0)continue;const key=clean(text.slice(0,at)),value=clean(text.slice(at+1));if(n===4){nested=key;inConfig=false;if(key==="driver")detail.driver=value;else if(["external","internal","attachable","enable_ipv6"].includes(key))detail[key==="enable_ipv6"?"enableIpv6":key]=value.toLowerCase()==="true";}else if(n===6&&nested==="driver_opts")detail.driverOpts[key]=value;else if(n===6&&nested==="ipam"){if(key==="driver")detail.ipam.driver=value;else if(key.replace(/^-\s*/,"")==="subnet"){detail.ipam.subnet=value;inConfig=true;}}else if(n>=8&&nested==="ipam"&&inConfig){if(key==="subnet")detail.ipam.subnet=value;else if(key==="ip_range")detail.ipam.ipRange=value;else if(key==="gateway")detail.ipam.gateway=value;}}});
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
    let optsAt=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim().startsWith("driver_opts:")),optsEnd=optsAt<0?optsAt:block.length;if(optsAt>=0)for(let i=optsAt+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){optsEnd=i;break;}}
    const opts=values.driverOpts||{},optsLines=["type","o","device"].filter(key=>opts[key]).map(key=>`      ${key}: ${quote(opts[key])}`),nextOpts=optsLines.length?["    driver_opts:",...optsLines]:[];if(optsAt>=0)block.splice(optsAt,optsEnd-optsAt,...nextOpts);else if(nextOpts.length)block.push(...nextOpts);
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

  function addNetworkYaml(yaml, name) {
    const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),range=locateTopEntry(lines,"networks",name),block="  "+quote(name)+":";
    if(range&&range.start>=0)return source;
    if(!range){let at=lines.findIndex(line=>line.trim()===LAYOUT_BEGIN);if(at<0)at=lines.length;lines.splice(at,0,"networks:",block);}
    else lines.splice(range.sectionEnd,0,block);
    return lines.join(eol);
  }

  function updateNetworkYaml(yaml, oldName, values) {
    const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),range=locateTopEntry(lines,"networks",oldName);if(!range||range.start<0)return addNetworkYaml(source,values.name);const block=lines.slice(range.start,range.end);block[0]="  "+quote(values.name)+":";
    const setKey=(key,value)=>{let at=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim().startsWith(key+":")),end=at<0?at:block.length;if(at>=0)for(let i=at+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){end=i;break;}}const next=value?["    "+key+": "+value]:[];if(at>=0)block.splice(at,end-at,...next);else if(next.length)block.push(...next);};
    setKey("driver",values.driver?quote(values.driver):"");setKey("external",values.external?"true":"");setKey("internal",values.internal?"true":"");setKey("attachable",values.attachable?"true":"");setKey("enable_ipv6",values.enableIpv6?"true":"");
    const replaceBlock=(key,next)=>{let at=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim()===key+":"),end=at<0?at:block.length;if(at>=0)for(let i=at+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){end=i;break;}}if(at>=0)block.splice(at,end-at,...next);else if(next.length)block.push(...next);};
    const opts=Object.entries(values.driverOpts||{}).filter(([key,value])=>key&&value).map(([key,value])=>`      ${quote(key)}: ${quote(value)}`);replaceBlock("driver_opts",opts.length?["    driver_opts:",...opts]:[]);
    const ip=values.ipam||{},config=[ip.subnet&&`        subnet: ${quote(ip.subnet)}`,ip.ipRange&&`        ip_range: ${quote(ip.ipRange)}`,ip.gateway&&`        gateway: ${quote(ip.gateway)}`].filter(Boolean),ipLines=[ip.driver&&`      driver: ${quote(ip.driver)}`,config.length&&"      config:",config.length&&"      - "+config.shift().trim(),...config].filter(Boolean);replaceBlock("ipam",ipLines.length?["    ipam:",...ipLines]:[]);
    lines.splice(range.start,range.end-range.start,...block);return lines.join(eol);
  }

  function removeNetworkYaml(yaml,name){const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),range=locateTopEntry(lines,"networks",name);if(range&&range.start>=0)lines.splice(range.start,range.end-range.start);return lines.join(eol);}

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
      Object.assign(this, { selected: "", selectedVolume: "", selectedNetwork: "", selectedLink: null, volumePanelService: "", portPanelService: "", hoverLink: null, hoverServiceVolumeRow: null, hoverServicePortRow: null, linkFrom: "", linkTarget: "", drag: null, pointer: { x: 0, y: 0 } }, options);
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
    volumeTop() { let bottom=80;this.model.services.forEach(service=>{const b=this.serviceBox(service);bottom=Math.max(bottom,b.y+b.h);});const volumeService=this.model.services.find(service=>service.name===this.volumePanelService),portService=this.model.services.find(service=>service.name===this.portPanelService),panelService=volumeService||portService,items=volumeService?this.serviceVolumes(volumeService):(portService?portService.ports:[]);if(panelService){const handle=volumeService?this.mountHandleBox(panelService):this.portHandleBox(panelService);bottom=Math.max(bottom,handle.y+handle.h+18+(items.length+1)*26);}return bottom+72; }
    volumeBox(i) { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20)));return{x:24+(i%cols)*(VW+20),y:this.volumeTop()+Math.floor(i/cols)*50,w:VW,h:VH}; }
    networkTop() { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20))),rows=Math.max(1,Math.ceil(this.model.volumes.length/cols));return this.volumeTop()+rows*50+38; }
    networkBox(i) { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20)));return{x:24+(i%cols)*(VW+20),y:this.networkTop()+Math.floor(i/cols)*50,w:VW,h:VH}; }
    sectionAddBox(kind) { const y=(kind==="volume"?this.volumeTop():this.networkTop())-29;return{x:kind==="volume"?88:64,y,w:22,h:18}; }
    hitSectionAdd(p,kind) { const b=this.sectionAddBox(kind);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h; }
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
    mountHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2-34,y:b.y+b.h-8,w:30,h:16}; }
    portHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2+4,y:b.y+b.h-8,w:30,h:16}; }
    serviceVolumes(service) { return service&&service.volumes||[]; }
    serviceVolumeLabel(mount) { const parsed=parseVolumeMount(mount),named=this.model.volumes.includes(mount.split(":")[0]);return parsed.kind==="bind"&&!named?`${parsed.source} → ${parsed.target}`:(named?`${mount.split(":")[0]} → ${parsed.target}`:parsed.target); }
    serviceVolumePanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.mountHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=this.serviceVolumes(service).reduce((width,mount)=>Math.max(width,ctx.measureText(this.serviceVolumeLabel(mount)).width),0);ctx.restore();return Math.min(Math.max(150,Math.ceil(textWidth)+90),Math.max(150,canvasWidth-x)); }
    serviceVolumeIconAt(service,index) { const handle=this.mountHandleBox(service);return{x:handle.x+handle.w/2+15,y:handle.y+handle.h+44+index*26}; }
    hitServiceVolumeIcon(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,at:this.serviceVolumeIconAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    serviceVolumeDeleteAt(service,index) { const handle=this.mountHandleBox(service);return{x:handle.x+handle.w/2+38,y:handle.y+handle.h+44+index*26}; }
    hitServiceVolumeDelete(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,at:this.serviceVolumeDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    serviceVolumeAddAt(service) { const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,panelWidth=this.serviceVolumePanelWidth(service);return{x:x-14+panelWidth-20,y:handle.y+handle.h+18}; }
    hitServiceVolumeAdd(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;const at=this.serviceVolumeAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServiceVolumeRow(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,panelWidth=this.serviceVolumePanelWidth(service);return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,y:handle.y+handle.h+44+index*26})).find(row=>p.x>=x+48&&p.x<=x-14+panelWidth&&Math.abs(p.y-row.y)<=10)||null; }
    hitMountHandle(p) {
      return [...this.model.services].reverse().find(service=>{const b=this.mountHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null;
    }
    portPanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.portHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=(service.ports||[]).reduce((width,port)=>Math.max(width,ctx.measureText(port).width),0);ctx.restore();return Math.min(Math.max(150,Math.ceil(textWidth)+66),Math.max(150,canvasWidth-x)); }
    servicePortDeleteAt(service,index) { const handle=this.portHandleBox(service);return{x:handle.x+handle.w/2+20,y:handle.y+handle.h+44+index*26}; }
    servicePortAddAt(service) { const handle=this.portHandleBox(service),x=handle.x+handle.w/2,width=this.portPanelWidth(service);return{x:x-14+width-20,y:handle.y+handle.h+18}; }
    hitPortHandle(p) { return [...this.model.services].reverse().find(service=>{const b=this.portHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null; }
    hitServicePortAdd(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;const at=this.servicePortAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServicePortDelete(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;return (service.ports||[]).map((port,index)=>({service,port,index,at:this.servicePortDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    hitServicePortRow(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;const handle=this.portHandleBox(service),x=handle.x+handle.w/2,width=this.portPanelWidth(service);return (service.ports||[]).map((port,index)=>({service,port,index,y:handle.y+handle.h+44+index*26})).find(row=>p.x>=x+8&&p.x<=x-14+width&&Math.abs(p.y-row.y)<=10)||null; }
    hitVolume(p) {
      for (let i = 0; i < this.model.volumes.length; i += 1) { const b = this.volumeBox(i); if (p.x >= b.x && p.x <= b.x + b.w && p.y >= b.y && p.y <= b.y + b.h) return this.model.volumes[i]; }
      return "";
    }
    hitNetwork(p) { for(let i=0;i<this.model.networks.length;i+=1){const b=this.networkBox(i);if(p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h)return this.model.networks[i];}return ""; }
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
    mountLinks() { const links=[];this.model.services.forEach(service=>service.volumes.forEach(mount=>{const volume=mount.split(":")[0],link=this.mountGeometry(service.name,volume);if(link)links.push(link);}));return links; }
    volumeReferenceCount(volume) { return this.model.services.reduce((count,service)=>count+service.volumes.filter(mount=>mount.split(":")[0]===volume).length,0); }
    networkReferenceCount(network) { return this.model.services.reduce((count,service)=>count+(service.networks.includes(network)?1:0),0); }
    networkLinks() { const links=[];this.model.services.forEach(service=>service.networks.forEach(network=>{const index=this.model.networks.indexOf(network);if(index<0)return;const a=this.serviceBox(service),b=this.networkBox(index);links.push({network,points:[{x:a.x+a.w/2,y:a.y+a.h},{x:b.x+b.w/2,y:b.y}]});}));return links; }
    pointerDown(e) {
      const p = this.point(e), handle = this.hitLinkHandle(p), mountHandle=this.hitMountHandle(p), portHandle=this.hitPortHandle(p), service = this.hitService(p), volume = this.hitVolume(p),network=this.hitNetwork(p); this.pointer = p;
      if(this.hitSectionAdd(p,"volume")){this.addVolume();return;}if(this.hitSectionAdd(p,"network")){this.addNetwork();return;}
      const servicePortAdd=this.hitServicePortAdd(p);if(servicePortAdd){this.editServicePort({service:servicePortAdd,port:"",index:-1});return;}
      const servicePortDelete=this.hitServicePortDelete(p);if(servicePortDelete){this.removeServicePort(servicePortDelete);return;}
      const servicePortRow=this.hitServicePortRow(p);if(servicePortRow){this.editServicePort(servicePortRow);return;}
      const serviceVolumeAdd=this.hitServiceVolumeAdd(p);if(serviceVolumeAdd){if(typeof this.onAddMount==="function")this.onAddMount(serviceVolumeAdd.name,this.model.volumes,mount=>this.addServiceVolume(serviceVolumeAdd,mount));else this.onStatus("挂载编辑器不可用");return;}
      const serviceVolumeDelete=this.hitServiceVolumeDelete(p);if(serviceVolumeDelete){this.removeServiceVolume(serviceVolumeDelete);return;}
      const serviceVolume=this.hitServiceVolumeIcon(p);if(serviceVolume){this.toggleServiceVolumeMode(serviceVolume);return;}
      const serviceVolumeRow=this.hitServiceVolumeRow(p);if(serviceVolumeRow){this.editServiceVolume(serviceVolumeRow);return;}
      if (this.hitDelete(p)) { this.removeSelectedDependency(); return; }
      if (handle) {
        this.selectedLink = null; this.selectedVolume = "";this.selectedNetwork=""; this.volumePanelService="";this.portPanelService="";
        this.selected = handle.name; this.linkFrom = handle.name; this.linkTarget = "";
        this.drag = { type: "link", name: handle.name };
        this.canvas.style.cursor = LINK_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.onStatus(`拖到目标服务，为 ${handle.name} 添加依赖`); this.render(); return;
      }
      if(mountHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService=mountHandle.name;this.portPanelService="";this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${mountHandle.name} 的数据卷`);this.render();return;}
      if(portHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService="";this.portPanelService=portHandle.name;this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${portHandle.name} 的端口`);this.render();return;}
      if (service && this.linkFrom) {
        if (service.name !== this.linkFrom) this.addDependency(this.linkFrom, service.name);
        this.linkFrom = ""; this.onStatus("依赖连接完成"); this.render(); return;
      }
      if (service) {
        this.selected = service.name; this.selectedVolume = "";this.selectedNetwork=""; this.selectedLink = null; this.volumePanelService="";this.portPanelService=""; const pos = this.positions[service.name];
        this.drag = { type: "service", name: service.name, dx: p.x - pos.x, dy: p.y - pos.y, moved: false };
        this.canvas.style.cursor = SELECT_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.render();
      } else if (volume) {
        this.selected = ""; this.selectedVolume = volume;this.selectedNetwork=""; this.selectedLink = null; this.volumePanelService="";this.portPanelService=""; this.drag = null; this.canvas.focus(); this.renderInspector(); this.render();
      } else if(network){this.selected="";this.selectedVolume="";this.selectedNetwork=network;this.selectedLink=null;this.volumePanelService="";this.portPanelService="";this.renderInspector();this.render();
      } else {
        const link=this.hitDependency(p);this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=link;this.volumePanelService="";this.portPanelService="";this.hoverServiceVolumeRow=null;this.hoverServicePortRow=null;this.canvas.focus();this.renderInspector();
        this.onStatus(link ? `已选择依赖 ${link.from} → ${link.to}，按 Delete 删除` : ""); this.render();
      }
    }
    pointerMove(e) {
      const p = this.point(e); this.pointer = p;
      if (!this.drag) {
        const previous=this.hoverLink,previousRow=this.hoverServiceVolumeRow,previousPortRow=this.hoverServicePortRow,handle=this.hitLinkHandle(p),mountHandle=this.hitMountHandle(p),portHandle=this.hitPortHandle(p),node=this.hitService(p),volume=this.hitVolume(p),network=this.hitNetwork(p),sectionAdd=this.hitSectionAdd(p,"volume")||this.hitSectionAdd(p,"network"),serviceVolume=this.hitServiceVolumeIcon(p),serviceVolumeDelete=this.hitServiceVolumeDelete(p),serviceVolumeRow=this.hitServiceVolumeRow(p),serviceVolumeAdd=this.hitServiceVolumeAdd(p),servicePortAdd=this.hitServicePortAdd(p),servicePortDelete=this.hitServicePortDelete(p),servicePortRow=this.hitServicePortRow(p);this.hoverServiceVolumeRow=serviceVolumeRow;this.hoverServicePortRow=servicePortRow;this.hoverLink=handle||mountHandle||portHandle||node||volume||network||sectionAdd||serviceVolume||serviceVolumeDelete||serviceVolumeRow||serviceVolumeAdd||servicePortAdd||servicePortDelete||servicePortRow?null:this.hitDependency(p);
        this.canvas.style.cursor = serviceVolume||serviceVolumeDelete||serviceVolumeRow||serviceVolumeAdd||servicePortAdd||servicePortDelete||servicePortRow||mountHandle||portHandle||this.hitDelete(p)||this.hoverLink||volume||network||sectionAdd?"pointer":(handle?LINK_CURSOR:(node?SELECT_CURSOR:"default"));
        if(!this.sameLink(previous,this.hoverLink)||(previousRow&&previousRow.index)!==(serviceVolumeRow&&serviceVolumeRow.index)||(previousPortRow&&previousPortRow.index)!==(servicePortRow&&servicePortRow.index))this.render();
        return;
      }
      this.canvas.style.cursor = this.drag.type === "link" ? LINK_CURSOR : SELECT_CURSOR;
      if (this.drag.type === "link") {
        const target = this.hitService(p);
        this.linkTarget = target && target.name !== this.drag.name ? target.name : "";
      }
      if (this.drag.type === "service") {
        this.positions[this.drag.name] = { x: Math.max(24, p.x - this.drag.dx), y: Math.max(35, p.y - this.drag.dy) };
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
      } else if (this.drag.moved) {
        try { localStorage.setItem(this.storageKey, JSON.stringify(this.positions)); } catch (_) {}
        this.yaml = writeLayout(this.yaml, this.positions);
        this.onChange(this.yaml, "已更新 Canvas 布局"); this.onStatus("已更新 Canvas 布局");
      }
      this.drag = null; this.render();
      this.canvas.style.cursor = this.hitLinkHandle(p) ? LINK_CURSOR : (this.hitMountHandle(p)||this.hitPortHandle(p)||this.hitVolume(p)?"pointer":(this.hitService(p)?SELECT_CURSOR:"default"));
    }
    startLink() {
      if (!this.selected) { this.onStatus("请先点击需要添加依赖的服务"); return; }
      this.linkFrom = this.selected; this.onStatus("请点击它所依赖的目标服务"); this.render();
    }
    keyDown(e) {
      if ((e.key === "Delete" || e.key === "Backspace") && this.selectedLink) { e.preventDefault(); this.removeSelectedDependency(); }
      else if ((e.key === "Delete" || e.key === "Backspace") && this.selectedVolume) { e.preventDefault(); this.removeSelectedVolume(); }
      else if ((e.key === "Delete" || e.key === "Backspace") && this.selectedNetwork) { e.preventDefault(); this.removeSelectedNetwork(); }
      else if (e.key === "Escape" && (this.selectedLink || this.selectedVolume || this.selectedNetwork || this.volumePanelService || this.portPanelService)) { e.preventDefault(); this.selectedLink = null; this.selectedVolume = "";this.selectedNetwork=""; this.volumePanelService="";this.portPanelService=""; this.onStatus("已取消选择"); this.renderInspector(); this.render(); }
    }
    removeSelectedDependency() {
      const link=this.selectedLink;if(!link)return;
      const service=this.model.services.find(s=>s.name===link.from);if(!service)return;
      const depends=service.depends.filter(name=>name!==link.to);this.selectedLink=null;this.hoverLink=null;
      this.commit(service,{depends},`已删除依赖 ${link.from} → ${link.to}`);
    }
    toggleServiceVolumeMode(row) {
      const item=parseVolumeMount(row.mount),readOnly=item.mode==="RO",volumes=row.service.volumes.map(mount=>mount===row.mount?toggleVolumeMountMode(mount):mount);this.commit(row.service,{volumes},`${item.kind==="bind"?item.source+" → ":""}${item.target} 已切换为${readOnly?"读写":"只读"}`);
    }
    removeServiceVolume(row) {
      const volumes=row.service.volumes.filter((mount,index)=>index!==row.index);this.commit(row.service,{volumes},`已删除 ${row.service.name} 的挂载 ${row.mount}`);
    }
    editServiceVolume(row) {
      if(typeof this.onEditMount!=="function"){this.onStatus("挂载编辑器不可用");return;}const parsed=parseVolumeMount(row.mount),named=this.model.volumes.includes(row.mount.split(":")[0]),initial={editing:true,type:named?"named":parsed.kind,source:named?row.mount.split(":")[0]:parsed.source,target:parsed.target,mode:parsed.mode.toLowerCase()};this.onEditMount(row.service.name,this.model.volumes,mount=>{const volumes=row.service.volumes.map((value,index)=>index===row.index?mount:value);this.commit(row.service,{volumes},`已更新 ${row.service.name} 的挂载 ${mount}`);},initial);
    }
    removeServicePort(row) { const ports=row.service.ports.filter((port,index)=>index!==row.index);this.commit(row.service,{ports},`已删除 ${row.service.name} 的端口 ${row.port}`); }
    editServicePort(row) { if(typeof this.onEditPort!=="function"){this.onStatus("端口编辑器不可用");return;}this.onEditPort(row.service.name,row.port,port=>{const ports=row.index<0?[...row.service.ports,port]:row.service.ports.map((value,index)=>index===row.index?port:value);this.commit(row.service,{ports:unique(ports)},`${row.index<0?"已添加":"已更新"} ${row.service.name} 的端口 ${port}`);}); }
    addDependency(from, to) {
      const service = this.model.services.find(s => s.name === from);
      if (service && !service.depends.includes(to)) { service.depends.push(to); this.commit(service, { depends: service.depends }, `已添加依赖 ${from} → ${to}`); }
    }
    addVolume() {
      let index=1,name="volume-1";while(this.model.volumes.includes(name))name="volume-"+(++index);
      this.yaml=updateVolumeYaml(this.yaml,"",{name,driver:"local",external:false});this.model=parse(this.yaml);this.selected="";this.selectedVolume=name;
      this.onChange(this.yaml,`已添加命名卷 ${name}`);this.onStatus(`已添加命名卷 ${name}`);this.renderInspector();this.render();
    }
    addNetwork() { let index=1,name="network-1";while(this.model.networks.includes(name))name="network-"+(++index);this.yaml=addNetworkYaml(this.yaml,name);this.model=parse(this.yaml);this.selected="";this.selectedVolume="";this.selectedNetwork=name;this.onChange(this.yaml,`已添加网络 ${name}`);this.onStatus(`已添加网络 ${name}`);this.renderInspector();this.render(); }
    updateSelectedNetwork(values) { const oldName=this.selectedNetwork,newName=String(values.name||"").trim();if(!oldName)return;if(!/^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(newName)){this.onStatus("网络名称只能包含字母、数字、点、下划线和连字符");return;}if(newName!==oldName&&this.model.networks.includes(newName)){this.onStatus(`网络 ${newName} 已存在`);return;}let yaml=updateNetworkYaml(this.yaml,oldName,{...values,name:newName});if(newName!==oldName)this.model.services.forEach(service=>{const networks=service.networks.map(name=>name===oldName?newName:name);if(JSON.stringify(networks)!==JSON.stringify(service.networks))yaml=updateServiceYaml(yaml,service.name,{networks});});this.yaml=yaml;this.model=parse(yaml);this.selectedNetwork=newName;this.onChange(yaml,`已更新网络 ${newName}`);this.onStatus(`已更新网络 ${newName}`);this.renderInspector();this.render(); }
    removeSelectedNetwork() { const name=this.selectedNetwork;if(!name)return;const affected=this.model.services.filter(service=>service.networks.includes(name)),suffix=affected.length?`\n同时会移除以下服务的网络引用：${affected.map(service=>service.name).join("、")}`:"";if(!global.confirm(`确定删除网络 ${name}？${suffix}`))return;let yaml=removeNetworkYaml(this.yaml,name);affected.forEach(service=>{yaml=updateServiceYaml(yaml,service.name,{networks:service.networks.filter(value=>value!==name)});});this.yaml=yaml;this.model=parse(yaml);this.selectedNetwork="";this.onChange(yaml,`已删除网络 ${name}`);this.onStatus(`已删除网络 ${name}`);this.renderInspector();this.render(); }
    updateSelectedVolume(values) {
      const oldName=this.selectedVolume,newName=String(values.name||"").trim();if(!oldName)return;
      if(!/^[A-Za-z0-9][A-Za-z0-9_.-]*$/.test(newName)){this.onStatus("卷名称只能包含字母、数字、点、下划线和连字符");return;}
      if(newName!==oldName&&this.model.volumes.includes(newName)){this.onStatus(`命名卷 ${newName} 已存在`);return;}
      const volumeType=["nfs","cifs"].includes(values.volumeType)?values.volumeType:"local",device=String(values.device||"").trim(),options=String(values.options||"").trim();if(volumeType!=="local"&&!device){this.onStatus(`${volumeType.toUpperCase()} 需要填写远程路径`);return;}const driverOpts=device?{type:volumeType==="local"?"none":volumeType,o:options||(volumeType==="local"?"bind":""),device}:{type:"",o:"",device:""};
      let yaml=updateVolumeYaml(this.yaml,oldName,{name:newName,driver:"local",driverOpts,external:!!values.external});
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
    commit(service, values, message) {
      this.yaml = updateServiceYaml(this.yaml, service.name, values); this.model = parse(this.yaml);
      this.onChange(this.yaml, message); this.onStatus(message); this.renderInspector(); this.render();
    }
    addServiceVolume(service, mount) {
      mount=String(mount||"").trim();if(!mount)return;if(service.volumes.includes(mount)){this.onStatus(`挂载 ${mount} 已存在`);return;}
      this.commit(service,{volumes:[...service.volumes,mount]},`已为 ${service.name} 添加挂载 ${mount}`);
    }
    renderInspector() {
      if(this.selectedNetwork){const network=this.model.networkDetails[this.selectedNetwork]||{name:this.selectedNetwork,driver:"",external:false,internal:false,attachable:false,enableIpv6:false,driverOpts:{},ipam:{}},ip=network.ipam||{},opts=Object.entries(network.driverOpts||{}).map(([key,value])=>`${key}=${value}`).join("\n"),checked=value=>value?"checked":"";this.inspector.innerHTML=`<div class="compose-inspector-title">网络 ${html(network.name)}</div><label>网络名称<input data-network-name type="text" value="${html(network.name)}" /></label><label>Driver<select data-network-driver><option value="" ${!network.driver?"selected":""}>默认（bridge）</option>${["bridge","overlay","macvlan","ipvlan","none"].map(driver=>`<option value="${driver}" ${network.driver===driver?"selected":""}>${driver}</option>`).join("")}</select></label><label>Driver options（key=value，每行一个）<textarea data-network-options rows="3">${html(opts)}</textarea></label><label>IPAM Driver<input data-network-ipam-driver type="text" value="${html(ip.driver||"")}" placeholder="default" /></label><label>Subnet<input data-network-subnet type="text" value="${html(ip.subnet||"")}" placeholder="172.28.0.0/16" /></label><label>IP range<input data-network-range type="text" value="${html(ip.ipRange||"")}" placeholder="172.28.5.0/24" /></label><label>Gateway<input data-network-gateway type="text" value="${html(ip.gateway||"")}" placeholder="172.28.0.1" /></label><label style="flex-direction:row;align-items:center"><input data-network-internal type="checkbox" style="width:auto" ${checked(network.internal)} /> Internal</label><label style="flex-direction:row;align-items:center"><input data-network-attachable type="checkbox" style="width:auto" ${checked(network.attachable)} /> Attachable</label><label style="flex-direction:row;align-items:center"><input data-network-ipv6 type="checkbox" style="width:auto" ${checked(network.enableIpv6)} /> Enable IPv6</label><label style="flex-direction:row;align-items:center"><input data-network-external type="checkbox" style="width:auto" ${checked(network.external)} /> External network</label><div style="display:flex;gap:8px"><button type="button" class="btn primary" data-network-apply>应用到草稿</button><button type="button" class="btn danger" data-network-delete>删除</button></div>`;this.inspector.querySelector("[data-network-apply]").onclick=()=>{const driverOpts={};this.inspector.querySelector("[data-network-options]").value.split(/\r?\n/).forEach(line=>{const at=line.indexOf("=");if(at>0)driverOpts[line.slice(0,at).trim()]=line.slice(at+1).trim();});this.updateSelectedNetwork({name:this.inspector.querySelector("[data-network-name]").value,driver:this.inspector.querySelector("[data-network-driver]").value,driverOpts,external:this.inspector.querySelector("[data-network-external]").checked,internal:this.inspector.querySelector("[data-network-internal]").checked,attachable:this.inspector.querySelector("[data-network-attachable]").checked,enableIpv6:this.inspector.querySelector("[data-network-ipv6]").checked,ipam:{driver:this.inspector.querySelector("[data-network-ipam-driver]").value.trim(),subnet:this.inspector.querySelector("[data-network-subnet]").value.trim(),ipRange:this.inspector.querySelector("[data-network-range]").value.trim(),gateway:this.inspector.querySelector("[data-network-gateway]").value.trim()}});};this.inspector.querySelector("[data-network-delete]").onclick=()=>this.removeSelectedNetwork();return;}
      if (this.selectedVolume) {
        const volume=this.model.volumeDetails[this.selectedVolume]||{name:this.selectedVolume,driver:"local",external:false,driverOpts:{type:"",o:"",device:""}},opts=volume.driverOpts||{},volumeType=["nfs","cifs"].includes(opts.type)?opts.type:"local";
        this.inspector.innerHTML=`<div class="compose-inspector-title">命名卷 ${html(volume.name)}</div><label>卷名称<input data-volume-name type="text" value="${html(volume.name)}" /></label><label>Driver<select data-volume-type><option value="local" ${volumeType==="local"?"selected":""}>local</option><option value="nfs" ${volumeType==="nfs"?"selected":""}>nfs</option><option value="cifs" ${volumeType==="cifs"?"selected":""}>cifs</option></select></label><div data-volume-params></div><label style="flex-direction:row;align-items:center"><input data-volume-external type="checkbox" style="width:auto" ${volume.external?"checked":""} /> External volume</label><div style="display:flex;gap:8px"><button type="button" class="btn primary" data-volume-apply>应用到草稿</button><button type="button" class="btn danger" data-volume-delete>删除</button></div>`;
        const renderParams=(type,initial)=>{const defaults=type==="nfs"?{device:":/exports/data",options:"addr=127.0.0.1,rw,nfsvers=4"}:(type==="cifs"?{device:"//server/share",options:"username=user,password=secret,vers=3.0"}:{device:"/srv/data",options:"bind"}),labels=type==="local"?{device:"宿主机目录（可选）",options:"挂载选项"}:{device:type==="nfs"?"NFS 远程路径":"CIFS 共享路径",options:"连接与挂载选项"},values=initial?{device:opts.device||"",options:opts.o||""}:{device:"",options:""},deviceInput=`<input id="compose-volume-device" data-volume-device type="text" value="${html(values.device)}" placeholder="${html(defaults.device)}" />`;this.inspector.querySelector("[data-volume-params]").innerHTML=`<label>${labels.device}${type==="local"?`<div class="dir-row">${deviceInput}<button type="button" class="btn" data-volume-browse>选择</button></div>`:deviceInput}</label><label>${labels.options}<input data-volume-options type="text" value="${html(values.options)}" placeholder="${html(defaults.options)}" /></label>`;const browse=this.inspector.querySelector("[data-volume-browse]");if(browse)browse.onclick=()=>{if(typeof global.openDirPicker==="function")global.openDirPicker("compose-volume-device",()=>{});else this.onStatus("目录选择器不可用");};};
        renderParams(volumeType,true);this.inspector.querySelector("[data-volume-type]").onchange=e=>renderParams(e.target.value,false);
        this.inspector.querySelector("[data-volume-apply]").onclick=()=>this.updateSelectedVolume({name:this.inspector.querySelector("[data-volume-name]").value,volumeType:this.inspector.querySelector("[data-volume-type]").value,device:this.inspector.querySelector("[data-volume-device]").value,options:this.inspector.querySelector("[data-volume-options]").value,external:this.inspector.querySelector("[data-volume-external]").checked});
        this.inspector.querySelector("[data-volume-delete]").onclick=()=>this.removeSelectedVolume();return;
      }
      const service = this.model.services.find(s => s.name === this.selected);
      if (!service) { this.inspector.innerHTML = '<div class="compose-inspector-empty">点击服务节点编辑属性</div>'; return; }
      const field = (label, key, value, area) => `<label>${label}${area ? `<textarea data-field="${key}" rows="3">${html((value || []).join("\n"))}</textarea>` : `<input data-field="${key}" type="text" value="${html(value || "")}" />`}</label>`;
      const restart=service.restart||"unless-stopped",restartField=`<label>重启策略<select data-field="restart"><option value="no" ${restart==="no"?"selected":""}>no</option><option value="always" ${restart==="always"?"selected":""}>always</option><option value="on-failure" ${restart==="on-failure"?"selected":""}>on-failure</option><option value="unless-stopped" ${restart==="unless-stopped"?"selected":""}>unless-stopped</option></select></label>`;
      this.inspector.innerHTML = `<div class="compose-inspector-title">${html(service.name)}</div>${field("镜像","image",service.image)}${field("容器名称","containerName",service.containerName)}${field("启动命令","command",service.command)}${restartField}${field("端口（每行一个）","ports",service.ports,true)}${field("网络（每行一个）","networks",service.networks,true)}<button type="button" class="btn primary" data-apply>应用到草稿</button>`;
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
      this.model.networks.forEach((v,i)=>{const b=this.networkBox(i);height=Math.max(height,b.y+b.h+32);});
      height=Math.max(height,this.networkTop()+28);
      const dpr = Math.max(1, global.devicePixelRatio || 1); this.canvas.width = width * dpr; this.canvas.height = height * dpr; this.canvas.style.width = width + "px"; this.canvas.style.height = height + "px";
      const ctx = this.canvas.getContext("2d"); ctx.scale(dpr, dpr);
      const c = { bg: color("--bg-2","#161b22"), card: color("--bg-3","#21262d"), text: color("--text","#f0f6fc"), muted: color("--muted","#8b949e"), line: color("--line","#30363d"), accent: color("--accent","#58a6ff"), active: color("--bg-active","#1f6feb33"), volume:"#9a6700", volumeActive:"rgba(154,103,0,.16)", port:"#1a7f37", portActive:"rgba(26,127,55,.16)", network:"#8250df", networkActive:"rgba(130,80,223,.16)" };
      ctx.fillStyle=c.bg;ctx.fillRect(0,0,width,height);this.drawSectionHeader(ctx,"命名卷","volume",c.volume);this.drawSectionHeader(ctx,"网络","network",c.network);
      this.drawMounts(ctx,c);this.drawNetworkLinks(ctx,c);this.drawLinks(ctx,c);this.model.volumes.forEach((v,i)=>this.drawVolume(ctx,v,i,c));this.model.networks.forEach((v,i)=>this.drawNetwork(ctx,v,i,c));this.model.services.forEach(s=>this.drawService(ctx,s,c));
      if (this.drag && this.drag.type === "link") {
        const service=this.model.services.find(s=>s.name===this.drag.name),b=service&&this.serviceBox(service);
        if(b){ctx.save();ctx.strokeStyle=c.accent;ctx.lineWidth=1.5;ctx.setLineDash([5,4]);ctx.beginPath();ctx.moveTo(b.x,b.y+b.h/2);ctx.lineTo(b.x-15,b.y+b.h/2);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore();}
      }
      this.drawServiceVolumes(ctx,c);this.drawServicePorts(ctx,c);
    }
    drawMounts(ctx,c) {
      if (!this.selectedVolume) return;
      this.mountLinks().filter(link=>link.volume===this.selectedVolume).forEach(link=>{ctx.save();ctx.strokeStyle=c.muted;ctx.lineWidth=1.5;ctx.setLineDash([6,5]);ctx.beginPath();ctx.moveTo(link.points[0].x,link.points[0].y);ctx.lineTo(link.points[1].x,link.points[1].y);ctx.stroke();ctx.restore();});
    }
    drawNetworkLinks(ctx,c) { if(!this.selectedNetwork)return;this.networkLinks().filter(link=>link.network===this.selectedNetwork).forEach(link=>{ctx.save();ctx.strokeStyle=c.muted;ctx.lineWidth=1.5;ctx.setLineDash([6,5]);ctx.beginPath();ctx.moveTo(link.points[0].x,link.points[0].y);ctx.lineTo(link.points[1].x,link.points[1].y);ctx.stroke();ctx.restore();}); }
    drawSectionHeader(ctx,label,kind,colorValue) { const top=kind==="volume"?this.volumeTop():this.networkTop(),button=this.sectionAddBox(kind);ctx.fillStyle=colorValue;ctx.font="600 12px system-ui";ctx.textBaseline="alphabetic";ctx.fillText(label,24,top-20);roundRect(ctx,button.x,button.y,button.w,button.h,5);ctx.strokeStyle=colorValue;ctx.lineWidth=1.25;ctx.stroke();ctx.beginPath();ctx.moveTo(button.x+7,button.y+9);ctx.lineTo(button.x+15,button.y+9);ctx.moveTo(button.x+11,button.y+5);ctx.lineTo(button.x+11,button.y+13);ctx.stroke(); }
    drawServiceVolumes(ctx,c) {
      c={...c,accent:c.volume,active:c.volumeActive,muted:c.volume};
      const service=this.model.services.find(item=>item.name===this.volumePanelService),items=this.serviceVolumes(service);if(!service)return;
      const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h,lastY=items.length?start+44+(items.length-1)*26:start,panelWidth=this.serviceVolumePanelWidth(service),headerY=start+18;
      ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,panelWidth,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle="#d0d7de";ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+panelWidth,start+31);ctx.stroke();ctx.fillStyle="#57606a";ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("数据卷",x+2,headerY);const add=this.serviceVolumeAddAt(service);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.fillStyle="#f6f8fa";ctx.fill();ctx.strokeStyle="#d0d7de";ctx.stroke();ctx.strokeStyle="#0969da";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.lineWidth=1.25;if(items.length){ctx.beginPath();ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);ctx.stroke();}ctx.textBaseline="middle";
      items.forEach((mount,index)=>{const parsed=parseVolumeMount(mount),named=this.model.volumes.includes(mount.split(":")[0]),item=named?{...parsed,kind:"named"}:parsed,y=start+44+index*26,label=this.serviceVolumeLabel(mount),hovered=this.hoverServiceVolumeRow&&this.hoverServiceVolumeRow.index===index;ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.fillStyle=item.mode==="RO"?"#57606a":c.accent;if(item.kind==="named"){const cx=x+20,size=7;ctx.beginPath();ctx.moveTo(cx,y-size);ctx.lineTo(cx+size,y);ctx.lineTo(cx,y+size);ctx.lineTo(cx-size,y);ctx.closePath();if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}else if(item.kind==="bind"){const size=11;ctx.beginPath();ctx.rect(x+20-size/2,y-size/2,size,size);if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}else{ctx.beginPath();ctx.arc(x+20,y,6,0,Math.PI*2);if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}const remove=this.serviceVolumeDeleteAt(service,index);ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,remove.y-4);ctx.lineTo(remove.x+4,remove.y+4);ctx.moveTo(remove.x+4,remove.y-4);ctx.lineTo(remove.x-4,remove.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,label,panelWidth-82),x+52,y);});ctx.restore();
    }
    drawServicePorts(ctx,c) {
      c={...c,accent:c.port,active:c.portActive,muted:c.port};
      const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return;const items=service.ports||[],handle=this.portHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h,lastY=items.length?start+44+(items.length-1)*26:start,width=this.portPanelWidth(service),headerY=start+18;
      ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle="#d0d7de";ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);ctx.stroke();ctx.fillStyle="#57606a";ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("端口",x+2,headerY);const add=this.servicePortAddAt(service);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.fillStyle="#f6f8fa";ctx.fill();ctx.strokeStyle="#d0d7de";ctx.stroke();ctx.strokeStyle="#0969da";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.lineWidth=1.25;if(items.length){ctx.beginPath();ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);ctx.stroke();}items.forEach((port,index)=>{const y=start+44+index*26,hovered=this.hoverServicePortRow&&this.hoverServicePortRow.index===index,remove=this.servicePortDeleteAt(service,index);ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,y-4);ctx.lineTo(remove.x+4,y+4);ctx.moveTo(remove.x+4,y-4);ctx.lineTo(remove.x-4,y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,port,width-55),x+34,y);});ctx.restore();
    }
    drawLinks(ctx,c) {
      this.model.services.forEach(sourceService => sourceService.depends.forEach(name => {
        const link=this.dependencyGeometry(sourceService.name,name);if(!link)return;const active=this.sameLink(link,this.selectedLink)||this.sameLink(link,this.hoverLink),points=link.points,target=points[points.length-1];
        ctx.save();ctx.strokeStyle=c.accent;ctx.fillStyle=c.accent;ctx.lineWidth=active?3:1.5;if(active){ctx.shadowColor=c.accent;ctx.shadowBlur=7;}ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);points.slice(1).forEach(point=>ctx.lineTo(point.x,point.y));ctx.stroke();ctx.beginPath();ctx.moveTo(target.x,target.y);ctx.lineTo(target.x-Math.cos(link.arrowAng-.45)*9,target.y-Math.sin(link.arrowAng-.45)*9);ctx.lineTo(target.x-Math.cos(link.arrowAng+.45)*9,target.y-Math.sin(link.arrowAng+.45)*9);ctx.closePath();ctx.fill();ctx.restore();
      }));
      if(this.selectedLink){const at=this.linkMidpoint(this.selectedLink);ctx.save();ctx.beginPath();ctx.arc(at.x,at.y,10,0,Math.PI*2);ctx.fillStyle="#d1242f";ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.stroke();ctx.strokeStyle="#fff";ctx.lineWidth=1.7;ctx.beginPath();ctx.moveTo(at.x-3.5,at.y-3.5);ctx.lineTo(at.x+3.5,at.y+3.5);ctx.moveTo(at.x+3.5,at.y-3.5);ctx.lineTo(at.x-3.5,at.y+3.5);ctx.stroke();ctx.restore();}
    }
    drawService(ctx,s,c) {
      const b=this.serviceBox(s),isTarget=s.name===this.linkTarget,linked=s.volumes.some(mount=>this.model.volumes.includes(mount.split(":")[0])),panelOpen=s.name===this.volumePanelService,portsOpen=s.name===this.portPanelService;ctx.save();if(isTarget){ctx.shadowColor=c.accent;ctx.shadowBlur=12;}roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=(s.name===this.selected||isTarget)?c.active:c.card;ctx.fill();ctx.strokeStyle=(s.name===this.selected||s.name===this.linkFrom||isTarget||linked)?c.accent:c.line;ctx.lineWidth=isTarget?3:((s.name===this.selected||s.name===this.linkFrom)?2:1);ctx.stroke();ctx.restore();ctx.fillStyle=c.text;ctx.font="600 14px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(clip(ctx,s.name,b.w-24),b.x+b.w/2,b.y+b.h/2);ctx.textAlign="left";ctx.textBaseline="alphabetic";
      ctx.beginPath();ctx.arc(b.x,b.y+b.h/2,6,0,Math.PI*2);ctx.fillStyle=c.bg;ctx.fill();ctx.strokeStyle=c.accent;ctx.lineWidth=2;ctx.stroke();
      const mh=this.mountHandleBox(s),cx=mh.x+mh.w/2,cy=mh.y+mh.h/2;roundRect(ctx,mh.x,mh.y,mh.w,mh.h,3);ctx.fillStyle=panelOpen?c.volumeActive:c.card;ctx.fill();ctx.strokeStyle=c.volume;ctx.lineWidth=panelOpen?2:1.5;ctx.stroke();ctx.strokeStyle=c.volume;ctx.lineWidth=1.1;ctx.beginPath();ctx.ellipse(cx,cy-3,5,2,0,0,Math.PI*2);ctx.moveTo(cx-5,cy-3);ctx.lineTo(cx-5,cy+3);ctx.ellipse(cx,cy+3,5,2,0,0,Math.PI);ctx.lineTo(cx+5,cy-3);ctx.stroke();ctx.textAlign="left";ctx.textBaseline="alphabetic";
      const ph=this.portHandleBox(s),px=ph.x+ph.w/2,py=ph.y+ph.h/2;roundRect(ctx,ph.x,ph.y,ph.w,ph.h,3);ctx.fillStyle=portsOpen?c.portActive:c.card;ctx.fill();ctx.strokeStyle=c.port;ctx.lineWidth=portsOpen?2:1.5;ctx.stroke();ctx.strokeStyle=c.port;ctx.lineWidth=1.2;ctx.beginPath();ctx.arc(px-4,py,2.5,0,Math.PI*2);ctx.moveTo(px-1.5,py);ctx.lineTo(px+5,py);ctx.moveTo(px+2,py-3);ctx.lineTo(px+5,py);ctx.lineTo(px+2,py+3);ctx.stroke();
    }
    drawVolume(ctx,v,i,c) { const b=this.volumeBox(i),active=v===this.selectedVolume,count=this.volumeReferenceCount(v),cy=b.y+b.h/2;ctx.save();roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=active?c.volumeActive:c.card;ctx.fill();ctx.strokeStyle=c.volume;ctx.lineWidth=active?2.5:1;ctx.stroke();ctx.strokeStyle=c.volume;ctx.lineWidth=1.2;ctx.beginPath();ctx.ellipse(b.x+15,cy-4,6,2.5,0,0,Math.PI*2);ctx.moveTo(b.x+9,cy-4);ctx.lineTo(b.x+9,cy+4);ctx.ellipse(b.x+15,cy+4,6,2.5,0,0,Math.PI);ctx.lineTo(b.x+21,cy-4);ctx.stroke();ctx.beginPath();ctx.arc(b.x+b.w-15,cy,9,0,Math.PI*2);ctx.fillStyle=c.volume;ctx.fill();ctx.fillStyle=c.text;ctx.font="600 10px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(String(count),b.x+b.w-15,cy);ctx.font="12px ui-monospace";ctx.fillText(clip(ctx,v,b.w-62),b.x+b.w/2,cy);ctx.restore();ctx.textAlign="left";ctx.textBaseline="alphabetic"; }
    drawNetwork(ctx,v,i,c) { const b=this.networkBox(i),active=v===this.selectedNetwork,count=this.networkReferenceCount(v),cy=b.y+b.h/2;ctx.save();roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=active?c.networkActive:c.card;ctx.fill();ctx.strokeStyle=c.network;ctx.lineWidth=active?2.5:1;ctx.stroke();ctx.beginPath();ctx.arc(b.x+15,cy,7,0,Math.PI*2);ctx.stroke();ctx.beginPath();ctx.arc(b.x+15,cy,2,0,Math.PI*2);ctx.fillStyle=c.network;ctx.fill();ctx.beginPath();ctx.arc(b.x+b.w-15,cy,9,0,Math.PI*2);ctx.fill();ctx.fillStyle=c.text;ctx.font="600 10px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(String(count),b.x+b.w-15,cy);ctx.font="12px ui-monospace";ctx.fillText(clip(ctx,v,b.w-62),b.x+b.w/2,cy);ctx.restore();ctx.textAlign="left";ctx.textBaseline="alphabetic"; }
  }

  global.ComposeCanvas = { parse, parseVolumeMount, toggleVolumeMountMode, readLayout, writeLayout, updateServiceYaml, updateVolumeYaml, removeVolumeYaml, updateNetworkYaml, removeNetworkYaml, create: options => new Editor(options) };
})(window);
