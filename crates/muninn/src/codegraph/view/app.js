/* runar graph — the code view.
 *
 * Two altitudes of one tool. The City draws directories as blocks and symbols
 * as buildings, height from cyclomatic complexity; the Symbol view draws one
 * definition's neighbourhood. A toggle moves between them and nothing else
 * does: selecting a symbol never changes altitude, so clicking a tower shows
 * you its card in place rather than teleporting you.
 *
 * The data source is swappable on purpose. A static export inlines the payload
 * as `window.__GRAPH__`; the server leaves that null and the same code fetches
 * `/api/graph`. Nothing below knows which it got, which is what keeps the two
 * delivery modes one build instead of two. */
(function () {
  "use strict";

  var SVGNS = "http://www.w3.org/2000/svg";
  var G = null;                 // the loaded payload
  var N = Object.create(null);  // id -> node
  var OUT = Object.create(null), IN = Object.create(null);

  /* past this the city is a carpet, not a picture */
  var CITY_BUDGET = 600;

  var S = {
    view: "city",
    sel: null,
    q: "",
    colorBy: "cx",
    project: null,
    projects: [],
  };

  /* ── tiny builders ─────────────────────────────────────────────── */
  function h(tag, attrs, kids) {
    var el = document.createElement(tag);
    if (attrs) for (var k in attrs) {
      var v = attrs[k];
      if (v === null || v === undefined || v === false) continue;
      if (k === "class") el.className = v;
      else if (k === "text") el.textContent = v;
      else if (k === "html") el.innerHTML = v;
      else if (k === "style") el.setAttribute("style", v);
      else if (k.slice(0, 2) === "on") el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v);
    }
    (kids || []).forEach(function (c) {
      if (c === null || c === undefined || c === false) return;
      el.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    });
    return el;
  }
  function s(tag, attrs, kids) {
    var el = document.createElementNS(SVGNS, tag);
    if (attrs) for (var k in attrs) {
      var v = attrs[k];
      if (v === null || v === undefined || v === false) continue;
      if (k === "text") el.textContent = v;
      else if (k.slice(0, 2) === "on") el.addEventListener(k.slice(2), v);
      else el.setAttribute(k, v);
    }
    (kids || []).forEach(function (c) { if (c) el.appendChild(typeof c === "string" ? document.createTextNode(c) : c); });
    return el;
  }
  function clear(el) { while (el.firstChild) el.removeChild(el.firstChild); return el; }
  function num(n) { return (n === null || n === undefined) ? "—" : n.toLocaleString(); }

  /* ── data helpers ──────────────────────────────────────────────── */
  function loc(n) { return n.f + ":" + n.ln; }
  function modLabel(id) {
    for (var i = 0; i < G.modules.length; i++) if (G.modules[i].id === id) return G.modules[i].label;
    return id;
  }
  function badge(lb) { return h("span", { class: "lb lb-" + lb, title: lb }, [h("i", {}), lb.toLowerCase()]); }
  function tierClass(e) { return "t-" + (e.r || "none"); }
  /* confidence drives opacity, so a 0.55 guess looks like a guess */
  function tierOpacity(e) { return e.c ? 0.30 + e.c * 0.65 : 0.5; }

  function neighbours(id, dir) {
    var list = (dir === "in" ? IN[id] : OUT[id]) || [];
    var seen = Object.create(null), out = [];
    list.forEach(function (e) {
      var other = dir === "in" ? e.s : e.t;
      if (seen[other] || !N[other]) return;
      seen[other] = 1;
      out.push({ node: N[other], edge: e });
    });
    return out.sort(function (a, b) {
      return (b.edge.c || 0) - (a.edge.c || 0) || b.node.si - a.node.si;
    });
  }

  function search(q, limit) {
    q = (q || "").trim().toLowerCase();
    /* one shape always — a bare [] for a blank query threw on the next
       forEach, so a single space in the box used to kill the view */
    if (!q) return { rows: [], total: 0 };
    var hits = G.nodes.filter(function (n) {
      return n.nm.toLowerCase().indexOf(q) >= 0 || n.qn.toLowerCase().indexOf(q) >= 0;
    });
    hits.sort(function (a, b) {
      var ea = a.nm.toLowerCase() === q ? 0 : (a.nm.toLowerCase().indexOf(q) === 0 ? 1 : 2);
      var eb = b.nm.toLowerCase() === q ? 0 : (b.nm.toLowerCase().indexOf(q) === 0 ? 1 : 2);
      return ea - eb || b.si - a.si;
    });
    return { rows: hits.slice(0, limit || 12), total: hits.length };
  }

  function truncated(shown, total, unit) {
    if (shown >= total) return null;
    return h("div", { class: "trunc" }, [
      "showing ", h("b", { text: String(shown) }), " of ",
      h("b", { text: num(total) }), " " + (unit || "")]);
  }

  function symRow(n, opt) {
    opt = opt || {};
    var mets = (opt.mets || []).map(function (m) {
      return h("span", {}, [m[0] + " ", h("b", { text: String(m[1]) })]);
    });
    return h("button", {
      class: "sym-row", role: "option", "aria-selected": opt.sel ? "true" : "false",
      "data-id": n.id, type: "button", title: n.qn,
    }, [
      badge(n.lb),
      h("span", { style: "min-width:0;display:flex;flex-direction:column" }, [
        h("span", { class: "nm", text: n.nm }),
        h("span", { class: "loc", text: loc(n) })]),
      mets.length ? h("span", { class: "met" }, mets) : null,
    ]);
  }

  function pill(v, label, hot) {
    return h("span", { class: "pill" + (hot ? " hot" : "") },
      [h("span", { class: "v", text: String(v) }), h("span", { class: "l", text: label })]);
  }

  /* Call sites and callers are separate numbers because they answer different
     questions — on real graphs they differ by up to 3.6x, and a hub ranking
     built on the first alone just rewards repetitive tests. */
  function card(n) {
    return h("div", { class: "card" }, [
      h("div", { class: "hd" }, [badge(n.lb), h("span", { class: "nm", text: n.nm }),
        n.ex ? h("span", { class: "pill" }, [h("span", { class: "l", text: "exported" })]) : null]),
      h("div", { class: "qn", text: n.qn }),
      n.sig ? h("pre", { class: "sig", text: n.sig }) : null,
      h("div", { class: "mets" }, [
        pill(n.si, "call sites in"), pill(n.ci, "distinct callers"),
        pill(n.so, "calls out"), pill(n.cx, "cyclomatic", n.cx >= 20),
        pill(n.cg, "cognitive", n.cg >= 30), pill(n.pc, "params")]),
    ]);
  }

  /* ── squarified treemap ────────────────────────────────────────── */
  function treemap(items, x, y, w, hgt) {
    var total = items.reduce(function (a, i) { return a + i.value; }, 0);
    if (!total) return [];
    var out = [], rest = items.slice().sort(function (a, b) { return b.value - a.value; });
    var rx = x, ry = y, rw = w, rh = hgt, scale = (w * hgt) / total;
    function worst(row, len) {
      var sum = row.reduce(function (a, i) { return a + i.value * scale; }, 0);
      var mx = row[0].value * scale, mn = row[row.length - 1].value * scale;
      var l2 = len * len, s2 = sum * sum;
      return Math.max(l2 * mx / s2, s2 / (l2 * mn));
    }
    while (rest.length) {
      var vertical = rw >= rh, len = vertical ? rh : rw, row = [rest.shift()];
      while (rest.length && worst(row, len) >= worst(row.concat([rest[0]]), len)) row.push(rest.shift());
      var sum = row.reduce(function (a, i) { return a + i.value * scale; }, 0);
      var thick = len ? sum / len : 0, off = 0;
      row.forEach(function (item) {
        var side = thick ? (item.value * scale) / thick : 0;
        out.push(vertical ? { item: item, x: rx, y: ry + off, w: thick, h: side }
                          : { item: item, x: rx + off, y: ry, w: side, h: thick });
        off += side;
      });
      if (vertical) { rx += thick; rw -= thick; } else { ry += thick; rh -= thick; }
      if (rw < .5 || rh < .5) break;
    }
    return out;
  }

  /* ── the city ──────────────────────────────────────────────────── */
  /* Only symbols with complexity are drawn. On a real 12k-symbol project 64%
     are fields and type declarations with zero complexity — nine thousand flat
     rectangles that say nothing and bury the towers that matter. */
  function cityGeometry(o) {
    /* Ground depth is chosen against the width rather than derived from a
       single height, because the shear adds 0.3·depth to the width and 0.74·
       depth to the height. Deriving it from one number gave a 3:1 slab that
       reads as a runway; W ≈ 1.2·depth lands the projected plane near 1.4:1,
       which reads as a city block. */
    var W = o.w, DEPTH = o.depth, LIFT = o.lift, maxH = o.maxH;
    var GROUND = DEPTH;
    function proj(x, y) { return { x: x + (DEPTH - y) * 0.30, y: y * 0.74 + LIFT }; }

    /* Blocks are sized by what will actually stand on them, not by the module's
       total symbol count. Sizing by the total gives a directory full of fields
       a large empty plot while a small dense one overflows. */
    var plates = treemap(G.modules
      .map(function (m) { return { value: (o.byModule[m.id] || []).length, m: m }; })
      .filter(function (x) { return x.value > 0; }), 0, 0, W, GROUND);

    var maxCx = 1, maxSi = 1;
    G.nodes.forEach(function (n) { if (n.cx > maxCx) maxCx = n.cx; if (n.si > maxSi) maxSi = n.si; });

    var kids = [], labels = [], buildings = [], pos = Object.create(null);
    var bw = Math.max(6, W / 62);

    plates.forEach(function (c) {
      var m = c.item.m;
      var a = proj(c.x, c.y), b = proj(c.x + c.w, c.y),
          d = proj(c.x + c.w, c.y + c.h), e = proj(c.x, c.y + c.h);
      var lit = o.district && m.id === o.district;
      kids.push(s("polygon", {
        class: "plate", points: [a, b, d, e].map(function (p) { return p.x + "," + p.y; }).join(" "),
        fill: lit ? "color-mix(in srgb, var(--p) 20%, var(--surface-2))" : "var(--surface-2)",
        stroke: lit ? "var(--p)" : "var(--border)", "stroke-width": lit ? "1.6" : "1",
      }));
      if (c.w > W * 0.10 && c.h > DEPTH * 0.06) {
        var max = Math.floor((c.w - 12) / 5.4);
        labels.push(s("text", { class: "plabel", x: e.x + 6, y: e.y - 5,
          text: m.label.length > max ? m.label.slice(0, Math.max(1, max - 1)) + "…" : m.label }));
      }
      var here = (o.byModule[m.id] || []);
      var cols = Math.max(1, Math.floor(c.w / (bw + 6)));
      here.forEach(function (n, i) {
        var col = i % cols, row = Math.floor(i / cols);
        var bx = c.x + 5 + col * (bw + 6), by = c.y + 7 + row * (GROUND * 0.040);
        if (bx > c.x + c.w - bw - 2 || by > c.y + c.h - 5) return;
        buildings.push({ n: n, x: bx, y: by });
      });
    });

    buildings.sort(function (a, b) { return a.y - b.y; });  /* far to near */

    /* Square-root scale, not linear. Complexity is heavily skewed — one
       212-cyclomatic outlier against a median around 3 — and dividing by the
       max flattens 99% of the city into a tiled floor where nothing is
       comparable. sqrt keeps the tallest tower tallest while leaving the
       ordinary buildings distinguishable from each other. */
    function scale(v, max) { return max > 0 ? Math.sqrt(Math.max(0, v) / max) : 0; }

    buildings.forEach(function (bd) {
      var n = bd.n;
      var hgt = 3 + scale(n.cx, maxCx) * maxH;
      var heat = o.colorBy === "si" ? scale(n.si, maxSi) : scale(n.cx, maxCx);
      var base = proj(bd.x, bd.y), isSel = o.selId === n.id;
      var fill = "color-mix(in srgb, var(--p) " + Math.round(16 + heat * 74) + "%, var(--surface))";
      pos[n.id] = { x: base.x + bw / 2, y: base.y - hgt };
      kids.push(s("g", { class: "bldg", onclick: function () { o.onPick(n.id); } }, [
        s("title", { text: n.nm + " — cyclomatic " + n.cx + ", " + n.si + " call sites" }),
        s("polygon", { points:
          (base.x + bw) + "," + base.y + " " + (base.x + bw + 3.5) + "," + (base.y - 2.5) + " " +
          (base.x + bw + 3.5) + "," + (base.y - 2.5 - hgt) + " " + (base.x + bw) + "," + (base.y - hgt),
          fill: "color-mix(in srgb, var(--ink) 22%, " + fill + ")", opacity: ".85" }),
        s("rect", { class: "top", x: base.x, y: base.y - hgt, width: bw, height: hgt, fill: fill,
          stroke: isSel ? "var(--ink)" : "var(--border-2)", "stroke-width": isSel ? "2" : ".8", rx: "1" }),
        isSel ? s("text", { class: "plabel", x: base.x + bw / 2, y: base.y - hgt - 7,
          "text-anchor": "middle", style: "fill:var(--ink);font-weight:700", text: n.nm }) : null,
      ]));
    });

    labels.forEach(function (l) { kids.push(l); });
    var top = LIFT - maxH - 22, bottom = DEPTH * 0.74 + LIFT + 14;
    return { kids: kids, pos: pos, drawn: buildings.length,
      viewBox: "0 " + Math.round(top) + " " + Math.round(W + DEPTH * 0.30 + 10) + " " + Math.round(bottom - top) };
  }

  /* ── the egonet ────────────────────────────────────────────────── */
  function egonet(sel, o) {
    o = o || {};
    var W = o.w || 620, H = o.h || 400, SHOW = o.show || 6;
    var cx = W / 2, cy = H / 2;
    var rIn = neighbours(sel.id, "in").slice(0, SHOW), rOut = neighbours(sel.id, "out").slice(0, SHOW);
    var kids = [], boxW = Math.min(96, W * 0.17), boxH = 26, edgeX = W * 0.175;

    function arc(list, side) {
      return list.map(function (item, i) {
        var t = list.length === 1 ? .5 : i / (list.length - 1);
        return { item: item, x: side < 0 ? edgeX : W - edgeX, y: 52 + t * (H - 104) };
      });
    }
    var pIn = arc(rIn, -1), pOut = arc(rOut, 1);

    pIn.concat(pOut).forEach(function (p) {
      var e = p.item.edge, toward = p.x < cx;
      var ax = toward ? p.x + boxW / 2 : cx + 38, ay = toward ? p.y : cy;
      var bx = toward ? cx - 38 : p.x - boxW / 2, by = toward ? cy : p.y;
      kids.push(s("path", { class: "edge " + tierClass(e),
        d: "M" + ax + " " + ay + "C" + ((ax + bx) / 2) + " " + ay + " " + ((ax + bx) / 2) + " " + by + " " + bx + " " + by,
        "stroke-opacity": String(tierOpacity(e)), "stroke-width": "1.4" }));
      if (e.n > 1) kids.push(s("text", { class: "cnt", x: String((ax + bx) / 2),
        y: String((ay + by) / 2 - 4), "text-anchor": "middle", text: "×" + e.n }));
    });

    kids.push(s("g", { class: "node sel", transform: "translate(" + cx + "," + cy + ")" }, [
      s("circle", { class: "ring", r: "34", fill: "color-mix(in srgb, var(--p) 20%, var(--surface))" }),
      s("text", { class: "lbl sel", y: "4", "text-anchor": "middle", style: "font-size:12px", text: sel.nm })]));

    pIn.concat(pOut).forEach(function (p) {
      var n = p.item.node, chars = Math.floor(boxW / 6.4);
      kids.push(s("g", { class: "node", transform: "translate(" + p.x + "," + p.y + ")",
        onclick: function () { o.onPick(n.id); } }, [
        s("title", { text: n.nm + " — " + loc(n) }),
        s("rect", { class: "ring", x: String(-boxW / 2), y: String(-boxH / 2),
          width: String(boxW), height: String(boxH), rx: "6" }),
        s("text", { class: "lbl", y: "4", "text-anchor": "middle",
          text: n.nm.length > chars ? n.nm.slice(0, chars - 1) + "…" : n.nm })]));
    });

    kids.push(s("text", { class: "cnt", x: String(edgeX), y: "26", "text-anchor": "middle",
      style: "font-weight:700;letter-spacing:.08em", text: "CALLERS" }));
    kids.push(s("text", { class: "cnt", x: String(W - edgeX), y: "26", "text-anchor": "middle",
      style: "font-weight:700;letter-spacing:.08em", text: "CALLS" }));

    return {
      svg: s("svg", { class: "gsvg", viewBox: "0 0 " + W + " " + H, role: "img",
        "aria-label": "Neighbourhood of " + sel.nm }, kids),
      shown: rIn.length + rOut.length,
      /* both sides in DISTINCT neighbours, matching what is drawn — measuring
         the callee side against call sites reports hidden neighbours that do
         not exist */
      hiddenIn: sel.ci - rIn.length,
      hiddenOut: sel.co - rOut.length,
      total: sel.ci + sel.co,
    };
  }

  /* ── city view ─────────────────────────────────────────────────── */
  function cityView() {
    var sel = S.sel ? N[S.sel] : null;
    /* Only symbols with complexity get a building — on a real 12k-symbol
       project 64% are fields and type declarations with none, and nine
       thousand flat rectangles bury the towers that matter.
       Then a budget: past roughly this many the city stops being a picture of
       anything and becomes a carpet, so the most complex win and the rest are
       reported rather than silently dropped. */
    var drawable = G.nodes.filter(function (n) { return n.cx > 0; })
                          .sort(function (a, b) { return b.cx - a.cx || a.nm.localeCompare(b.nm); });
    var eligible = drawable.length;
    var shown = drawable.slice(0, CITY_BUDGET);
    var byModule = Object.create(null);
    shown.forEach(function (n) { (byModule[n.m] || (byModule[n.m] = [])).push(n); });

    var city = cityGeometry({
      w: 660, depth: 470, lift: 175, maxH: 140, colorBy: S.colorBy, selId: S.sel, byModule: byModule,
      district: sel ? sel.m : null, onPick: pick,
    });
    var kids = city.kids, arcs = 0;

    if (sel && city.pos[sel.id]) {
      var a = city.pos[sel.id];
      G.edges.forEach(function (ed) {
        var other = ed.s === sel.id ? ed.t : (ed.t === sel.id ? ed.s : null);
        if (other === null || !city.pos[other]) return;
        var b = city.pos[other];
        var mx = (a.x + b.x) / 2, my = Math.min(a.y, b.y) - 46;
        kids.push(s("path", { class: "edge hi " + tierClass(ed),
          d: "M" + a.x + " " + a.y + "Q" + mx + " " + my + " " + b.x + " " + b.y,
          "stroke-opacity": String(tierOpacity(ed)), "stroke-width": "1.3" }));
        arcs++;
      });
    }

    var byCx = S.colorBy !== "si";
    var list = (byCx ? G.ranks.complexity : G.ranks.callSites).slice(0, 8);
    var listMax = list[0] ? (byCx ? list[0].cx : list[0].si) : 1;

    var main = h("div", { class: "cityv-main" }, [
      h("div", { class: "cityv-bar" }, [
        h("span", { class: "lbl", text: "height = cyclomatic complexity · colour =" }),
        h("div", { class: "modes", role: "group", "aria-label": "Colour by" },
          [["cx", "complexity"], ["si", "call sites"]].map(function (m) {
            return h("button", { class: "btn", type: "button", text: m[1],
              "aria-pressed": String(S.colorBy === m[0]),
              onclick: function () { S.colorBy = m[0]; render(); } });
          })),
        h("span", { class: "lbl spacer", text: num(city.drawn) + " of " + num(eligible) +
          " symbols with complexity" + (eligible > city.drawn ? " · tallest first" : "") }),
      ]),
      h("div", { class: "cityv-canvas" }, [
        s("svg", { class: "gsvg", viewBox: city.viewBox, role: "img", "aria-label": "Code city" }, kids)]),
      h("div", { class: "cityv-legend" }, [
        h("span", { class: "ramp" }, ["low ",
          h("i", { style: "background:color-mix(in srgb,var(--p) 16%,var(--surface))" }),
          h("i", { style: "background:color-mix(in srgb,var(--p) 44%,var(--surface))" }),
          h("i", { style: "background:color-mix(in srgb,var(--p) 68%,var(--surface))" }),
          h("i", { style: "background:color-mix(in srgb,var(--p) 90%,var(--surface))" }), " high"]),
        h("span", { text: "blocks are directories · one building is one symbol with complexity" }),
        arcs ? h("span", { text: "arcs are this symbol's calls · dash = weaker tier" }) : null,
        (sel && !arcs) ? h("span", { text: "this symbol has no building drawn at this scale" }) : null,
      ]),
    ]);

    var side = h("div", { class: "cityv-side" });
    if (sel) {
      side.appendChild(card(sel));
      side.appendChild(h("div", { class: "side" }, [
        h("button", { class: "btn", type: "button", style: "width:100%;justify-content:center",
          text: "Open in Symbol view →", onclick: function () { S.view = "symbol"; render(); } })]));
    } else {
      side.appendChild(h("div", { class: "empty" }, [
        h("p", { class: "side-h", text: "Altitude" }),
        h("p", { text: "Height is cyclomatic complexity, so the thing most likely to hurt you " +
          "is also the tallest thing on screen. Click a tower — or search, if you already " +
          "know the name." })]));
    }

    side.appendChild(h("div", { class: "cityv-rank" }, [
      h("h4", { text: byCx ? "Most complex in the whole graph" : "Most called in the whole graph" }),
      /* the ranking is over the whole graph but only symbols with complexity
         are drawn, so many rows have nothing to select — disabled and marked
         beats a live-looking button that silently does nothing */
      h("div", {}, list.map(function (r) {
        var v = byCx ? r.cx : r.si, here = !!N[r.id];
        return h("button", { class: "trow" + (here ? "" : " off"), type: "button", disabled: !here,
          title: here ? r.qn : "Ranked from the full graph; not drawn at this scale",
          onclick: function () { if (here) pick(r.id); } }, [
          h("i", { class: "sk", style: "height:" + Math.round(6 + v / listMax * 20) + "px" }),
          h("span", { class: "nm", text: r.nm }),
          here ? null : h("span", { class: "nd", text: "not drawn" }),
          h("span", { class: "n", text: String(v) })]);
      })),
    ]));

    return h("div", { class: "cityv" }, [main, side]);
  }

  /* ── symbol view ───────────────────────────────────────────────── */
  function symbolView() {
    var sel = S.sel ? N[S.sel] : null;
    var hubs = G.ranks.callSites.filter(function (r) { return N[r.id]; }).slice(0, 10);

    var rows = h("div", { class: "symv-rows", role: "listbox", "aria-label": "Symbols" });
    var colHead = h("div", { class: "symv-colh" });
    var truncEl = h("div");

    /* only these three repaint on input: rebuilding the column would destroy
       and recreate the field, taking the caret and any IME composition */
    function paintList() {
      var q = (S.q || "").trim();
      var res = q ? search(q, 12) : { rows: [], total: 0 };
      var listRows = q ? res.rows : hubs.map(function (r) { return N[r.id]; }).filter(Boolean);

      clear(colHead);
      colHead.appendChild(h("span", { class: "side-h", text: q ? "Matches" : "Most called" }));
      colHead.appendChild(h("span", { style: "font-size:var(--t-xs);color:var(--muted)",
        text: q ? num(res.total) + " found" : "across " + num(G.meta.symbols) }));

      clear(rows);
      if (q && !res.total) rows.appendChild(h("p",
        { style: "padding:14px 10px;color:var(--muted);font-size:var(--t-sm)",
          text: "No symbol matches “" + q + "”." }));
      listRows.forEach(function (n) {
        rows.appendChild(symRow(n, { sel: S.sel === n.id, mets: [["in", n.si]] }));
      });

      clear(truncEl);
      if (q) { var t = truncated(res.rows.length, res.total, "matches"); if (t) truncEl.appendChild(t); }
    }
    rows.addEventListener("click", function (e) {
      var b = e.target.closest("[data-id]");
      if (b) pick(+b.dataset.id);
    });
    paintList();

    var stage = h("div", { class: "symv-stage" }, [
      h("div", { class: "symv-stageh" }, [
        sel ? badge(sel.lb) : null,
        h("span", { class: "path", text: sel ? sel.qn : "nothing selected" })])]);
    var canvas = h("div", { class: "symv-canvas" });
    stage.appendChild(canvas);

    if (!sel) {
      canvas.appendChild(h("div", { class: "symv-empty" }, [
        h("div", { class: "big", text: "Search for a symbol." }),
        h("p", { html: "There are <b>" + num(G.meta.symbols) + "</b> symbols here. Drawing them " +
          "all would tell you nothing, so nothing is drawn until you ask. If you have no name " +
          "in mind, go up to the City — or start from the most-called:" }),
        h("div", { class: "hubs" }, hubs.slice(0, 6).map(function (r) {
          return h("button", { class: "hub", type: "button", onclick: function () { pick(r.id); } },
            [r.nm, h("b", { text: r.si + " in" })]);
        }))]));
    } else {
      var ego = egonet(sel, { w: 620, h: 400, show: 6, onPick: pick });
      canvas.appendChild(ego.svg);
      if (ego.hiddenIn > 0 || ego.hiddenOut > 0) {
        stage.appendChild(h("div", { class: "trunc", html:
          "drawing <b>" + ego.shown + "</b> of <b>" + ego.total + "</b> neighbours — " +
          (ego.hiddenIn > 0 ? ego.hiddenIn + " more callers" : "") +
          (ego.hiddenIn > 0 && ego.hiddenOut > 0 ? ", " : "") +
          (ego.hiddenOut > 0 ? ego.hiddenOut + " more callees" : "") + " one click away" }));
      }
    }

    var locus = h("div", { class: "symv-locus" });
    if (sel) {
      locus.appendChild(card(sel));
      var nb = neighbours(sel.id, "in").slice(0, 6);
      if (nb.length) locus.appendChild(h("div", { class: "side" }, [
        h("h4", { text: "Called from" }),
        h("div", {}, nb.map(function (x) {
          return h("div", { class: "srow" }, [
            h("span", { class: "nm", text: x.node.nm }),
            h("span", { style: "font-size:var(--t-xs);color:var(--muted)",
              text: x.edge.r || x.edge.ty.toLowerCase() })]);
        }))]));
      locus.appendChild(h("div", { class: "side" }, [
        h("button", { class: "btn", type: "button", style: "width:100%;justify-content:center",
          text: "↑ Show in the City", onclick: function () { S.view = "city"; render(); } })]));
    } else {
      locus.appendChild(h("div", { class: "empty" }, [
        h("p", { class: "side-h", text: "No selection" }),
        h("p", { text: "Definition site, signature and metrics appear here once a symbol is selected." })]));
    }

    return { el: h("div", { class: "symv" }, [
      h("div", { class: "symv-list" }, [colHead, rows, truncEl]), stage, locus]),
      paintList: paintList };
  }

  /* ── shell ─────────────────────────────────────────────────────── */
  var listPainter = null;

  function pick(id) { S.sel = id; render(); }   /* never changes altitude */

  function icon(paths) {
    return s("svg", { viewBox: "0 0 24 24", fill: "none", stroke: "currentColor",
      "stroke-width": "2", "stroke-linecap": "round", "stroke-linejoin": "round" },
      paths.map(function (d) { return s("path", { d: d }); }));
  }

  function bar() {
    var sel = S.sel ? N[S.sel] : null;
    var hits = h("div", { class: "hits", hidden: true });

    function paintHits() {
      clear(hits);
      var q = (S.q || "").trim();
      if (!q) { hits.hidden = true; return; }
      hits.hidden = false;
      var res = search(q, 8);
      if (!res.total) {
        hits.appendChild(h("p", { style: "padding:9px;color:var(--muted);font-size:var(--t-sm)",
          text: "No symbol matches “" + q + "”." }));
        return;
      }
      res.rows.forEach(function (n) { hits.appendChild(symRow(n, { sel: n.id === S.sel, mets: [["cx", n.cx]] })); });
      var t = truncated(res.rows.length, res.total, "matches");
      if (t) hits.appendChild(t);
    }
    hits.addEventListener("click", function (e) {
      var b = e.target.closest("[data-id]");
      if (b) { S.q = ""; pick(+b.dataset.id); }
    });

    var input = h("input", { class: "inp", type: "search", value: S.q, placeholder: "Search symbols…",
      "aria-label": "Search symbols",
      oninput: function (e) { S.q = e.target.value; paintHits(); if (listPainter) listPainter(); } });

    paintHits();

    return h("div", { class: "bar" }, [
      h("div", { class: "brand" }, [h("b", { text: "runar graph" })]),
      h("button", { class: "proj", id: "projBtn", "aria-haspopup": "true", "aria-expanded": "false",
        title: "Switch project", onclick: openProjects }, [
        h("span", { class: "dot" }), h("span", { class: "nm", text: G.meta.project }),
        h("span", { class: "mt", text: num(G.meta.symbols) + " symbols" })]),
      h("div", { class: "alt", role: "group", "aria-label": "Altitude" }, [
        h("button", { type: "button", "aria-pressed": String(S.view === "city"),
          onclick: function () { S.view = "city"; render(); } },
          [icon(["M3 21h18M5 21V10l4-3v14M13 21V6l6 4v11"]), "City"]),
        h("button", { type: "button", "aria-pressed": String(S.view === "symbol"),
          disabled: !sel, title: sel ? "" : "Pick a symbol first",
          onclick: function () { S.view = "symbol"; render(); } },
          [icon(["M12 3v6M12 15v6M3 12h6M15 12h6"]), "Symbol"]),
      ]),
      h("div", { class: "search" }, [input, hits]),
      sel ? h("span", { class: "pin", text: "● " + sel.nm }) : null,
      h("button", { class: "btn icon spacer", "aria-label": "Light / dark theme", title: "Light / dark",
        onclick: toggleTheme }, [icon(["M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"])]),
    ]);
  }

  function toggleTheme() {
    var root = document.documentElement;
    var cur = root.getAttribute("data-theme");
    if (!cur) cur = matchMedia("(prefers-color-scheme:dark)").matches ? "dark" : "light";
    root.setAttribute("data-theme", cur === "dark" ? "light" : "dark");
  }

  /* ── project switcher ──────────────────────────────────────────── */
  function openProjects() {
    var stage = document.getElementById("stage");
    var panel = h("div", { class: "projpanel", role: "dialog", "aria-label": "Projects" });
    var body = h("div", { class: "body" });
    panel.appendChild(h("div", { class: "ph" }, [
      h("h2", { text: "Projects" }),
      h("span", { class: "sum", text: S.projects.length + " with a code graph" }),
      h("button", { class: "btn", type: "button", text: "Close", style: "margin-left:auto",
        onclick: function () { panel.remove(); } })]));
    body.appendChild(h("div", { class: "projgrid" }, S.projects.map(function (p) {
      return h("button", { class: "projrow", type: "button",
        "aria-current": String(p.project === G.meta.project),
        onclick: function () { panel.remove(); loadProject(p.project); } }, [
        h("span", { class: "t" }, [h("span", { class: "nm", text: p.project })]),
        h("span", { class: "root", text: p.root }),
        h("span", { class: "mets" }, [
          h("span", { html: "<b>" + num(p.symbols) + "</b> symbols" }),
          h("span", { html: "<b>" + num(p.edges) + "</b> edges" }),
          h("span", { html: "<b>" + num(p.files) + "</b> files" }),
          h("span", { text: "indexed " + (p.indexedAt || "").slice(0, 10) })])]);
    })));
    if (SOURCE.mode === "static") {
      body.appendChild(h("p", { class: "note", html:
        "This is a static export, so only <b>" + G.meta.project + "</b> travels with the file. " +
        "Run <code>runar graph serve</code> to switch between projects live." }));
    }
    panel.appendChild(body);
    stage.appendChild(panel);
  }

  /* ── data source: inlined by the export, fetched by the server ─── */
  var SOURCE = window.__GRAPH__
    ? { mode: "static",
        graph: function () { return Promise.resolve(window.__GRAPH__); },
        projects: function () { return Promise.resolve(window.__GRAPH__.projects || []); } }
    : { mode: "live",
        graph: function (p) {
          return fetch("api/graph" + (p ? "?project=" + encodeURIComponent(p) : ""))
            .then(function (r) { if (!r.ok) throw new Error("HTTP " + r.status); return r.json(); });
        },
        projects: function () {
          return fetch("api/projects").then(function (r) { return r.json(); });
        } };

  function index(payload) {
    G = payload;
    N = Object.create(null); OUT = Object.create(null); IN = Object.create(null);
    G.nodes.forEach(function (n) { N[n.id] = n; });
    G.edges.forEach(function (e) {
      (OUT[e.s] || (OUT[e.s] = [])).push(e);
      (IN[e.t] || (IN[e.t] = [])).push(e);
    });
  }

  function render() {
    var stage = document.getElementById("stage");
    var root = document.getElementById("app");
    clear(root);
    root.appendChild(bar());
    var st = h("div", { class: "stage", id: "stage" });
    listPainter = null;
    if (S.view === "symbol") {
      var v = symbolView();
      listPainter = v.paintList;
      st.appendChild(v.el);
    } else {
      st.appendChild(cityView());
    }
    root.appendChild(st);
  }

  function loadProject(name) {
    var root = document.getElementById("app");
    clear(root);
    root.appendChild(h("div", { class: "boot", text: "Loading " + name + "…" }));
    SOURCE.graph(name).then(function (payload) {
      index(payload);
      S.sel = null; S.q = ""; S.view = "city"; S.project = payload.meta.project;
      render();
    }).catch(fail);
  }

  function fail(e) {
    var root = document.getElementById("app");
    clear(root);
    root.appendChild(h("div", { class: "boot" }, [
      h("div", { class: "err", html: "<b>Could not load the graph.</b><br>" + String(e.message || e) })]));
  }

  addEventListener("keydown", function (e) {
    var t = e.target;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA")) return;
    if (e.metaKey || e.ctrlKey || e.altKey) return;
    if (e.key === "Escape") {
      var p = document.querySelector(".projpanel");
      if (p) { p.remove(); return; }
    }
    if (e.key === "c") { S.view = "city"; render(); }
    if (e.key === "s" && S.sel) { S.view = "symbol"; render(); }
  });

  Promise.all([SOURCE.graph(), SOURCE.projects()])
    .then(function (r) {
      index(r[0]);
      S.projects = r[1] || [];
      S.project = r[0].meta.project;
      render();
    })
    .catch(fail);
})();
