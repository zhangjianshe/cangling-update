(function (global) {
  "use strict";
  const SW = 188, SH = 66, VW = 148, VH = 34;
  const LAYOUT_BEGIN = "# cangling-canvas-layout:begin";
  const LAYOUT_END = "# cangling-canvas-layout:end";
  const NOTE_BEGIN = "# cangling-compose-note:begin", NOTE_END = "# cangling-compose-note:end";
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
  function readHeaderNote(yaml){const lines=String(yaml||"").split(/\r?\n/),begin=lines.findIndex(line=>line.trim()===NOTE_BEGIN),end=lines.findIndex((line,index)=>index>begin&&line.trim()===NOTE_END);return begin>=0&&end>begin?lines.slice(begin+1,end).map(line=>line.replace(/^\s*#\s?/,"")).join("\n"):"";}
  function writeHeaderNote(yaml,note){const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),begin=lines.findIndex(line=>line.trim()===NOTE_BEGIN),end=lines.findIndex((line,index)=>index>begin&&line.trim()===NOTE_END);if(begin>=0)lines.splice(begin,(end>=0?end:begin)-begin+1);const text=String(note||"").trim(),block=text?[NOTE_BEGIN,...text.split(/\r?\n/).map(line=>"# "+line),NOTE_END,""]:[];lines.unshift(...block);return lines.join(eol);}

  function parse(yaml) {
    const model = { services: [], networks: [], volumes: [], volumeDetails: {}, networkDetails: {} };
    let section = "", service = null, volumeDef = null, nested = "", volumeNested = "", serviceNetwork = "", healthNested = "";
    for (const raw of String(yaml || "").split(/\r?\n/)) {
      const line = raw.replace(/\s+#.*$/, "");
      if (!line.trim()) continue;
      const n = indent(line), text = line.trim();
      if (n === 0 && /^[\w.-]+:\s*$/.test(text)) {
        section = text.slice(0, -1); service = null; volumeDef = null; nested = ""; volumeNested = ""; continue;
      }
      if (section === "services" && n === 2 && /^[^:]+:\s*$/.test(text)) {
        service = { name: clean(text.slice(0, -1)), image: "", containerName: "", command: "", restart: "", user: "", healthcheck: null, depends: [], networks: [], networkIps: {}, volumes: [], ports: [], environment: [], envFiles: [] };
        model.services.push(service); nested = ""; continue;
      }
      if (section === "services" && service) {
        if (n === 4) {
          const at = text.indexOf(":");
          if (at < 0) continue;
          const key = text.slice(0, at).trim(), value = clean(text.slice(at + 1));
          nested = key;serviceNetwork="";healthNested="";
          if (key === "image") service.image = value;
          else if (key === "container_name") service.containerName = value;
          else if (key === "command") service.command = value;
          else if (key === "restart") service.restart = value;
          else if (key === "user") service.user = value;
          else if (key === "healthcheck") service.healthcheck = { test: [], interval: "", timeout: "", retries: "", startPeriod: "", startInterval: "", disable: false };
          else if (key === "depends_on") service.depends.push(...inlineList(value));
          else if (key === "networks") service.networks.push(...inlineList(value));
          else if (key === "volumes") service.volumes.push(...inlineList(value));
          else if (key === "ports") service.ports.push(...inlineList(value));
          else if (key === "environment") service.environment.push(...inlineList(value));
          else if (key === "env_file") { const files=inlineList(value);service.envFiles.push(...(files.length?files:(value?[value]:[]))); }
          continue;
        }
        if (n >= 6 && text.startsWith("- ")) {
          const value = clean(text.slice(2));
          if (nested === "depends_on") service.depends.push(value);
          else if (nested === "networks") service.networks.push(value);
          else if (nested === "volumes") service.volumes.push(value);
          else if (nested === "ports") service.ports.push(value);
          else if (nested === "environment") service.environment.push(value);
          else if (nested === "env_file") service.envFiles.push(value);
          else if (nested === "healthcheck" && healthNested === "test" && service.healthcheck) service.healthcheck.test.push(value);
        } else if(n===6&&nested==="environment") { const at=text.indexOf(":");if(at>0){const key=clean(text.slice(0,at)),value=clean(text.slice(at+1));service.environment.push(key+(value?"="+value:""));}
        } else if(n===6&&nested==="healthcheck"&&service.healthcheck) { const at=text.indexOf(":");if(at>0){const key=text.slice(0,at).trim(),value=clean(text.slice(at+1));healthNested=key;if(key==="test")service.healthcheck.test=inlineList(value).length?inlineList(value):(value?["CMD-SHELL",value]:[]);else if(key==="interval")service.healthcheck.interval=value;else if(key==="timeout")service.healthcheck.timeout=value;else if(key==="retries")service.healthcheck.retries=value;else if(key==="start_period")service.healthcheck.startPeriod=value;else if(key==="start_interval")service.healthcheck.startInterval=value;else if(key==="disable")service.healthcheck.disable=value.toLowerCase()==="true";}
        } else if (n === 6 && (nested === "depends_on" || nested === "networks")) {
          const key = clean(text.split(":", 1)[0]);
          if (key) { service[nested === "depends_on" ? "depends" : "networks"].push(key);if(nested==="networks")serviceNetwork=key; }
        } else if(n===8&&nested==="networks"&&serviceNetwork){const at=text.indexOf(":");if(at>0&&text.slice(0,at).trim()==="ipv4_address")service.networkIps[serviceNetwork]=clean(text.slice(at+1));
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
      ["command", "command", false], ["restart", "restart", false], ["user", "user", false],
      ["depends_on", "depends", true], ["ports", "ports", true],
      ["volumes", "volumes", true], ["networks", "networks", true], ["environment", "environment", true], ["env_file", "envFiles", true],
    ];
    fields.forEach(([yamlKey, field, list]) => {
      if (Object.prototype.hasOwnProperty.call(values, field)) replaceKey(block, yamlKey, values[field], list);
    });
    if(Object.prototype.hasOwnProperty.call(values,"healthcheck")){let start=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim().startsWith("healthcheck:")),end=start<0?start:block.length;if(start>=0)for(let i=start+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){end=i;break;}}const health=values.healthcheck,next=[];if(health){next.push("    healthcheck:");const test=health.test||[];if(test.length)next.push("      test: ["+test.map(value=>JSON.stringify(String(value))).join(", ")+"]");if(health.interval)next.push("      interval: "+quote(health.interval));if(health.timeout)next.push("      timeout: "+quote(health.timeout));if(health.retries)next.push("      retries: "+String(health.retries));if(health.startPeriod)next.push("      start_period: "+quote(health.startPeriod));if(health.startInterval)next.push("      start_interval: "+quote(health.startInterval));if(health.disable)next.push("      disable: true");}if(start>=0)block.splice(start,end-start,...next);else if(next.length)block.splice(1,0,...next);}
    if(Object.prototype.hasOwnProperty.call(values,"networkIps")){let start=block.findIndex((line,index)=>index>0&&indent(line)===4&&line.trim().startsWith("networks:")),end=start<0?start:block.length;if(start>=0)for(let i=start+1;i<block.length;i+=1){if(block[i].trim()&&indent(block[i])<=4){end=i;break;}}const networks=unique(values.networks||[]),ips=values.networkIps||{},next=networks.length?["    networks:",...networks.flatMap(network=>ips[network]?[`      ${quote(network)}:`,`        ipv4_address: ${quote(ips[network])}`]:[`      ${quote(network)}:`])]:[];if(start>=0)block.splice(start,end-start,...next);else if(next.length)block.splice(1,0,...next);}
    lines.splice(range.start, range.end - range.start, ...block);
    let result = lines.join("\n");
    if (hadNewline && !result.endsWith("\n")) result += "\n";
    return result;
  }
  function addServiceYaml(yaml,name){const source=String(yaml||""),eol=source.includes("\r\n")?"\r\n":"\n",lines=source.split(/\r?\n/),at=lines.findIndex(line=>indent(line)===0&&line.trim()==="services:");const block=["  "+quote(name)+":","    image: alpine:latest","    restart: unless-stopped"];if(at<0){let insert=lines.findIndex(line=>line.trim()===LAYOUT_BEGIN);if(insert<0)insert=lines.length;lines.splice(insert,0,"services:",...block);}else{let end=lines.length;for(let i=at+1;i<lines.length;i+=1){if(lines[i].trim()&&indent(lines[i])===0){end=i;break;}}lines.splice(end,0,...block);}return lines.join(eol);}

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
      Object.assign(this, { selected: "", selectedVolume: "", selectedNetwork: "", selectedLink: null, envFileSelected: false, envFileTarget: "", envFileMenuService: "", volumePanelService: "", portPanelService: "", networkPanelService: "", environmentPanelService: "", hoverLink: null, hoverServiceVolumeRow: null, hoverServicePortRow: null, hoverServiceNetworkRow: null, hoverServiceEnvironmentRow: null, linkFrom: "", linkTarget: "", drag: null, pointer: { x: 0, y: 0 }, panX: 0, panY: 0, zoom: 1 }, options);
      this.yaml = String(options.yaml || ""); this.model = parse(this.yaml);
      this.positions = readLayout(this.yaml);
      if (!this.positions) {
        try { this.positions = JSON.parse(localStorage.getItem(this.storageKey) || "{}"); } catch (_) { this.positions = {}; }
      }
      this.down = e => this.pointerDown(e); this.move = e => this.pointerMove(e); this.up = e => this.pointerUp(e); this.key = e => this.keyDown(e);this.menu=e=>e.preventDefault();this.wheel=e=>this.zoomAt(e);
      this.canvas.tabIndex = 0;
      this.canvas.addEventListener("pointerdown", this.down); this.canvas.addEventListener("pointermove", this.move);
      this.canvas.addEventListener("pointerup", this.up); this.canvas.addEventListener("pointercancel", this.up);
      this.canvas.addEventListener("keydown", this.key);
      this.canvas.addEventListener("contextmenu",this.menu);
      this.canvas.addEventListener("wheel",this.wheel,{passive:false});
      this.renderInspector(); this.render();
    }
    destroy() {
      this.canvas.removeEventListener("pointerdown", this.down); this.canvas.removeEventListener("pointermove", this.move);
      this.canvas.removeEventListener("pointerup", this.up); this.canvas.removeEventListener("pointercancel", this.up);
      this.canvas.removeEventListener("keydown", this.key);
      this.canvas.removeEventListener("contextmenu",this.menu);
      this.canvas.removeEventListener("wheel",this.wheel);
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
    volumeTop() { let bottom=80;this.model.services.forEach(service=>{const b=this.serviceBox(service);bottom=Math.max(bottom,b.y+b.h);});const volumeService=this.model.services.find(service=>service.name===this.volumePanelService),portService=this.model.services.find(service=>service.name===this.portPanelService),networkService=this.model.services.find(service=>service.name===this.networkPanelService),environmentService=this.model.services.find(service=>service.name===this.environmentPanelService),panelService=volumeService||portService||networkService||environmentService,items=volumeService?this.serviceVolumes(volumeService):(portService?portService.ports:(networkService?networkService.networks:(environmentService?environmentService.environment:[])));if(panelService){const handle=volumeService?this.mountHandleBox(panelService):(portService?this.portHandleBox(panelService):(networkService?this.networkHandleBox(panelService):this.environmentHandleBox(panelService)));bottom=Math.max(bottom,handle.y+handle.h+18+(items.length+1)*26);}return bottom+72; }
    volumeBox(i) { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20)));return{x:24+(i%cols)*(VW+20),y:this.volumeTop()+Math.floor(i/cols)*50,w:VW,h:VH}; }
    networkTop() { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20))),rows=Math.max(1,Math.ceil(this.model.volumes.length/cols));return this.volumeTop()+rows*50+38; }
    networkBox(i) { const width=Math.max(820,this.canvas.parentElement?this.canvas.parentElement.clientWidth:0),cols=Math.max(1,Math.floor((width-48)/(VW+20)));return{x:24+(i%cols)*(VW+20),y:this.networkTop()+Math.floor(i/cols)*50,w:VW,h:VH}; }
    sectionAddBox(kind) { const top=kind==="service"?38:(kind==="volume"?this.volumeTop():this.networkTop()),y=top-29;return{x:kind==="volume"?72:60,y,w:22,h:18}; }
    hitSectionAdd(p,kind) { const b=this.sectionAddBox(kind);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h; }
    envFileBox() { return{x:92,y:9,w:34,h:18}; }
    hitEnvFile(p) { const b=this.envFileBox();return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h; }
    serviceEnvFileBadgeBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w-25,y:b.y+7,w:17,h:17}; }
    hitServiceEnvFileBadge(p) { return [...this.model.services].reverse().find(service=>service.envFiles.includes(".env")&&(()=>{const b=this.serviceEnvFileBadgeBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})())||null; }
    envFileMenuBox(service) { const b=this.serviceEnvFileBadgeBox(service);return{x:b.x+b.w+5,y:b.y-4,w:83,h:26}; }
    hitEnvFileMenuDelete(p) { const service=this.model.services.find(item=>item.name===this.envFileMenuService);if(!service)return null;const b=this.envFileMenuBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h?service:null; }
    point(e) { const r = this.canvas.getBoundingClientRect(); return { x: (e.clientX - r.left - this.panX)/this.zoom, y: (e.clientY - r.top - this.panY)/this.zoom }; }
    zoomAt(e) { e.preventDefault();const r=this.canvas.getBoundingClientRect(),sx=e.clientX-r.left,sy=e.clientY-r.top,worldX=(sx-this.panX)/this.zoom,worldY=(sy-this.panY)/this.zoom,unit=e.deltaMode===1?16:(e.deltaMode===2?r.height:1),next=Math.min(2.5,Math.max(.35,this.zoom*Math.exp(-e.deltaY*unit*.001)));if(Math.abs(next-this.zoom)<.001)return;this.zoom=next;this.panX=sx-worldX*next;this.panY=sy-worldY*next;this.render(); }
    hitService(p) {
      return [...this.model.services].reverse().find(s => { const b = this.serviceBox(s); return p.x >= b.x && p.x <= b.x + b.w && p.y >= b.y && p.y <= b.y + b.h; }) || null;
    }
    hitLinkHandle(p) {
      return [...this.model.services].reverse().find(s => {
        const b = this.serviceBox(s);
        return Math.hypot(p.x - b.x, p.y - (b.y + b.h / 2)) <= 10;
      }) || null;
    }
    mountHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2-72,y:b.y+b.h-8,w:30,h:16}; }
    portHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2-34,y:b.y+b.h-8,w:30,h:16}; }
    networkHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2+4,y:b.y+b.h-8,w:30,h:16}; }
    environmentHandleBox(service) { const b=this.serviceBox(service);return{x:b.x+b.w/2+42,y:b.y+b.h-8,w:30,h:16}; }
    serviceVolumes(service) { return service&&service.volumes||[]; }
    serviceVolumeLabel(mount) { const parsed=parseVolumeMount(mount),named=this.model.volumes.includes(mount.split(":")[0]);return parsed.kind==="bind"&&!named?`${parsed.source} → ${parsed.target}`:(named?`${mount.split(":")[0]} → ${parsed.target}`:parsed.target); }
    serviceVolumePanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.mountHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=this.serviceVolumes(service).reduce((width,mount)=>Math.max(width,ctx.measureText(this.serviceVolumeLabel(mount)).width),0);ctx.restore();return Math.min(Math.max(150,Math.ceil(textWidth)+90),Math.max(150,canvasWidth-x)); }
    serviceVolumeIconAt(service,index) { const handle=this.mountHandleBox(service);return{x:handle.x+handle.w/2+15,y:handle.y+handle.h+49+index*26}; }
    hitServiceVolumeIcon(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,at:this.serviceVolumeIconAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    serviceVolumeDeleteAt(service,index) { const handle=this.mountHandleBox(service);return{x:handle.x+handle.w/2+38,y:handle.y+handle.h+49+index*26}; }
    hitServiceVolumeDelete(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,at:this.serviceVolumeDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    serviceVolumeAddAt(service) { const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,panelWidth=this.serviceVolumePanelWidth(service);return{x:x-14+panelWidth-20,y:handle.y+handle.h+23}; }
    hitServiceVolumeAdd(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;const at=this.serviceVolumeAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServiceVolumeRow(p) { const service=this.model.services.find(item=>item.name===this.volumePanelService);if(!service)return null;const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,panelWidth=this.serviceVolumePanelWidth(service);return this.serviceVolumes(service).map((mount,index)=>({service,mount,index,y:handle.y+handle.h+49+index*26})).find(row=>p.x>=x+48&&p.x<=x-14+panelWidth&&Math.abs(p.y-row.y)<=10)||null; }
    hitMountHandle(p) {
      return [...this.model.services].reverse().find(service=>{const b=this.mountHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null;
    }
    portPanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.portHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=(service.ports||[]).reduce((width,port)=>Math.max(width,ctx.measureText(port).width),0);ctx.restore();return Math.min(Math.max(150,Math.ceil(textWidth)+66),Math.max(150,canvasWidth-x)); }
    servicePortDeleteAt(service,index) { const handle=this.portHandleBox(service);return{x:handle.x+handle.w/2+20,y:handle.y+handle.h+49+index*26}; }
    servicePortAddAt(service) { const handle=this.portHandleBox(service),x=handle.x+handle.w/2,width=this.portPanelWidth(service);return{x:x-14+width-20,y:handle.y+handle.h+23}; }
    hitPortHandle(p) { return [...this.model.services].reverse().find(service=>{const b=this.portHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null; }
    hitServicePortAdd(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;const at=this.servicePortAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServicePortDelete(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;return (service.ports||[]).map((port,index)=>({service,port,index,at:this.servicePortDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    hitServicePortRow(p) { const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return null;const handle=this.portHandleBox(service),x=handle.x+handle.w/2,width=this.portPanelWidth(service);return (service.ports||[]).map((port,index)=>({service,port,index,y:handle.y+handle.h+49+index*26})).find(row=>p.x>=x+8&&p.x<=x-14+width&&Math.abs(p.y-row.y)<=10)||null; }
    networkPanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.networkHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=(service.networks||[]).reduce((width,name)=>Math.max(width,ctx.measureText(name+(service.networkIps[name]?" · "+service.networkIps[name]:"")).width),0);ctx.restore();return Math.min(Math.max(150,Math.ceil(textWidth)+66),Math.max(150,canvasWidth-x)); }
    serviceNetworkDeleteAt(service,index) { const h=this.networkHandleBox(service);return{x:h.x+h.w/2+20,y:h.y+h.h+49+index*26}; }
    serviceNetworkAddAt(service) { const h=this.networkHandleBox(service),x=h.x+h.w/2,width=this.networkPanelWidth(service);return{x:x-14+width-20,y:h.y+h.h+23}; }
    hitNetworkHandle(p) { return [...this.model.services].reverse().find(service=>{const b=this.networkHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null; }
    hitServiceNetworkAdd(p) { const service=this.model.services.find(item=>item.name===this.networkPanelService);if(!service)return null;const at=this.serviceNetworkAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServiceNetworkDelete(p) { const service=this.model.services.find(item=>item.name===this.networkPanelService);if(!service)return null;return service.networks.map((network,index)=>({service,network,index,at:this.serviceNetworkDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    hitServiceNetworkRow(p) { const service=this.model.services.find(item=>item.name===this.networkPanelService);if(!service)return null;const h=this.networkHandleBox(service),x=h.x+h.w/2,width=this.networkPanelWidth(service);return service.networks.map((network,index)=>({service,network,index,y:h.y+h.h+49+index*26})).find(row=>p.x>=x+8&&p.x<=x-14+width&&Math.abs(p.y-row.y)<=10)||null; }
    environmentPanelWidth(service) { const ctx=this.canvas.getContext("2d"),handle=this.environmentHandleBox(service),x=handle.x+handle.w/2,canvasWidth=parseFloat(this.canvas.style.width||"820");ctx.save();ctx.font="600 12px ui-monospace, monospace";const textWidth=service.environment.reduce((width,value)=>Math.max(width,ctx.measureText(value).width),0);ctx.restore();return Math.min(Math.max(170,Math.ceil(textWidth)+66),Math.max(170,canvasWidth-x)); }
    serviceEnvironmentDeleteAt(service,index) { const h=this.environmentHandleBox(service);return{x:h.x+h.w/2+20,y:h.y+h.h+49+index*26}; }
    serviceEnvironmentAddAt(service) { const h=this.environmentHandleBox(service),x=h.x+h.w/2,width=this.environmentPanelWidth(service);return{x:x-14+width-20,y:h.y+h.h+23}; }
    hitEnvironmentHandle(p) { return [...this.model.services].reverse().find(service=>{const b=this.environmentHandleBox(service);return p.x>=b.x&&p.x<=b.x+b.w&&p.y>=b.y&&p.y<=b.y+b.h;})||null; }
    hitServiceEnvironmentAdd(p) { const service=this.model.services.find(item=>item.name===this.environmentPanelService);if(!service)return null;const at=this.serviceEnvironmentAddAt(service);return Math.abs(p.x-at.x)<=12&&Math.abs(p.y-at.y)<=9?service:null; }
    hitServiceEnvironmentDelete(p) { const service=this.model.services.find(item=>item.name===this.environmentPanelService);if(!service)return null;return service.environment.map((environment,index)=>({service,environment,index,at:this.serviceEnvironmentDeleteAt(service,index)})).find(row=>Math.hypot(p.x-row.at.x,p.y-row.at.y)<=9)||null; }
    hitServiceEnvironmentRow(p) { const service=this.model.services.find(item=>item.name===this.environmentPanelService);if(!service)return null;const h=this.environmentHandleBox(service),x=h.x+h.w/2,width=this.environmentPanelWidth(service);return service.environment.map((environment,index)=>({service,environment,index,y:h.y+h.h+49+index*26})).find(row=>p.x>=x+8&&p.x<=x-14+width&&Math.abs(p.y-row.y)<=10)||null; }
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
      if(e.button===2){this.drag={type:"pan",x:e.clientX,y:e.clientY,panX:this.panX,panY:this.panY};this.canvas.style.cursor="grabbing";this.canvas.setPointerCapture(e.pointerId);e.preventDefault();return;}
      const p = this.point(e), handle = this.hitLinkHandle(p), mountHandle=this.hitMountHandle(p), portHandle=this.hitPortHandle(p),networkHandle=this.hitNetworkHandle(p),environmentHandle=this.hitEnvironmentHandle(p),service = this.hitService(p), volume = this.hitVolume(p),network=this.hitNetwork(p); this.pointer = p;
      const menuDelete=this.hitEnvFileMenuDelete(p);if(menuDelete){this.envFileMenuService="";this.selected=menuDelete.name;this.commit(menuDelete,{envFiles:menuDelete.envFiles.filter(path=>path!==".env")},`已移除 ${menuDelete.name} 的 .env 关联`);return;}
      const envBadge=this.hitServiceEnvFileBadge(p);if(envBadge){this.envFileMenuService=this.envFileMenuService===envBadge.name?"":envBadge.name;this.render();return;}
      if(this.hitEnvFile(p)){this.drag={type:"env-file",x:e.clientX,y:e.clientY,moved:false};this.envFileTarget="";this.canvas.style.cursor="grabbing";this.canvas.setPointerCapture(e.pointerId);e.preventDefault();return;}this.envFileSelected=false;this.envFileMenuService="";
      const serviceEnvironmentAdd=this.hitServiceEnvironmentAdd(p);if(serviceEnvironmentAdd){this.editServiceEnvironment({service:serviceEnvironmentAdd,environment:"",index:-1});return;}const serviceEnvironmentDelete=this.hitServiceEnvironmentDelete(p);if(serviceEnvironmentDelete){this.removeServiceEnvironment(serviceEnvironmentDelete);return;}const serviceEnvironmentRow=this.hitServiceEnvironmentRow(p);if(serviceEnvironmentRow){this.editServiceEnvironment(serviceEnvironmentRow);return;}
      const serviceNetworkAdd=this.hitServiceNetworkAdd(p);if(serviceNetworkAdd){this.editServiceNetwork({service:serviceNetworkAdd,network:"",index:-1});return;}const serviceNetworkDelete=this.hitServiceNetworkDelete(p);if(serviceNetworkDelete){this.removeServiceNetwork(serviceNetworkDelete);return;}const serviceNetworkRow=this.hitServiceNetworkRow(p);if(serviceNetworkRow){this.editServiceNetwork(serviceNetworkRow);return;}
      if(this.hitSectionAdd(p,"service")){this.addService();return;}if(this.hitSectionAdd(p,"volume")){this.addVolume();return;}if(this.hitSectionAdd(p,"network")){this.addNetwork();return;}
      const servicePortAdd=this.hitServicePortAdd(p);if(servicePortAdd){this.editServicePort({service:servicePortAdd,port:"",index:-1});return;}
      const servicePortDelete=this.hitServicePortDelete(p);if(servicePortDelete){this.removeServicePort(servicePortDelete);return;}
      const servicePortRow=this.hitServicePortRow(p);if(servicePortRow){this.editServicePort(servicePortRow);return;}
      const serviceVolumeAdd=this.hitServiceVolumeAdd(p);if(serviceVolumeAdd){if(typeof this.onAddMount==="function")this.onAddMount(serviceVolumeAdd.name,this.model.volumes,mount=>this.addServiceVolume(serviceVolumeAdd,mount));else this.onStatus("挂载编辑器不可用");return;}
      const serviceVolumeDelete=this.hitServiceVolumeDelete(p);if(serviceVolumeDelete){this.removeServiceVolume(serviceVolumeDelete);return;}
      const serviceVolume=this.hitServiceVolumeIcon(p);if(serviceVolume){this.toggleServiceVolumeMode(serviceVolume);return;}
      const serviceVolumeRow=this.hitServiceVolumeRow(p);if(serviceVolumeRow){this.editServiceVolume(serviceVolumeRow);return;}
      if (this.hitDelete(p)) { this.removeSelectedDependency(); return; }
      if (handle) {
        this.selectedLink = null; this.selectedVolume = "";this.selectedNetwork=""; this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService="";
        this.selected = handle.name; this.linkFrom = handle.name; this.linkTarget = "";
        this.drag = { type: "link", name: handle.name };
        this.canvas.style.cursor = LINK_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.onStatus(`拖到目标服务，为 ${handle.name} 添加依赖`); this.render(); return;
      }
      if(mountHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService=mountHandle.name;this.portPanelService="";this.networkPanelService="";this.environmentPanelService="";this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${mountHandle.name} 的数据卷`);this.render();return;}
      if(portHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService="";this.portPanelService=portHandle.name;this.networkPanelService="";this.environmentPanelService="";this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${portHandle.name} 的端口`);this.render();return;}
      if(networkHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService="";this.portPanelService="";this.networkPanelService=networkHandle.name;this.environmentPanelService="";this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${networkHandle.name} 的网络`);this.render();return;}
      if(environmentHandle){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService=environmentHandle.name;this.drag=null;this.canvas.style.cursor="pointer";this.canvas.focus();this.renderInspector();this.onStatus(`已打开 ${environmentHandle.name} 的环境变量`);this.render();return;}
      if (service && this.linkFrom) {
        if (service.name !== this.linkFrom) this.addDependency(this.linkFrom, service.name);
        this.linkFrom = ""; this.onStatus("依赖连接完成"); this.render(); return;
      }
      if (service) {
        this.selected = service.name; this.selectedVolume = "";this.selectedNetwork=""; this.selectedLink = null; this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService=""; const pos = this.positions[service.name];
        this.drag = { type: "service", name: service.name, dx: p.x - pos.x, dy: p.y - pos.y, moved: false };
        this.canvas.style.cursor = SELECT_CURSOR;
        this.canvas.setPointerCapture(e.pointerId); this.renderInspector(); this.render();
      } else if (volume) {
        this.selected = ""; this.selectedVolume = volume;this.selectedNetwork=""; this.selectedLink = null; this.volumePanelService="";this.portPanelService="";this.environmentPanelService=""; this.drag = null; this.canvas.focus(); this.renderInspector(); this.render();
      } else if(network){this.selected="";this.selectedVolume="";this.selectedNetwork=network;this.selectedLink=null;this.volumePanelService="";this.portPanelService="";this.environmentPanelService="";this.renderInspector();this.render();
      } else {
        const link=this.hitDependency(p);this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=link;this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService="";this.hoverServiceVolumeRow=null;this.hoverServicePortRow=null;this.hoverServiceNetworkRow=null;this.hoverServiceEnvironmentRow=null;this.canvas.focus();this.renderInspector();
        this.onStatus(link ? `已选择依赖 ${link.from} → ${link.to}，按 Delete 删除` : "");
        if(!link){this.drag={type:"pan",x:e.clientX,y:e.clientY,panX:this.panX,panY:this.panY};this.canvas.style.cursor="grabbing";this.canvas.setPointerCapture(e.pointerId);}
        this.render();
      }
    }
    pointerMove(e) {
      const p = this.point(e); this.pointer = p;
      if (!this.drag) {
        const previous=this.hoverLink,previousRow=this.hoverServiceVolumeRow,previousPortRow=this.hoverServicePortRow,previousEnvironmentRow=this.hoverServiceEnvironmentRow,handle=this.hitLinkHandle(p),mountHandle=this.hitMountHandle(p),portHandle=this.hitPortHandle(p),networkHandle=this.hitNetworkHandle(p),environmentHandle=this.hitEnvironmentHandle(p),envBadge=this.hitServiceEnvFileBadge(p),envMenuDelete=this.hitEnvFileMenuDelete(p),node=this.hitService(p),volume=this.hitVolume(p),network=this.hitNetwork(p),sectionAdd=this.hitSectionAdd(p,"service")||this.hitSectionAdd(p,"volume")||this.hitSectionAdd(p,"network"),envFile=this.hitEnvFile(p),serviceVolume=this.hitServiceVolumeIcon(p),serviceVolumeDelete=this.hitServiceVolumeDelete(p),serviceVolumeRow=this.hitServiceVolumeRow(p),serviceVolumeAdd=this.hitServiceVolumeAdd(p),servicePortAdd=this.hitServicePortAdd(p),servicePortDelete=this.hitServicePortDelete(p),servicePortRow=this.hitServicePortRow(p),serviceEnvironmentAdd=this.hitServiceEnvironmentAdd(p),serviceEnvironmentDelete=this.hitServiceEnvironmentDelete(p),serviceEnvironmentRow=this.hitServiceEnvironmentRow(p);this.hoverServiceVolumeRow=serviceVolumeRow;this.hoverServicePortRow=servicePortRow;this.hoverServiceEnvironmentRow=serviceEnvironmentRow;this.hoverLink=handle||mountHandle||portHandle||networkHandle||environmentHandle||envBadge||envMenuDelete||node||volume||network||sectionAdd||envFile||serviceVolume||serviceVolumeDelete||serviceVolumeRow||serviceVolumeAdd||servicePortAdd||servicePortDelete||servicePortRow||serviceEnvironmentAdd||serviceEnvironmentDelete||serviceEnvironmentRow?null:this.hitDependency(p);
        this.canvas.style.cursor = serviceVolume||serviceVolumeDelete||serviceVolumeRow||serviceVolumeAdd||servicePortAdd||servicePortDelete||servicePortRow||serviceEnvironmentAdd||serviceEnvironmentDelete||serviceEnvironmentRow||mountHandle||portHandle||networkHandle||environmentHandle||envBadge||envMenuDelete||this.hitDelete(p)||this.hoverLink||volume||network||sectionAdd||envFile?"pointer":(handle?LINK_CURSOR:(node?SELECT_CURSOR:"default"));
        if(!this.sameLink(previous,this.hoverLink)||(previousRow&&previousRow.index)!==(serviceVolumeRow&&serviceVolumeRow.index)||(previousPortRow&&previousPortRow.index)!==(servicePortRow&&servicePortRow.index)||(previousEnvironmentRow&&previousEnvironmentRow.index)!==(serviceEnvironmentRow&&serviceEnvironmentRow.index))this.render();
        return;
      }
      this.canvas.style.cursor = this.drag.type === "link" ? LINK_CURSOR : SELECT_CURSOR;
      if(this.drag.type==="pan"){this.panX=this.drag.panX+e.clientX-this.drag.x;this.panY=this.drag.panY+e.clientY-this.drag.y;this.canvas.style.cursor="grabbing";this.render();return;}
      if(this.drag.type==="env-file"){this.drag.moved=this.drag.moved||Math.hypot(e.clientX-this.drag.x,e.clientY-this.drag.y)>4;const target=this.hitService(p);this.envFileTarget=target?target.name:"";this.canvas.style.cursor="grabbing";this.render();return;}
      if (this.drag.type === "link") {
        const target = this.hitService(p);
        this.linkTarget = target && target.name !== this.drag.name ? target.name : "";
      }
      if (this.drag.type === "service") {
        this.positions[this.drag.name] = { x: Math.max(25,Math.round((p.x-this.drag.dx)/5)*5), y: Math.max(35,Math.round((p.y-this.drag.dy)/5)*5) };
        this.drag.moved = true;
      }
      this.render();
    }
    pointerUp(e) {
      if (!this.drag) return; const p = this.point(e);
      if(this.drag.type==="pan"){this.drag=null;this.canvas.style.cursor="grab";return;}
      if(this.drag.type==="env-file"){const moved=this.drag.moved,target=e.type==="pointercancel"?null:(this.model.services.find(service=>service.name===this.envFileTarget)||this.hitService(p));this.drag=null;this.envFileTarget="";this.canvas.style.cursor="default";if(moved&&target){if(target.envFiles.includes(".env")){this.onStatus(`${target.name} 已关联 .env`);this.render();return;}this.selected=target.name;this.envFileSelected=false;this.commit(target,{envFiles:[...target.envFiles,".env"]},`已为 ${target.name} 关联 .env`);return;}if(!moved&&e.type!=="pointercancel"){this.selected="";this.selectedVolume="";this.selectedNetwork="";this.selectedLink=null;this.envFileSelected=true;this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService="";this.canvas.focus();if(typeof this.onOpenEnvFile==="function")this.onOpenEnvFile();else this.onStatus(".env 编辑器不可用");}this.render();return;}
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
      else if (e.key === "Escape" && (this.selectedLink || this.selectedVolume || this.selectedNetwork || this.volumePanelService || this.portPanelService || this.networkPanelService || this.environmentPanelService)) { e.preventDefault(); this.selectedLink = null; this.selectedVolume = "";this.selectedNetwork=""; this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService=""; this.onStatus("已取消选择"); this.renderInspector(); this.render(); }
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
    removeServiceNetwork(row) { const networks=row.service.networks.filter((value,index)=>index!==row.index),networkIps={...row.service.networkIps};delete networkIps[row.network];this.commit(row.service,{networks,networkIps},`已删除 ${row.service.name} 的网络 ${row.network}`); }
    editServiceNetwork(row) { if(typeof this.onEditNetwork!=="function"){this.onStatus("网络编辑器不可用");return;}this.onEditNetwork(row.service.name,this.model.networks,{network:row.network,ipv4Address:row.service.networkIps[row.network]||""},result=>{const network=result.network,networks=row.index<0?[...row.service.networks,network]:row.service.networks.map((value,index)=>index===row.index?network:value),networkIps={...row.service.networkIps};if(row.network&&row.network!==network)delete networkIps[row.network];if(result.ipv4Address)networkIps[network]=result.ipv4Address;else delete networkIps[network];this.commit(row.service,{networks:unique(networks),networkIps},`${row.index<0?"已添加":"已更新"} ${row.service.name} 的网络 ${network}`);}); }
    removeServiceEnvironment(row) { const environment=row.service.environment.filter((value,index)=>index!==row.index);this.commit(row.service,{environment},`已删除 ${row.service.name} 的环境变量 ${row.environment.split("=")[0]}`); }
    editServiceEnvironment(row) { if(typeof this.onEditEnvironment!=="function"){this.onStatus("环境变量编辑器不可用");return;}this.onEditEnvironment(row.service.name,row.environment,value=>{const key=value.split("=")[0],base=row.index<0?row.service.environment.filter(item=>item.split("=")[0]!==key):row.service.environment,environment=row.index<0?[...base,value]:base.map((item,index)=>index===row.index?value:item);this.commit(row.service,{environment},`${row.index<0?"已添加":"已更新"} ${row.service.name} 的环境变量 ${key}`);}); }
    addDependency(from, to) {
      const service = this.model.services.find(s => s.name === from);
      if (service && !service.depends.includes(to)) { service.depends.push(to); this.commit(service, { depends: service.depends }, `已添加依赖 ${from} → ${to}`); }
    }
    addService() { let index=1,name="service-1";while(this.model.services.some(service=>service.name===name))name="service-"+(++index);this.yaml=addServiceYaml(this.yaml,name);this.model=parse(this.yaml);this.ensurePositions();this.selected=name;this.selectedVolume="";this.selectedNetwork="";this.volumePanelService="";this.portPanelService="";this.networkPanelService="";this.environmentPanelService="";this.onChange(this.yaml,`已添加服务 ${name}`);this.onStatus(`已添加服务 ${name}`);this.renderInspector();this.render(); }
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
      if (!service) { const note=readHeaderNote(this.yaml);this.inspector.innerHTML=`<div class="compose-inspector-title">Compose</div><label>文件头注释<textarea data-compose-note rows="5" placeholder="输入 Compose 文件说明">${html(note)}</textarea></label><button type="button" class="btn primary" data-compose-save>保存 Compose</button>`;this.inspector.querySelector("[data-compose-save]").onclick=()=>{const yaml=writeHeaderNote(this.yaml,this.inspector.querySelector("[data-compose-note]").value);if(yaml!==this.yaml){this.yaml=yaml;this.model=parse(yaml);this.onChange(yaml,"已更新 Compose 文件头注释");}if(typeof this.onSaveCompose==="function")this.onSaveCompose();};return; }
      const field = (label, key, value, area) => `<label>${label}${area ? `<textarea data-field="${key}" rows="3">${html((value || []).join("\n"))}</textarea>` : `<input data-field="${key}" type="text" value="${html(value || "")}" />`}</label>`;
      const restart=service.restart||"unless-stopped",restartField=`<label>重启策略<select data-field="restart"><option value="no" ${restart==="no"?"selected":""}>no</option><option value="always" ${restart==="always"?"selected":""}>always</option><option value="on-failure" ${restart==="on-failure"?"selected":""}>on-failure</option><option value="unless-stopped" ${restart==="unless-stopped"?"selected":""}>unless-stopped</option></select></label>`;
      const health=service.healthcheck,healthEnabled=!!health,test=health&&health.test||[],testMode=["CMD","CMD-SHELL","NONE"].includes(test[0])?test[0]:"CMD-SHELL",testCommand=testMode==="CMD"?test.slice(1).join("\n"):test.slice(1).join(" "),healthField=(label,key,value,placeholder,type="text")=>`<label>${label}<input data-health-${key} type="${type}" value="${html(value||"")}" placeholder="${placeholder}" /></label>`,healthEditor=`<section class="compose-health-editor"><label class="compose-check-row"><input data-health-enabled type="checkbox" ${healthEnabled?"checked":""} /> 启用 Healthcheck</label><div data-health-fields ${healthEnabled?"":"hidden"}><label>检测方式<select data-health-mode><option value="CMD-SHELL" ${testMode==="CMD-SHELL"?"selected":""}>CMD-SHELL</option><option value="CMD" ${testMode==="CMD"?"selected":""}>CMD</option><option value="NONE" ${testMode==="NONE"?"selected":""}>NONE</option></select></label><label>检测命令<textarea data-health-command rows="3" placeholder="CMD-SHELL：填写命令；CMD：每行一个参数">${html(testCommand)}</textarea></label>${healthField("检查间隔","interval",health&&health.interval,"30s")}${healthField("超时时间","timeout",health&&health.timeout,"10s")}${healthField("失败重试次数","retries",health&&health.retries,"3","number")}${healthField("启动宽限期","start-period",health&&health.startPeriod,"40s")}${healthField("启动期检查间隔","start-interval",health&&health.startInterval,"5s")}<label class="compose-check-row"><input data-health-disable type="checkbox" ${health&&health.disable?"checked":""} /> disable: true</label></div></section>`;
      const readOnlyList=(label,items)=>`<section class="compose-readonly-list"><div class="compose-readonly-title">${label}</div>${items.length?`<ul>${items.map(item=>`<li>${html(item)}</li>`).join("")}</ul>`:`<div class="compose-readonly-empty">无</div>`}</section>`,mounts=service.volumes.map(mount=>this.serviceVolumeLabel(mount)),ports=service.ports||[],networks=service.networks.map(name=>name+(service.networkIps[name]?` · ${service.networkIps[name]}`:""));
      this.inspector.innerHTML = `<div class="compose-service-inspector"><div class="compose-service-inspector-content"><div class="compose-inspector-title">${html(service.name)}</div>${field("镜像","image",service.image)}${field("容器名称","containerName",service.containerName)}${field("启动命令","command",service.command)}${field("运行用户（可选）","user",service.user)}${restartField}${healthEditor}${readOnlyList("数据卷",mounts)}${readOnlyList("端口",ports)}${readOnlyList("网络",networks)}${readOnlyList("环境变量",service.environment)}${readOnlyList("环境变量文件",service.envFiles)}</div><div class="compose-service-inspector-footer"><button type="button" class="btn primary" data-apply>应用到草稿</button></div></div>`;
      this.inspector.querySelector("[data-health-enabled]").onchange=e=>{this.inspector.querySelector("[data-health-fields]").hidden=!e.target.checked;};
      this.inspector.querySelector("[data-apply]").onclick = () => {
        const patch = {};
        this.inspector.querySelectorAll("[data-field]").forEach(input => {
          const key = input.dataset.field, value = input.tagName === "TEXTAREA" ? unique(input.value.split(/\r?\n/)) : input.value.trim();
          if (JSON.stringify(value) !== JSON.stringify(service[key])) patch[key] = value;
        });
        const enabled=this.inspector.querySelector("[data-health-enabled]").checked;let nextHealth=null;if(enabled){const mode=this.inspector.querySelector("[data-health-mode]").value,command=this.inspector.querySelector("[data-health-command]").value.trim(),disable=this.inspector.querySelector("[data-health-disable]").checked;if(mode!=="NONE"&&!command&&!disable){this.onStatus("请填写 Healthcheck 检测命令，或启用 disable");return;}const retries=this.inspector.querySelector("[data-health-retries]").value.trim();if(retries&&(!/^\d+$/.test(retries)||Number(retries)<1)){this.onStatus("Healthcheck 失败重试次数必须是正整数");return;}nextHealth={test:mode==="NONE"?["NONE"]:(command?(mode==="CMD"?["CMD",...command.split(/\r?\n/).map(value=>value.trim()).filter(Boolean)]:["CMD-SHELL",command]):[]),interval:this.inspector.querySelector("[data-health-interval]").value.trim(),timeout:this.inspector.querySelector("[data-health-timeout]").value.trim(),retries,startPeriod:this.inspector.querySelector("[data-health-start-period]").value.trim(),startInterval:this.inspector.querySelector("[data-health-start-interval]").value.trim(),disable};}if(JSON.stringify(nextHealth)!==JSON.stringify(service.healthcheck))patch.healthcheck=nextHealth;
        if (!Object.keys(patch).length) { this.onStatus("服务属性没有变化"); return; }
        this.commit(service, patch, `已更新服务 ${service.name} 的属性`);
      };
    }
    render() {
      this.ensurePositions(); const viewport=this.canvas.parentElement,width=Math.max(1,viewport?viewport.clientWidth:820),height=Math.max(1,viewport?viewport.clientHeight:360);
      const dpr = Math.max(1, global.devicePixelRatio || 1); this.canvas.width = width * dpr; this.canvas.height = height * dpr; this.canvas.style.width = width + "px"; this.canvas.style.height = height + "px";
      const ctx = this.canvas.getContext("2d"); ctx.scale(dpr, dpr);
      const c = { bg: color("--bg-2","#161b22"), card: color("--bg-3","#21262d"), text: color("--text","#f0f6fc"), muted: color("--muted","#8b949e"), line: color("--line","#30363d"), accent: color("--accent","#58a6ff"), active: color("--bg-active","#1f6feb33"), volume:"#9a6700", volumeActive:"rgba(154,103,0,.16)", port:"#1a7f37", portActive:"rgba(26,127,55,.16)", network:"#8250df", networkActive:"rgba(130,80,223,.16)", environment:"#bc4c00", environmentActive:"rgba(188,76,0,.16)" };
      ctx.fillStyle=c.bg;ctx.fillRect(0,0,width,height);this.drawGrid(ctx,width,height,c);ctx.translate(this.panX,this.panY);ctx.scale(this.zoom,this.zoom);this.drawSectionHeader(ctx,"服务","service",c.accent);this.drawEnvFileButton(ctx,c);this.drawSectionHeader(ctx,"命名卷","volume",c.volume);this.drawSectionHeader(ctx,"网络","network",c.network);
      this.drawMounts(ctx,c);this.drawNetworkLinks(ctx,c);this.drawLinks(ctx,c);this.drawEnvFileDrag(ctx,c);this.model.volumes.forEach((v,i)=>this.drawVolume(ctx,v,i,c));this.model.networks.forEach((v,i)=>this.drawNetwork(ctx,v,i,c));this.model.services.forEach(s=>this.drawService(ctx,s,c));
      if (this.drag && this.drag.type === "link") {
        const service=this.model.services.find(s=>s.name===this.drag.name),b=service&&this.serviceBox(service);
        if(b){ctx.save();ctx.strokeStyle=c.accent;ctx.lineWidth=1.5;ctx.setLineDash([5,4]);ctx.beginPath();ctx.moveTo(b.x,b.y+b.h/2);ctx.lineTo(b.x-15,b.y+b.h/2);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore();}
      }
      this.drawUnifiedServicePanel(ctx,c);this.drawEnvironmentPanel(ctx,c);this.drawServiceNetworkIps(ctx,c);this.drawEnvFileMenu(ctx,c);
    }
    drawMounts(ctx,c) {
      if (!this.selectedVolume) return;
      this.mountLinks().filter(link=>link.volume===this.selectedVolume).forEach(link=>{ctx.save();ctx.strokeStyle=c.muted;ctx.lineWidth=1.5;ctx.setLineDash([6,5]);ctx.beginPath();ctx.moveTo(link.points[0].x,link.points[0].y);ctx.lineTo(link.points[1].x,link.points[1].y);ctx.stroke();ctx.restore();});
    }
    drawNetworkLinks(ctx,c) { if(!this.selectedNetwork)return;this.networkLinks().filter(link=>link.network===this.selectedNetwork).forEach(link=>{ctx.save();ctx.strokeStyle=c.muted;ctx.lineWidth=1.5;ctx.setLineDash([6,5]);ctx.beginPath();ctx.moveTo(link.points[0].x,link.points[0].y);ctx.lineTo(link.points[1].x,link.points[1].y);ctx.stroke();ctx.restore();}); }
    drawEnvFileDrag(ctx,c) { if(!this.drag||this.drag.type!=="env-file")return;const b=this.envFileBox();ctx.save();ctx.strokeStyle="#0a7f83";ctx.lineWidth=1.5;ctx.setLineDash([5,4]);ctx.beginPath();ctx.moveTo(b.x+b.w/2,b.y+b.h);ctx.lineTo(this.pointer.x,this.pointer.y);ctx.stroke();ctx.restore(); }
    drawUnifiedServicePanel(ctx,c) { const kind=this.volumePanelService?"volume":(this.portPanelService?"port":(this.networkPanelService?"network":""));if(!kind)return;const key=kind==="volume"?this.volumePanelService:(kind==="port"?this.portPanelService:this.networkPanelService),service=this.model.services.find(item=>item.name===key);if(!service)return;const handle=kind==="volume"?this.mountHandleBox(service):(kind==="port"?this.portHandleBox(service):this.networkHandleBox(service)),items=kind==="volume"?service.volumes:(kind==="port"?service.ports:service.networks),width=kind==="volume"?this.serviceVolumePanelWidth(service):(kind==="port"?this.portPanelWidth(service):this.networkPanelWidth(service)),add=kind==="volume"?this.serviceVolumeAddAt(service):(kind==="port"?this.servicePortAddAt(service):this.serviceNetworkAddAt(service)),tone=kind==="volume"?c.volume:(kind==="port"?c.port:c.network),label=kind==="volume"?"数据卷":(kind==="port"?"端口":"网络"),x=handle.x+handle.w/2,start=handle.y+handle.h+5,lastY=items.length?start+44+(items.length-1)*26:start;ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle=tone;ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);if(items.length){ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);}ctx.stroke();ctx.fillStyle=tone;ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText(label,x+2,start+18);roundRect(ctx,add.x-10,add.y-8,20,16,5);ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();items.forEach((value,index)=>{const y=start+44+index*26,remove=kind==="volume"?this.serviceVolumeDeleteAt(service,index):(kind==="port"?this.servicePortDeleteAt(service,index):this.serviceNetworkDeleteAt(service,index)),hovered=kind==="volume"?this.hoverServiceVolumeRow&&this.hoverServiceVolumeRow.index===index:(kind==="port"?this.hoverServicePortRow&&this.hoverServicePortRow.index===index:this.hoverServiceNetworkRow&&this.hoverServiceNetworkRow.index===index),text=kind==="volume"?this.serviceVolumeLabel(value):value;ctx.strokeStyle=tone;ctx.lineWidth=1.25;ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();if(kind==="volume"){const parsed=parseVolumeMount(value),named=this.model.volumes.includes(value.split(":")[0]),shape=named?"diamond":parsed.kind;ctx.fillStyle=tone;ctx.beginPath();if(shape==="diamond"){ctx.moveTo(x+20,y-7);ctx.lineTo(x+27,y);ctx.lineTo(x+20,y+7);ctx.lineTo(x+13,y);ctx.closePath();}else if(shape==="bind")ctx.rect(x+14.5,y-5.5,11,11);else ctx.arc(x+20,y,6,0,Math.PI*2);if(parsed.mode==="RO")ctx.stroke();else ctx.fill();}ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,y-4);ctx.lineTo(remove.x+4,y+4);ctx.moveTo(remove.x+4,y-4);ctx.lineTo(remove.x-4,y+4);ctx.stroke();ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,text,width-(kind==="volume"?82:55)),x+(kind==="volume"?52:34),y);});ctx.restore(); }
    drawServiceNetworkIps(ctx,c) { const service=this.model.services.find(item=>item.name===this.networkPanelService);if(!service)return;const handle=this.networkHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h+5;ctx.save();ctx.fillStyle="#24292f";ctx.font="400 12px ui-monospace, monospace";ctx.textBaseline="middle";service.networks.forEach((name,index)=>{const ip=service.networkIps[name];if(ip)ctx.fillText(` · ${ip}`,x+34+ctx.measureText(name).width,start+44+index*26);});ctx.restore(); }
    drawEnvironmentPanel(ctx,c) { const service=this.model.services.find(item=>item.name===this.environmentPanelService);if(!service)return;const items=service.environment,handle=this.environmentHandleBox(service),width=this.environmentPanelWidth(service),add=this.serviceEnvironmentAddAt(service),x=handle.x+handle.w/2,start=handle.y+handle.h+5,lastY=items.length?start+44+(items.length-1)*26:start;ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle=c.environment;ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);if(items.length){ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);}ctx.stroke();ctx.fillStyle=c.environment;ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("环境变量",x+2,start+18);roundRect(ctx,add.x-10,add.y-8,20,16,5);ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();items.forEach((value,index)=>{const y=start+44+index*26,remove=this.serviceEnvironmentDeleteAt(service,index),hovered=this.hoverServiceEnvironmentRow&&this.hoverServiceEnvironmentRow.index===index;ctx.strokeStyle=c.environment;ctx.lineWidth=1.25;ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,y-4);ctx.lineTo(remove.x+4,y+4);ctx.moveTo(remove.x+4,y-4);ctx.lineTo(remove.x-4,y+4);ctx.stroke();ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,value,width-55),x+34,y);});ctx.restore(); }
    drawEnvFileMenu(ctx,c) { const service=this.model.services.find(item=>item.name===this.envFileMenuService);if(!service)return;const b=this.envFileMenuBox(service);ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=8;roundRect(ctx,b.x,b.y,b.w,b.h,5);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle="#d0d7de";ctx.lineWidth=1;ctx.stroke();ctx.fillStyle="#cf222e";ctx.font="600 11px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText("×  删除关联",b.x+b.w/2,b.y+b.h/2);ctx.restore(); }
    drawGrid(ctx,width,height,c) { const gap=20*this.zoom,ox=((this.panX+10*this.zoom)%gap+gap)%gap,oy=((this.panY+10*this.zoom)%gap+gap)%gap;ctx.save();ctx.fillStyle=c.muted;ctx.globalAlpha=.42;for(let y=oy;y<height;y+=gap)for(let x=ox;x<width;x+=gap){ctx.beginPath();ctx.arc(x,y,1,0,Math.PI*2);ctx.fill();}ctx.restore(); }
    drawPanelChrome(ctx,kind,c) { const key=kind==="volume"?this.volumePanelService:(kind==="port"?this.portPanelService:this.networkPanelService),service=this.model.services.find(item=>item.name===key);if(!service)return;const handle=kind==="volume"?this.mountHandleBox(service):(kind==="port"?this.portHandleBox(service):this.networkHandleBox(service)),items=kind==="volume"?service.volumes:(kind==="port"?service.ports:service.networks),width=kind==="volume"?this.serviceVolumePanelWidth(service):(kind==="port"?this.portPanelWidth(service):this.networkPanelWidth(service)),add=kind==="volume"?this.serviceVolumeAddAt(service):(kind==="port"?this.servicePortAddAt(service):this.serviceNetworkAddAt(service)),tone=kind==="volume"?c.volume:(kind==="port"?c.port:c.network),label=kind==="volume"?"数据卷":(kind==="port"?"端口":"网络"),x=handle.x+handle.w/2,start=handle.y+handle.h+5,lastY=items.length?start+44+(items.length-1)*26:start;ctx.save();ctx.strokeStyle=tone;ctx.fillStyle=tone;ctx.lineWidth=1;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);if(items.length){ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);}ctx.stroke();ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText(label,x+2,start+18);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.stroke();ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.restore(); }
    drawPanelAddIcon(ctx,kind,c) { const key=kind==="volume"?this.volumePanelService:(kind==="port"?this.portPanelService:this.networkPanelService),service=this.model.services.find(item=>item.name===key);if(!service)return;const add=kind==="volume"?this.serviceVolumeAddAt(service):(kind==="port"?this.servicePortAddAt(service):this.serviceNetworkAddAt(service)),tone=kind==="volume"?c.volume:(kind==="port"?c.port:c.network);ctx.save();ctx.fillStyle="#fff";ctx.fillRect(add.x-14,add.y-11,28,29);roundRect(ctx,add.x-10,add.y-8,20,16,5);ctx.fillStyle=tone;ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.restore(); }
    drawSectionHeader(ctx,label,kind,colorValue) { const top=kind==="service"?38:(kind==="volume"?this.volumeTop():this.networkTop()),button=this.sectionAddBox(kind),centerY=button.y+button.h/2;ctx.fillStyle=colorValue;ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText(label,24,centerY);roundRect(ctx,button.x,button.y,button.w,button.h,5);ctx.strokeStyle=colorValue;ctx.lineWidth=1.25;ctx.stroke();ctx.beginPath();ctx.moveTo(button.x+7,centerY);ctx.lineTo(button.x+15,centerY);ctx.moveTo(button.x+11,centerY-4);ctx.lineTo(button.x+11,centerY+4);ctx.stroke();ctx.textBaseline="alphabetic"; }
    drawEnvFileButton(ctx,c) { const b=this.envFileBox(),tone="#0a7f83",cx=b.x+b.w/2,cy=b.y+b.h/2;ctx.save();roundRect(ctx,b.x,b.y,b.w,b.h,5);ctx.fillStyle=this.envFileSelected?"rgba(10,127,131,.16)":c.card;ctx.fill();ctx.strokeStyle=tone;ctx.lineWidth=this.envFileSelected?2:1.25;ctx.stroke();ctx.strokeStyle=tone;ctx.lineWidth=1;ctx.beginPath();ctx.moveTo(cx-8,cy-6);ctx.lineTo(cx+3,cy-6);ctx.lineTo(cx+8,cy-1);ctx.lineTo(cx+8,cy+6);ctx.lineTo(cx-8,cy+6);ctx.closePath();ctx.moveTo(cx+3,cy-6);ctx.lineTo(cx+3,cy-1);ctx.lineTo(cx+8,cy-1);ctx.stroke();ctx.fillStyle=tone;ctx.font="700 7px ui-monospace, monospace";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText("ENV",cx,cy+2);ctx.restore(); }
    drawServiceVolumes(ctx,c) {
      c={...c,accent:c.volume,active:c.volumeActive,muted:c.volume};
      const service=this.model.services.find(item=>item.name===this.volumePanelService),items=this.serviceVolumes(service);if(!service)return;
      const handle=this.mountHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h+5,lastY=items.length?start+44+(items.length-1)*26:start,panelWidth=this.serviceVolumePanelWidth(service),headerY=start+18;
      ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,panelWidth,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle="#d0d7de";ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+panelWidth,start+31);ctx.stroke();ctx.fillStyle="#57606a";ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("数据卷",x+2,headerY);const add=this.serviceVolumeAddAt(service);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.fillStyle="#f6f8fa";ctx.fill();ctx.strokeStyle="#d0d7de";ctx.stroke();ctx.strokeStyle="#0969da";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.lineWidth=1.25;if(items.length){ctx.beginPath();ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);ctx.stroke();}ctx.textBaseline="middle";
      items.forEach((mount,index)=>{const parsed=parseVolumeMount(mount),named=this.model.volumes.includes(mount.split(":")[0]),item=named?{...parsed,kind:"named"}:parsed,y=start+44+index*26,label=this.serviceVolumeLabel(mount),hovered=this.hoverServiceVolumeRow&&this.hoverServiceVolumeRow.index===index;ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.fillStyle=item.mode==="RO"?"#57606a":c.accent;if(item.kind==="named"){const cx=x+20,size=7;ctx.beginPath();ctx.moveTo(cx,y-size);ctx.lineTo(cx+size,y);ctx.lineTo(cx,y+size);ctx.lineTo(cx-size,y);ctx.closePath();if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}else if(item.kind==="bind"){const size=11;ctx.beginPath();ctx.rect(x+20-size/2,y-size/2,size,size);if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}else{ctx.beginPath();ctx.arc(x+20,y,6,0,Math.PI*2);if(item.mode==="RO"){ctx.strokeStyle="#57606a";ctx.lineWidth=1.25;ctx.stroke();}else{ctx.fill();}ctx.strokeStyle="#afb8c1";}const remove=this.serviceVolumeDeleteAt(service,index);ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,remove.y-4);ctx.lineTo(remove.x+4,remove.y+4);ctx.moveTo(remove.x+4,remove.y-4);ctx.lineTo(remove.x-4,remove.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,label,panelWidth-82),x+52,y);});ctx.restore();
    }
    drawServicePorts(ctx,c) {
      c={...c,accent:c.port,active:c.portActive,muted:c.port};
      const service=this.model.services.find(item=>item.name===this.portPanelService);if(!service)return;const items=service.ports||[],handle=this.portHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h+5,lastY=items.length?start+44+(items.length-1)*26:start,width=this.portPanelWidth(service),headerY=start+18;
      ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle="#d0d7de";ctx.lineWidth=1;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);ctx.stroke();ctx.fillStyle="#57606a";ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("端口",x+2,headerY);const add=this.servicePortAddAt(service);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.fillStyle="#f6f8fa";ctx.fill();ctx.strokeStyle="#d0d7de";ctx.stroke();ctx.strokeStyle="#0969da";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.lineWidth=1.25;if(items.length){ctx.beginPath();ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);ctx.stroke();}items.forEach((port,index)=>{const y=start+44+index*26,hovered=this.hoverServicePortRow&&this.hoverServicePortRow.index===index,remove=this.servicePortDeleteAt(service,index);ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.strokeStyle="#cf222e";ctx.lineWidth=1.5;ctx.beginPath();ctx.moveTo(remove.x-4,y-4);ctx.lineTo(remove.x+4,y+4);ctx.moveTo(remove.x+4,y-4);ctx.lineTo(remove.x-4,y+4);ctx.stroke();ctx.strokeStyle="#afb8c1";ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,port,width-55),x+34,y);});ctx.restore();
    }
    drawServiceNetworks(ctx,c) { const service=this.model.services.find(item=>item.name===this.networkPanelService);if(!service)return;const items=service.networks,handle=this.networkHandleBox(service),x=handle.x+handle.w/2,start=handle.y+handle.h,lastY=items.length?start+44+(items.length-1)*26:start,width=this.networkPanelWidth(service),headerY=start+18;ctx.save();ctx.shadowColor="rgba(0,0,0,.18)";ctx.shadowBlur=10;roundRect(ctx,x-14,start+2,width,(items.length+1)*26+12,6);ctx.fillStyle="#fff";ctx.fill();ctx.shadowBlur=0;ctx.strokeStyle=c.network;ctx.stroke();ctx.beginPath();ctx.moveTo(x-14,start+31);ctx.lineTo(x-14+width,start+31);ctx.stroke();ctx.fillStyle=c.network;ctx.font="600 12px system-ui";ctx.textBaseline="middle";ctx.fillText("网络",x+2,headerY);const add=this.serviceNetworkAddAt(service);roundRect(ctx,add.x-12,add.y-9,24,18,5);ctx.fillStyle="#f6f8fa";ctx.fill();ctx.strokeStyle=c.network;ctx.stroke();ctx.beginPath();ctx.moveTo(add.x-4,add.y);ctx.lineTo(add.x+4,add.y);ctx.moveTo(add.x,add.y-4);ctx.lineTo(add.x,add.y+4);ctx.stroke();if(items.length){ctx.beginPath();ctx.moveTo(x,start+32);ctx.lineTo(x,lastY);ctx.stroke();}items.forEach((network,index)=>{const y=start+44+index*26,hovered=this.hoverServiceNetworkRow&&this.hoverServiceNetworkRow.index===index,remove=this.serviceNetworkDeleteAt(service,index);ctx.beginPath();ctx.moveTo(x,y);ctx.lineTo(x+9,y);ctx.stroke();ctx.strokeStyle="#cf222e";ctx.beginPath();ctx.moveTo(remove.x-4,y-4);ctx.lineTo(remove.x+4,y+4);ctx.moveTo(remove.x+4,y-4);ctx.lineTo(remove.x-4,y+4);ctx.stroke();ctx.strokeStyle=c.network;ctx.fillStyle="#24292f";ctx.font=`${hovered?"600":"400"} 12px ui-monospace, monospace`;ctx.fillText(clip(ctx,network,width-55),x+34,y);});ctx.restore(); }
    drawLinks(ctx,c) {
      this.model.services.forEach(sourceService => sourceService.depends.forEach(name => {
        const link=this.dependencyGeometry(sourceService.name,name);if(!link)return;const active=this.sameLink(link,this.selectedLink)||this.sameLink(link,this.hoverLink),points=link.points,target=points[points.length-1];
        ctx.save();ctx.strokeStyle=c.accent;ctx.fillStyle=c.accent;ctx.lineWidth=active?3:1.5;if(active){ctx.shadowColor=c.accent;ctx.shadowBlur=7;}ctx.beginPath();ctx.moveTo(points[0].x,points[0].y);points.slice(1).forEach(point=>ctx.lineTo(point.x,point.y));ctx.stroke();ctx.beginPath();ctx.moveTo(target.x,target.y);ctx.lineTo(target.x-Math.cos(link.arrowAng-.45)*9,target.y-Math.sin(link.arrowAng-.45)*9);ctx.lineTo(target.x-Math.cos(link.arrowAng+.45)*9,target.y-Math.sin(link.arrowAng+.45)*9);ctx.closePath();ctx.fill();ctx.restore();
      }));
      if(this.selectedLink){const at=this.linkMidpoint(this.selectedLink);ctx.save();ctx.beginPath();ctx.arc(at.x,at.y,10,0,Math.PI*2);ctx.fillStyle="#d1242f";ctx.fill();ctx.strokeStyle="#fff";ctx.lineWidth=1.5;ctx.stroke();ctx.strokeStyle="#fff";ctx.lineWidth=1.7;ctx.beginPath();ctx.moveTo(at.x-3.5,at.y-3.5);ctx.lineTo(at.x+3.5,at.y+3.5);ctx.moveTo(at.x+3.5,at.y-3.5);ctx.lineTo(at.x-3.5,at.y+3.5);ctx.stroke();ctx.restore();}
    }
    drawService(ctx,s,c) {
      const b=this.serviceBox(s),isLinkTarget=s.name===this.linkTarget,isEnvTarget=s.name===this.envFileTarget,isTarget=isLinkTarget||isEnvTarget,linked=s.volumes.some(mount=>this.model.volumes.includes(mount.split(":")[0])),panelOpen=s.name===this.volumePanelService,portsOpen=s.name===this.portPanelService,volumeTone=s.volumes.length?c.volume:c.muted,portTone=s.ports.length?c.port:c.muted,networkTone=s.networks.length?c.network:c.muted,environmentTone=s.environment.length?c.environment:c.muted,targetTone=isEnvTarget?"#0a7f83":c.accent;ctx.save();if(isTarget){ctx.shadowColor=targetTone;ctx.shadowBlur=12;}roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=(s.name===this.selected||isTarget)?c.active:c.card;ctx.fill();ctx.strokeStyle=isTarget?targetTone:((s.name===this.selected||s.name===this.linkFrom||linked)?c.accent:c.line);ctx.lineWidth=isTarget?3:((s.name===this.selected||s.name===this.linkFrom)?2:1);ctx.stroke();ctx.restore();ctx.fillStyle=c.text;ctx.font="600 14px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(clip(ctx,s.name,b.w-48),b.x+b.w/2,b.y+(b.h-8)/2);ctx.textAlign="left";ctx.textBaseline="alphabetic";
      ctx.fillStyle=c.bg;ctx.fillRect(b.x+b.w/2-73,b.y+b.h-1,b.w>146?146:b.w,2);
      ctx.beginPath();ctx.arc(b.x,b.y+b.h/2,6,0,Math.PI*2);ctx.fillStyle=c.bg;ctx.fill();ctx.strokeStyle=c.accent;ctx.lineWidth=2;ctx.stroke();
      const mh=this.mountHandleBox(s),cx=mh.x+mh.w/2,cy=mh.y+mh.h/2;roundRect(ctx,mh.x,mh.y,mh.w,mh.h,3);ctx.fillStyle=panelOpen&&s.volumes.length?c.volumeActive:c.card;ctx.fill();ctx.strokeStyle=volumeTone;ctx.lineWidth=panelOpen?2:1.5;ctx.stroke();ctx.strokeStyle=volumeTone;ctx.lineWidth=1.1;ctx.beginPath();ctx.ellipse(cx,cy-3,5,2,0,0,Math.PI*2);ctx.moveTo(cx-5,cy-3);ctx.lineTo(cx-5,cy+3);ctx.ellipse(cx,cy+3,5,2,0,0,Math.PI);ctx.lineTo(cx+5,cy-3);ctx.stroke();ctx.textAlign="left";ctx.textBaseline="alphabetic";
      const ph=this.portHandleBox(s),px=ph.x+ph.w/2,py=ph.y+ph.h/2;roundRect(ctx,ph.x,ph.y,ph.w,ph.h,3);ctx.fillStyle=portsOpen&&s.ports.length?c.portActive:c.card;ctx.fill();ctx.strokeStyle=portTone;ctx.lineWidth=portsOpen?2:1.5;ctx.stroke();ctx.strokeStyle=portTone;ctx.lineWidth=1.2;ctx.beginPath();ctx.arc(px-4,py,2.5,0,Math.PI*2);ctx.moveTo(px-1.5,py);ctx.lineTo(px+5,py);ctx.moveTo(px+2,py-3);ctx.lineTo(px+5,py);ctx.lineTo(px+2,py+3);ctx.stroke();
      const nh=this.networkHandleBox(s),nx=nh.x+nh.w/2,ny=nh.y+nh.h/2,networksOpen=s.name===this.networkPanelService;roundRect(ctx,nh.x,nh.y,nh.w,nh.h,3);ctx.fillStyle=networksOpen&&s.networks.length?c.networkActive:c.card;ctx.fill();ctx.strokeStyle=networkTone;ctx.lineWidth=networksOpen?2:1.5;ctx.stroke();ctx.lineWidth=1.15;ctx.beginPath();ctx.moveTo(nx,ny-3);ctx.lineTo(nx-5,ny+3);ctx.moveTo(nx,ny-3);ctx.lineTo(nx+5,ny+3);ctx.moveTo(nx-5,ny+3);ctx.lineTo(nx+5,ny+3);ctx.stroke();ctx.fillStyle=networkTone;[[nx,ny-4],[nx-6,ny+4],[nx+6,ny+4]].forEach(point=>{ctx.beginPath();ctx.arc(point[0],point[1],2,0,Math.PI*2);ctx.fill();});
      const eh=this.environmentHandleBox(s),ex=eh.x+eh.w/2,ey=eh.y+eh.h/2,environmentOpen=s.name===this.environmentPanelService;roundRect(ctx,eh.x,eh.y,eh.w,eh.h,3);ctx.fillStyle=environmentOpen&&s.environment.length?c.environmentActive:c.card;ctx.fill();ctx.strokeStyle=environmentTone;ctx.lineWidth=environmentOpen?2:1.5;ctx.stroke();ctx.fillStyle=environmentTone;ctx.font="600 11px ui-monospace, monospace";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText("$",ex,ey+.5);ctx.textAlign="left";ctx.textBaseline="alphabetic";
      if(s.envFiles.includes(".env")){const eb=this.serviceEnvFileBadgeBox(s),ecx=eb.x+eb.w/2,ecy=eb.y+eb.h/2;ctx.save();roundRect(ctx,eb.x,eb.y,eb.w,eb.h,4);ctx.fillStyle="rgba(10,127,131,.14)";ctx.fill();ctx.strokeStyle="#0a7f83";ctx.lineWidth=1;ctx.stroke();ctx.fillStyle="#0a7f83";ctx.font="700 7px ui-monospace, monospace";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText("ENV",ecx,ecy+.5);ctx.restore();}
    }
    drawVolume(ctx,v,i,c) { const b=this.volumeBox(i),active=v===this.selectedVolume,count=this.volumeReferenceCount(v),cy=b.y+b.h/2;ctx.save();roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=active?c.volumeActive:c.card;ctx.fill();ctx.strokeStyle=c.volume;ctx.lineWidth=active?2.5:1;ctx.stroke();ctx.strokeStyle=c.volume;ctx.lineWidth=1.2;ctx.beginPath();ctx.ellipse(b.x+15,cy-4,6,2.5,0,0,Math.PI*2);ctx.moveTo(b.x+9,cy-4);ctx.lineTo(b.x+9,cy+4);ctx.ellipse(b.x+15,cy+4,6,2.5,0,0,Math.PI);ctx.lineTo(b.x+21,cy-4);ctx.stroke();ctx.beginPath();ctx.arc(b.x+b.w-15,cy,9,0,Math.PI*2);ctx.fillStyle=c.volume;ctx.fill();ctx.fillStyle="#fff";ctx.font="600 10px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(String(count),b.x+b.w-15,cy);ctx.fillStyle=c.text;ctx.font="12px ui-monospace";ctx.fillText(clip(ctx,v,b.w-62),b.x+b.w/2,cy);ctx.restore();ctx.textAlign="left";ctx.textBaseline="alphabetic"; }
    drawNetwork(ctx,v,i,c) { const b=this.networkBox(i),active=v===this.selectedNetwork,count=this.networkReferenceCount(v),cy=b.y+b.h/2;ctx.save();roundRect(ctx,b.x,b.y,b.w,b.h,6);ctx.fillStyle=active?c.networkActive:c.card;ctx.fill();ctx.strokeStyle=c.network;ctx.lineWidth=active?2.5:1;ctx.stroke();ctx.beginPath();ctx.arc(b.x+15,cy,7,0,Math.PI*2);ctx.stroke();ctx.beginPath();ctx.arc(b.x+15,cy,2,0,Math.PI*2);ctx.fillStyle=c.network;ctx.fill();ctx.beginPath();ctx.arc(b.x+b.w-15,cy,9,0,Math.PI*2);ctx.fill();ctx.fillStyle="#fff";ctx.font="600 10px system-ui";ctx.textAlign="center";ctx.textBaseline="middle";ctx.fillText(String(count),b.x+b.w-15,cy);ctx.fillStyle=c.text;ctx.font="12px ui-monospace";ctx.fillText(clip(ctx,v,b.w-62),b.x+b.w/2,cy);ctx.restore();ctx.textAlign="left";ctx.textBaseline="alphabetic"; }
  }

  global.ComposeCanvas = { parse, parseVolumeMount, toggleVolumeMountMode, readLayout, writeLayout, readHeaderNote, writeHeaderNote, addServiceYaml, updateServiceYaml, updateVolumeYaml, removeVolumeYaml, updateNetworkYaml, removeNetworkYaml, create: options => new Editor(options) };
})(window);
